# Miscellaneous tool compressors — pip, grep/rg, find/fd, ls, curl
# Ported from lean-ctx

# ---------------------------------------------------------------------------
# pip
# ---------------------------------------------------------------------------

(defn- pip-compress-install [output]
  (var packages @[])
  (var time "")
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (if-let [c (match "Successfully installed ([^\n]+)" t)]
      (set packages (filter (fn [p] (not (empty? p))) (string/split " " (string/trim (in c 0))))))
    (if-let [c (match "in ([0-9]+\\.?[0-9]*\\s*[ms]+)" t)]
      (set time (in c 0))))
  (if (empty? packages)
    (string/join (take 5 (string/split "\n" output)) "\n")
    (string "installed " (length packages) " packages: "
            (string/join (take 10 packages) ", ")
            (if (> (length packages) 10) (string " ...+ " (- (length packages) 10) " more") "")
            (if (not (empty? time)) (string " (" time ")") ""))))

(defn pip-compress [command output]
  (if (not (or (string/find "pip " command) (string/find "pip3 " command))) (break nil))
  (pip-compress-install output))

# ---------------------------------------------------------------------------
# grep / ripgrep
# ---------------------------------------------------------------------------

(defn- compress-grep-output [output]
  (def lines (string/split "\n" output))
  (if (<= (length lines) 30) (break output))
  # Head+tail: grep emits matches in file order, so the LAST matches
  # are as load-bearing as the first. Head-only truncation hid the
  # tail entirely (the regression this refactor fixes).
  (truncate-lines lines 18 8 "matching lines"))

(defn grep-compress [command output]
  (if (not (or (string/find "grep " command) (string/has-prefix? "grep" command)
               (string/find "rg " command)))
    (break nil))
  (compress-grep-output output))

# ---------------------------------------------------------------------------
# find / fd
# ---------------------------------------------------------------------------

(defn- compress-find-output [output]
  (def lines (nonblank-lines output))
  (if (<= (length lines) 20) (break (string/join lines "\n")))
  # Show actual paths head+tail. The previous version guessed
  # file-vs-dir from "does the basename contain a dot" — which
  # miscounts (Makefile/README → "dir", .git → "file"). `find`
  # output is just paths; there's no reliable file/dir signal in the
  # string, so we report the honest total instead of a wrong split.
  (truncate-lines lines 12 6 "paths"))

(defn find-compress [command output]
  (if (not (or (string/find "find " command) (string/find "fd " command))) (break nil))
  (compress-find-output output))

# ---------------------------------------------------------------------------
# ls
# ---------------------------------------------------------------------------

(defn- compress-ls-output [output]
  (def lines (nonblank-lines output))
  (if (<= (length lines) 30) (break (string/join lines "\n")))
  # Show actual entry names head+tail rather than collapsing to a bare
  # count — the model usually re-runs `ls` to learn WHICH files exist,
  # so a names-less "N entries" was nearly useless. (The old file/dir
  # split only worked for `ls -l`; dropped as unreliable.)
  (truncate-lines lines 18 6 "entries"))

(defn ls-compress [command output]
  (if (not (or (string/has-prefix? "ls" command) (string/find " ls " command) (string/find "/ls" command))) (break nil))
  (compress-ls-output output))

# ---------------------------------------------------------------------------
# curl
# ---------------------------------------------------------------------------

(defn- compress-curl-output [output]
  (def trimmed (string/trim output))
  (when (empty? trimmed) (break "ok"))
  # Already head+tail; route through the shared helper so the marker
  # matches every other compressor.
  (truncate-lines (string/split "\n" trimmed) 10 10 "lines"))

(defn curl-compress [command output]
  (if (not (or (string/find "curl " command) (string/has-prefix? "curl" command)))
    (break nil))
  (compress-curl-output output))

nil
