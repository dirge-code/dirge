# Delimiter repair for LLM-authored Janet.
#
# Models routinely emit Lisp with unbalanced delimiters, and a notebook
# cell is the shape that hits it hardest — the code is written straight
# into a tool argument with no editor to close forms. Unmatched openers
# are closed at the end so the first attempt parses.
#
# This is Janet-flavoured and is NOT the nrepl plugin's Clojure version:
# in Janet `#` starts a line comment and `;` is the SPLICE operator, so
# treating `;` as a comment (as the Clojure repair does) would silently
# delete the rest of any line containing a splice. Janet also has long
# strings delimited by backticks, which ignore backslash escapes.
#
# Byte values throughout — the hot path stays free of string conversions.

(def- open-paren  40)  # (
(def- close-paren 41)  # )
(def- open-brack  91)  # [
(def- close-brack 93)  # ]
(def- open-brace  123) # {
(def- close-brace 125) # }
(def- hash        35)  # #  — Janet line comment
(def- doublequote 34)  # "
(def- backtick    96)  # `  — Janet long string
(def- backslash   92)  # \
(def- newline     10)  # \n

(def- closer-for @{open-paren close-paren
                   open-brack close-brack
                   open-brace close-brace})

(defn repair-delimiters
  "Append any missing closing delimiters to `code`. Returns the repaired
  string, or the original when it was already balanced."
  [code]
  (var stack @[])
  (var i 0)
  (def len (length code))
  (while (< i len)
    (def ch (get code i))
    (cond
      # Line comment -> skip to end of line.
      (= ch hash)
      (do
        (while (and (< i len) (not= (get code i) newline))
          (set i (+ i 1)))
        (set i (+ i 1)))

      # Long string: runs to the next matching backtick run. No escapes
      # inside, so scan straight for the terminator.
      (= ch backtick)
      (do
        (set i (+ i 1))
        (while (and (< i len) (not= (get code i) backtick))
          (set i (+ i 1)))
        (set i (+ i 1)))

      # Regular string: consume to the closing quote, honouring escapes.
      (= ch doublequote)
      (do
        (set i (+ i 1))
        (var done false)
        (while (and (< i len) (not done))
          (def c (get code i))
          (cond
            (= c backslash) (set i (+ i 2))
            (= c doublequote) (do (set done true) (set i (+ i 1)))
            (set i (+ i 1)))))

      (or (= ch open-paren) (= ch open-brack) (= ch open-brace))
      (do (array/push stack ch) (set i (+ i 1)))

      (or (= ch close-paren) (= ch close-brack) (= ch close-brace))
      (do
        (when (and (> (length stack) 0)
                   (= (get closer-for (last stack)) ch))
          (array/pop stack))
        (set i (+ i 1)))

      (set i (+ i 1))))
  (if (empty? stack)
    code
    (do
      (def out (buffer code))
      (loop [j :down-to [(- (length stack) 1) 0]]
        (buffer/push-byte out (get closer-for (get stack j))))
      (string out))))
