# Vendored from janet-lang/spork (MIT license)
# https://github.com/janet-lang/spork/blob/master/spork/regex.janet
#
# Copyright (c) 2022 Calvin Rose and contributors
#
# Permission is hereby granted, free of charge, to any person obtaining a copy
# of this software and associated documentation files (the "Software"), to deal
# in the Software without restriction, including without limitation the rights
# to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
# copies of the Software, and to permit persons to whom the Software is
# furnished to do so, subject to the following conditions:
#
# The above copyright notice and this permission notice shall be included in all
# copies or substantial portions of the Software.
#
# THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
# IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
# FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
# AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
# LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
# OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
# SOFTWARE.

###
### regex.janet
###
### A module for compiling a subset of regexes to Janet PEGs.
### All regex are considered to be anchored, and performance is
### not going to be competitive with a native regex engine.
###
###
### Supported regex features:
###   - single bytes
###   - escape characters
###   - +, *, ?, .
###   - Repetitions, e.g. a{1}, a{1,3}. Repetitions are eagerly evaluated.
###   - Ranges, e.g. [A-Za-z]
###   - Character classes, inverted character classes, e.g. [abc], [^abc]
###   - Alternation (choice), except alternation is ordered, as in pegs - e.g. a|b|c
###   - Captures using parentheses, e.g. (abc)
###   - Non-capture groups, e.g. (?:abc)
###
###
### Features found in other regex may never be added - for more complex usage,
### use Janet's native PEG library.
###

(defn- postfix-modify
  "Apply regex postfix operators to a pattern."
  [cc suffix &opt suf2]
  (case suffix
    "?" ['? cc]
    "*" ['any cc]
    "+" ['some cc]
    (if (empty? suffix)
      cc
      (if suf2
        ['between (scan-number suffix) (scan-number suf2) cc]
        ['repeat (scan-number suffix) cc]))))

(defn- make-sequence
  "Combine a series of patterns into a sequence combinator, but
  merge string literals together."
  [& ccs]
  (let [res @['*]
        buf @""]
    (each cc ccs
      (if (string? cc)
        (buffer/push-string buf cc)
        (do
          (unless (empty? buf)
            (array/push res (string buf)))
          (array/push res cc)
          (buffer/clear buf))))
    (unless (empty? buf) (array/push res (string buf)))
    (if (= 2 (length res)) (in res 1) (tuple/slice res))))

(defn- make-choice
  "Combine multiple patterns with the choice combinator, or
  return the first argument if there is only one argument.
  Will also reduce multiple choices into a single choice operator."
  [l &opt r]
  (if r
    (if (and (tuple? r) (= 'choice (first r)))
      ['choice l ;(tuple/slice r 1)]
      ['choice l r])
    l))

(def peg
  "Peg used to generate peg source code from a regular expression string."
  (peg/compile
    ~{# Custom character classes (bracketed characters)
      # Compiled to a single (range ...) peg combinator
      :hex (range "09" "af" "AF")
      :escapedchar (+ (/ `\n` "\n")
                      (/ `\t` "\t")
                      (/ `\e` "\e")
                      (/ `\v` "\v")
                      (/ `\r` "\r")
                      (/ `\f` "\f")
                      (/ `\0` "\0")
                      (/ `\z` "\z")
                      (/ (* `\x` '(* :hex :hex)) ,|(string/from-bytes (scan-number (string "0x" $))))
                      (* `\` '1))
      :namedclass1 (+ (/ `\s` "  \t\n")
                      (/ `\d` "09")
                      (/ `\a` "AZaz")
                      (/ `\w` "AZaz09__")
                      (/ `\S` "\0\x08\x0e\x1f\x21\xff")
                      (/ `\D` "\0\x2f\x3a\xff")
                      (/ `\A` "\0\x40\x5b\x60\x7b\xff")
                      (/ `\W` "\0\x2f\x3a\x40\x5b\x5e\x60\x60\x7b\xff"))
      :singbyte (+ :escapedchar (if-not (set "-]") '1))
      :singchar (/ :singbyte ,|(string $ $))
      :charspan (* :singbyte "-" :singbyte)
      :make-range (/ (accumulate (any (+ :namedclass1 :charspan :singchar)))
                     ,|['range ;(partition 2 $)])
      :brack (* "[" :make-range "]")
      :invbrack (/ (* "[^" :make-range "]") ,|['if-not $ 1])

      # Other single characters
      :escapedchar2 (+ (/ `\s` (set "\n\t\r\v\f "))
                       (/ `\d` (range "09"))
                       (/ `\a` (range "AZ" "az"))
                       (/ `\w` (range "AZ" "az" "09" "__"))
                       (/ `\S` (range "\0\x08" "\x0e\x1f" "\x21\xff"))
                       (/ `\D` (range "\0\x2f" "\x3a\xff"))
                       (/ `\A` (range "\0\x40" "\x5b\x60" "\x7b\xff"))
                       (/ `\W` (range "\0\x2f" "\x3a\x40" "\x5b\x5e" "\x60\x60" "\x7b\xff"))
                       :escapedchar)
      :normalchars (if-not (set `[.+*?()|`) '1)
      :dot (/ "." 1)
      :cc (+ :dot :invbrack :brack :escapedchar2 :normalchars)

      # Postfix modifier
      :postfix (+ '"?" '"*" '"+" (* "{" ':d+ (? (* "," ':d+)) "}") '"")

      # Character modifiers
      :cc-with-mod (/ (* :cc :postfix) ,postfix-modify)

      # Single alternation is a sequence of character classes.
      :span (/ (some :cc-with-mod) ,make-sequence)

      # Captures and groupings
      :grouping (* "(?:" :node ")")
      :capture (/ (* "(" :node ")") ,|['capture $])
      :node1 (/ (* (+ :grouping :capture :span) :postfix) ,postfix-modify)
      :node-span (/ (some :node1) ,make-sequence)
      :node (/ (* :node-span (? (* "|" :node))) ,make-choice)

      :main (* :node (+ -1 (error "")))}))

(defn source
  "Compile a subset of regex to PEG source code."
  [pattern]
  (def [res] (peg/match peg pattern))
  res)

(defn compile :shadow
  "Compile a subset of regex to a PEG if pattern is a string.
  If pattern is a PEG, will return the PEG as is."
  [pattern]
  (if (string? pattern)
    (peg/compile (source pattern))
    pattern))

(defn match :shadow
  "Similar to peg/match, but for regexes."
  [reg text &opt start]
  (peg/match (compile reg) text (or start 0)))

(defn find :shadow
  "Similar to peg/find, but for regexes."
  [reg text &opt start]
  (peg/find (compile reg) text (or start 0)))

(defn find-all
  "Similar to peg/find-all, but for regexes."
  [reg text &opt start]
  (peg/find-all (compile reg) text (or start 0)))

(defn replace
  "Similar to peg/replace, but for regexes."
  [reg rep text &opt start]
  (peg/replace (compile reg) rep text (or start 0)))

(defn replace-all
  "Similar to peg/replace-all, but for regexes."
  [reg rep text &opt start]
  (peg/replace-all (compile reg) rep text (or start 0)))
