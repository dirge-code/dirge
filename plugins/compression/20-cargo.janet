# Cargo output compressors — ported from lean-ctx
#
# Functions defined in the shared Janet environment:
#   cargo-compress [command output] → compressed string or nil

# ---------------------------------------------------------------------------
# Shared patterns
# ---------------------------------------------------------------------------

# Match "Compiling crate v1.2.3" — capture crate name and version
(defn- compiling-match [line]
  (match "Compiling ([^ ]+) v([^ ]+)" line))

# Match "error[E0123]: message" — capture error code and message
(defn- error-match [line]
  (match "error\\[E([0-9]+)\\]: (.+)" line))

# Match "warning: message"
(defn- warning-match [line]
  (match "warning: (.+)" line))

# Match "test result: ok. 5 passed; 0 failed; 0 ignored"
(defn- test-result-match [line]
  (match "test result: ([a-zA-Z]+)\\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored" line))

# Match "Finished dev [unoptimized] target(s) in 12.34s"
# Note: spork/regex PEG doesn't backtrack, so .+ would greedily consume
# the " in" marker. Use string/find instead.
(defn- finished-match [line]
  (if-let [pos (string/find " in " line)]
    (let [time (string/trim (string/slice line (+ pos 4)))]
      (if (not (empty? time)) time))))

# ---------------------------------------------------------------------------
# cargo build / check
# ---------------------------------------------------------------------------

(defn- cargo-compress-build [output]
  (var crate-count 0)
  (var crates @[])
  (var errors @[])
  (var warnings 0)
  (var time "")
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (if-let [c (compiling-match t)]
      (do
        (++ crate-count)
        (array/push crates (string (in c 0) " v" (in c 1)))))
    (if-let [c (error-match t)]
      (do
        (def err (string "E" (in c 0) ": " (in c 1)))
        (array/push errors err)
        (harness/record-entity "error" (string "E" (in c 0)) {:message (in c 1)})))
    (when (and (warning-match t)
               (not (string/find "generated" t)))
      (++ warnings))
    (if-let [time-val (finished-match t)]
      (set time time-val)))
  (def parts @[])
  (when (> crate-count 0)
    (array/push parts (string "compiled " crate-count " crates")))
  (each c crates
    (harness/record-entity "crate" c {}))
  (when (not (empty? errors))
    (array/push parts (string (length errors) " errors:"))
    (each e errors (array/push parts (string "  " e))))
  (when (> warnings 0)
    (array/push parts (string warnings " warnings")))
  (when (not (empty? time))
    (array/push parts (string "(" time ")")))
  (if (empty? parts) "ok" (string/join parts "\n")))

# ---------------------------------------------------------------------------
# cargo test
# ---------------------------------------------------------------------------

(defn- cargo-compress-test [output]
  (var results @[])
  (var failed-tests @[])
  (var passed-tests @[])
  (var time "")
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (if-let [c (test-result-match t)]
      (array/push results (string (in c 0) ": " (in c 1) " pass, " (in c 2) " fail, " (in c 3) " skip")))
    (when (and (string/find "FAILED" t) (string/find "---" t))
      (if-let [name (last (string/split " " t))]
        (do
          (array/push failed-tests name)
          (harness/record-entity "test" name {:status "failing"}))))
    (if (and (string/has-prefix? "test " t)
             (>= (length line) 6)
             (= (string/slice line (- (length line) 6)) " ... ok"))
      (let [name (string/trim (string/replace "test " "" (string/replace " ... ok" "" line)))]
        (if (> (length name) 50)
          (array/push passed-tests (string/slice name 0 50))
          (array/push passed-tests name))))
    (if-let [time-val (finished-match t)]
      (set time time-val)))
  (def parts @[])
  (when (not (empty? results))
    (each r results (array/push parts r)))
  (when (not (empty? failed-tests))
    (array/push parts (string "failed: " (string/join failed-tests ", "))))
  (when (not (empty? passed-tests))
    (def total (length passed-tests))
    (def shown (take 5 passed-tests))
    (def suffix (if (> total 5) (string " ...+" (- total 5) " more") ""))
    (array/push parts (string "ran: " (string/join shown ", ") suffix)))
  (when (not (empty? time))
    (array/push parts (string "(" time ")")))
  (if (empty? parts) "ok" (string/join parts "\n")))

# ---------------------------------------------------------------------------
# cargo clippy
# ---------------------------------------------------------------------------

(defn- cargo-compress-clippy [output]
  (var warnings @[])
  (var errors @[])
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (if-let [c (error-match t)]
      (array/push errors (in c 1))
      (if-let [c (warning-match t)]
        (let [msg (in c 0)]
          (when (and (not (string/find "generated" msg))
                     (not (string/has-prefix? "`" msg)))
            (array/push warnings msg))))))
  (def parts @[])
  (when (not (empty? errors))
    (array/push parts (string (length errors) " errors: " (string/join errors "; "))))
  (when (not (empty? warnings))
    (array/push parts (string (length warnings) " warnings")))
  (if (empty? parts) "clean" (string/join parts "\n")))

# ---------------------------------------------------------------------------
# cargo doc
# ---------------------------------------------------------------------------

(defn- cargo-compress-doc [output]
  (var crate-count 0)
  (var warnings 0)
  (var time "")
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (when (or (string/find "Documenting " t) (compiling-match t))
      (++ crate-count))
    (when (and (warning-match t) (not (string/find "generated" t)))
      (++ warnings))
    (if-let [time-val (finished-match t)]
      (set time time-val)))
  (def parts @[])
  (when (> crate-count 0)
    (array/push parts (string "documented " crate-count " crates")))
  (when (> warnings 0)
    (array/push parts (string warnings " warnings")))
  (when (not (empty? time))
    (array/push parts (string "(" time ")")))
  (if (empty? parts) "ok" (string/join parts "\n")))

# ---------------------------------------------------------------------------
# cargo tree
# ---------------------------------------------------------------------------

(defn- cargo-compress-tree [output]
  (def lines (string/split "\n" output))
  (if (<= (length lines) 20) (break output))
  (def direct (filter
                (fn [l]
                  (or (not (string/has-prefix? " " l))
                      (string/has-prefix? "\xe2\x94\x9c\xe2\x94\x80\xe2\x94\x80 " l)
                      (string/has-prefix? "\xe2\x94\x94\xe2\x94\x80\xe2\x94\x80 " l)))
                lines))
  (if (empty? direct)
    (let [shown (take 20 lines)]
      (string (string/join shown "\n")
              "\n... (" (- (length lines) 20) " more lines)"))
    (string (length direct) " direct deps (" (length lines) " total lines):\n"
            (string/join direct "\n"))))

# ---------------------------------------------------------------------------
# cargo fmt
# ---------------------------------------------------------------------------

(defn- cargo-compress-fmt [output]
  (def trimmed (string/trim output))
  (when (empty? trimmed) (break "ok (formatted)"))
  (def diffs (filter
               (fn [l] (or (string/has-prefix? "Diff in " (string/trim l))
                          (string/has-prefix? "  --> " l)))
               (string/split "\n" trimmed)))
  (when (not (empty? diffs))
    (break (string (length diffs) " formatting issues:\n" (string/join diffs "\n"))))
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" trimmed)))
  (if (<= (length lines) 5)
    (string/join lines "\n")
    (string (string/join (take 5 lines) "\n")
            "\n... (" (- (length lines) 5) " more lines)")))

# ---------------------------------------------------------------------------
# cargo update
# ---------------------------------------------------------------------------

(defn- cargo-compress-update [output]
  (var updated @[])
  (var unchanged 0)
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (cond
      (or (string/has-prefix? "Updating " t) (string/has-prefix? "    Updating " t))
      (array/push updated t)
      (or (string/has-prefix? "Unchanged " t) (string/find "Unchanged" t))
      (++ unchanged)))
  (if (and (empty? updated) (= unchanged 0))
    (do
      (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
      (if (empty? lines) (break "ok (up-to-date)"))
      (if (<= (length lines) 5) (break (string/join lines "\n")))
      (break (string (string/join (take 5 lines) "\n")
                     "\n... (" (- (length lines) 5) " more lines)"))))
  (def parts @[])
  (when (not (empty? updated))
    (array/push parts (string (length updated) " updated:"))
    (each u (take 15 updated) (array/push parts (string "  " u)))
    (when (> (length updated) 15)
      (array/push parts (string "  ... +" (- (length updated) 15) " more"))))
  (when (> unchanged 0)
    (array/push parts (string unchanged " unchanged")))
  (string/join parts "\n"))

# ---------------------------------------------------------------------------
# cargo run
# ---------------------------------------------------------------------------

(defn- cargo-compress-run [output]
  (var compiling 0)
  (var time "")
  (var program-lines @[])
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (if (or (compiling-match t) (string/has-prefix? "Compiling " t))
      (++ compiling)
      (if (or (string/has-prefix? "Running `" t)
              (string/has-prefix? "Running " t))
        nil
        (if-let [time-val (finished-match t)]
          (set time time-val)
          (array/push program-lines line)))))
  (def result @[])
  (when (> compiling 0)
    (def header (string "(compiled " compiling " crates"))
    (def with-time (if (not (empty? time))
                     (string header ", " time ")")
                     (string header ")")))
    (array/push result with-time))
  (if (<= (length program-lines) 50)
    (each l program-lines (array/push result l))
    (do
      (each l (take 25 program-lines) (array/push result l))
      (array/push result (string "... (" (- (length program-lines) 50) " lines omitted)"))
      (each l (take 25 (drop (- (length program-lines) 25) program-lines))
        (array/push result l))))
  (def joined (string/join result "\n"))
  (if (empty? (string/trim joined)) "ok" joined))

# ---------------------------------------------------------------------------
# cargo bench
# ---------------------------------------------------------------------------

(defn- cargo-compress-bench [output]
  (var compiling 0)
  (var time "")
  (var errors @[])
  (var bench-results @[])
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (cond
      (or (compiling-match t) (string/has-prefix? "Compiling " t)) (++ compiling)
      (or (string/has-prefix? "Benchmarking " t)
          (string/has-prefix? "Gnuplot " t)
          (string/has-prefix? "Collecting " t)
          (string/has-prefix? "Warming up" t)
          (string/has-prefix? "Analyzing " t)) nil
      (and (string/has-prefix? "Running " t) (string/find "target" t)) nil
      (finished-match t) (set time (finished-match t))
      (error-match t) (let [c (error-match t)]
                        (array/push errors (string "E" (in c 0) ": " (in c 1))))
      (and (string/has-prefix? "test " t) (string/find "bench:" t)) (array/push bench-results t)
      (or (string/find "time:" t) (string/find "thrpt:" t)) (array/push bench-results t)
      (test-result-match t) (let [c (test-result-match t)]
                              (array/push bench-results
                                (string (in c 0) ": " (in c 1) " pass, " (in c 2) " fail, " (in c 3) " skip")))))
  (def parts @[])
  (when (not (empty? errors))
    (array/push parts (string (length errors) " errors:"))
    (each e errors (array/push parts (string "  " e)))
    (break (string/join parts "\n")))
  (when (> compiling 0)
    (def header (string "compiled " compiling " crates"))
    (def with-time (if (not (empty? time))
                     (string header " (" time ")")
                     header))
    (array/push parts with-time))
  (if (empty? bench-results)
    (array/push parts "no benchmark results captured")
    (do
      (array/push parts (string (length bench-results) " benchmarks:"))
      (each b bench-results (array/push parts (string "  " b)))))
  (if (empty? parts) "ok" (string/join parts "\n")))

# ---------------------------------------------------------------------------
# cargo metadata
# ---------------------------------------------------------------------------

(defn- cargo-compress-metadata [output]
  (def lines (string/split "\n" output))
  (if (or (string/has-prefix? "{" (string/trim output))
          (and (> (length lines) 1)
               (string/has-prefix? "{" (string/trim (in lines 0)))))
    (do
      (def trimmed (string/trim output))
      (def parts @[(string (length lines) " lines of JSON metadata")])
      (def workspace-members (match "\"workspace_members\"[^\\[]*\\[([^\\]]*)" trimmed))
      (when (not (nil? workspace-members))
        (def members (string/split "," (in workspace-members 0)))
        (array/push parts (string "  workspace_members: " (length members))))
      (def target-dir (match "\"target_directory\"[^\\\"]*\"([^\"]*)\"" trimmed))
      (when (not (nil? target-dir))
        (array/push parts (string "  target_directory: " (in target-dir 0))))
      (def workspace-root (match "\"workspace_root\"[^\\\"]*\"([^\"]*)\"" trimmed))
      (when (not (nil? workspace-root))
        (array/push parts (string "  workspace_root: " (in workspace-root 0))))
      (string/join parts "\n"))
    (let [lines (string/split "\n" output)]
      (if (<= (length lines) 20)
        output
        (string (string/join (take 10 lines) "\n")
                "\n... (" (- (length lines) 10) " more lines, non-JSON metadata)")))))

# ---------------------------------------------------------------------------
# Subcommand dispatch
# ---------------------------------------------------------------------------

(defn cargo-compress [command output]
  (if (not (string/find "cargo" command)) (break nil))
  (cond
    (or (string/find "build" command) (string/find "check" command))
    (cargo-compress-build output)
    (string/find "test" command)
    (cargo-compress-test output)
    (string/find "clippy" command)
    (cargo-compress-clippy output)
    (string/find "doc" command)
    (cargo-compress-doc output)
    (string/find "tree" command)
    (cargo-compress-tree output)
    (string/find "metadata" command)
    (cargo-compress-metadata output)
    (string/find "fmt" command)
    (cargo-compress-fmt output)
    (string/find "update" command)
    (cargo-compress-update output)
    (string/find "run" command)
    (cargo-compress-run output)
    (string/find "bench" command)
    (cargo-compress-bench output)
    nil))

nil
