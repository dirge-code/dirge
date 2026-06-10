# Shared output-truncation helpers for the compression plugin.
# Loaded after 00-regex.janet, before the per-tool compressors
# (alphabetical sort), so every compressor can call these.
#
# Principle: NEVER silently drop the tail of output. When a list is
# too long to keep whole, show a HEAD and a TAIL with an explicit,
# unambiguous marker stating exactly how many lines are hidden — and
# that what's shown is a head+tail sample, not a relevance ranking.
# Errors, summaries, and totals usually live at the END of command
# output, so head-only truncation is the dangerous kind.

(defn truncate-lines
  ``Keep the first `head-n` and last `tail-n` of `lines` (an indexed
  collection of strings) when the list exceeds head-n+tail-n, with a
  clear marker in between. `label` names the unit for the hidden count
  (e.g. "matches", "results", "lines"). Returns a string. When the
  list already fits, returns every line joined, unchanged.``
  [lines head-n tail-n label]
  (def n (length lines))
  (if (<= n (+ head-n tail-n))
    (string/join lines "\n")
    (let [head (take head-n lines)
          tail (take tail-n (drop (- n tail-n) lines))
          hidden (- n head-n tail-n)]
      (string
        (string/join head "\n")
        "\n… [" hidden " of " n " " label
        " hidden — showing first " head-n " + last " tail-n "] …\n"
        (string/join tail "\n")))))

(defn nonblank-lines
  "Split `text` on newlines and drop blank/whitespace-only lines."
  [text]
  (filter (fn [l] (not (empty? (string/trim l))))
          (string/split "\n" text)))

nil
