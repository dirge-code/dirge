# Compression plugin — init.janet
# Ported from lean-ctx
#
# Loaded AFTER 00-regex.janet (alphabetical sort). The regex functions
# (find, find-all, replace, replace-all, match, compile) are already
# available in the shared Janet environment — no import needed.
#
# Hook: on-tool-end
#   Receives ctx with :tool (string) and :output (string).
#   Returns nil (no change) or calls harness/replace-result with
#   compressed output.
#
# Pipeline:
#   1. strip-ansi       — remove CSI escape sequences
#   2. compress-generic — remove progress bars, spinners, blank-line runs
#   3. tool-specific    — per-command pattern compressors (git, cargo, etc.)

# ---------------------------------------------------------------------------
# ANSI escape stripping
# ---------------------------------------------------------------------------

(def ANSI_CSI_PATTERN '(sequence "\x1b[" (any (set "0-9;")) (set "A-Za-z")))

(defn strip-ansi
  "Remove ANSI CSI escape sequences from text."
  [text]
  (peg/replace-all ANSI_CSI_PATTERN "" text))

# ---------------------------------------------------------------------------
# Generic noise removal
# Pattern inspired by headroom's output sanitization.
# ---------------------------------------------------------------------------

(defn- progress-bar-line?
  "True if the line uses carriage-return overwrite (progress/status bar)."
  [line]
  (not (nil? (find "\r" line))))

(defn- spinner-line?
  "True if the line is a download/spinner noise line."
  [line]
  (or (not (nil? (find "Downloading " line)))
      (not (nil? (find "Downloaded " line)))
      (not (nil? (find "Collecting " line)))))

(defn- noise-line?
  "True if the line is cosmetic noise to be removed."
  [line]
  (or (progress-bar-line? line)
      (spinner-line? line)))

(defn compress-generic
  "Remove progress bars, download spinners, and collapse runs of >3 blank
  lines to a single blank line. Non-matching text passes through unchanged."
  [text]
  (let [lines (string/split "\n" text)
        kept @[]]
    (var blank-run 0)
    (each line lines
      (if (noise-line? line)
        nil  # skip noise lines
        (if (= "" line)
          (++ blank-run)
          (do
            # Flush blank run first
            (when (> blank-run 0)
              (if (<= blank-run 3)
                (repeat blank-run (array/push kept ""))
                (array/push kept ""))
              (set blank-run 0))
            (array/push kept line)))))
    # Flush trailing blank run
    (when (> blank-run 0)
      (if (<= blank-run 3)
        (repeat blank-run (array/push kept ""))
        (array/push kept "")))
    (string/join kept "\n")))

(defn compress-shell
  "Apply generic compression to shell output. Called by tool-specific
  compressors after their own transformations, or directly for
  unrecognized commands."
  [text]
  (-> text
      strip-ansi
      compress-generic))

# ---------------------------------------------------------------------------
# Hook: on-tool-end
# ---------------------------------------------------------------------------

(defn on-tool-end [ctx]
  # Only compress shell tool output — read, grep, etc. don't need it.
  (let [tool (ctx :tool)
        output (ctx :output)
        command (ctx :command)]
    (if (not (or (= tool "bash") (= tool "bash_output")))
      (break nil))
    (if (or (nil? output) (empty? output))
      nil
      (do
        # Try tool-specific compressors in priority order.
        # Each compressor returns nil if it doesn't handle the
        # command; the first non-nil result wins. Falls back to
        # generic compression (ANSI strip, noise removal).
        (def compressed
          (if (and (not (nil? command)) (not (empty? command)))
            (or (git-compress command output)
                (cargo-compress command output)
                (docker-compress command output)
                (kubectl-compress command output)
                (npm-compress command output)
                (pip-compress command output)
                (grep-compress command output)
                (find-compress command output)
                (ls-compress command output)
                (curl-compress command output)
                (compress-shell output))
            (compress-shell output)))
        (when (not= compressed output)
          (harness/replace-result compressed))))))

nil
