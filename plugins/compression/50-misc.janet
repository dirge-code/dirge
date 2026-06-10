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
  (def match-count (length (filter (fn [l] (not (empty? (string/trim l)))) lines)))
  (let [sample (take 20 lines)]
    (string (string/join sample "\n")
            "\n... (" match-count " matches, " (- (length lines) 20) " lines omitted)")))

(defn grep-compress [command output]
  (if (not (or (string/find "grep " command) (string/has-prefix? "grep" command)
               (string/find "rg " command)))
    (break nil))
  (compress-grep-output output))

# ---------------------------------------------------------------------------
# find / fd
# ---------------------------------------------------------------------------

(defn- compress-find-output [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 20) (break (string/join lines "\n")))
  (def dirs (filter (fn [l] (not (string/find "." (last (string/split "/" l))))) lines))
  (def files (- (length lines) (length dirs)))
  (def sample (take 15 lines))
  (string (string/join sample "\n")
          "\n... (" (length lines) " results: " files " files, " (length dirs) " dirs)"))

(defn find-compress [command output]
  (if (not (or (string/find "find " command) (string/find "fd " command))) (break nil))
  (compress-find-output output))

# ---------------------------------------------------------------------------
# ls
# ---------------------------------------------------------------------------

(defn- compress-ls-output [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 30) (break (string/join lines "\n")))
  (def dirs (filter (fn [l]
                      (let [t (string/trim l)]
                        (or (string/has-prefix? "d" t) (string/has-prefix? "total" t))))
                    lines))
  (def files (- (length lines) (length dirs)))
  (string (length lines) " entries (" files " files, " (length dirs) " dirs)"))

(defn ls-compress [command output]
  (if (not (or (string/has-prefix? "ls" command) (string/find " ls " command) (string/find "/ls" command))) (break nil))
  (compress-ls-output output))

# ---------------------------------------------------------------------------
# curl
# ---------------------------------------------------------------------------

(defn- compress-curl-output [output]
  (def trimmed (string/trim output))
  (when (empty? trimmed) (break "ok"))
  (def lines (string/split "\n" trimmed))
  (if (<= (length lines) 20) (break trimmed))
  (def head (take 10 lines))
  (def tail (take 10 (drop (- (length lines) 10) lines)))
  (string (string/join head "\n")
          "\n... (" (- (length lines) 20) " lines omitted)\n"
          (string/join tail "\n")))

(defn curl-compress [command output]
  (if (not (or (string/find "curl " command) (string/has-prefix? "curl" command)))
    (break nil))
  (compress-curl-output output))

nil
