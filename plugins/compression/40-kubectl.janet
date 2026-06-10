# Kubectl output compressors — ported from lean-ctx
#
# Functions defined in the shared Janet environment:
#   kubectl-compress [command output] → compressed string or nil

# ---------------------------------------------------------------------------
# kubectl get — table aggregation with STATUS counts
# ---------------------------------------------------------------------------

(defn- kubectl-compress-get [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (empty? lines) (break "no resources"))
  (if (and (= (length lines) 1) (string/has-prefix? "No resources" (in lines 0)))
    (break "no resources"))
  (if (<= (length lines) 1) (break (string/trim output)))
  (def header (in lines 0))
  (def cols (filter (fn [c] (not (empty? c))) (string/split " " header)))
  (var status-col -1)
  (each [i col] (pairs cols)
    (when (= (string/ascii-lower col) "status") (set status-col i)))
  (def data (filter (fn [l] (not (empty? (string/split " " l)))) (tuple/slice lines 1)))
  (if (empty? data) (break "no resources"))
  (def total (length data))

  # Aggregation path: if we have STATUS column and >5 resources
  (if (and (>= status-col 0) (> total 5))
    (do
      (var running 0)
      (var pending 0)
      (var other @[])
      (each line data
        (def parts (filter (fn [p] (not (empty? p))) (string/split " " line)))
        (when (>= (length parts) (+ status-col 1))
          (def s (in parts status-col))
          (cond
            (or (= s "Running") (= s "Active")) (++ running)
            (or (= s "Pending") (= s "ContainerCreating")) (++ pending)
            (array/push other s))))
      (def parts @[])
      (when (> running 0) (array/push parts (string running " Running")))
      (when (> pending 0) (array/push parts (string pending " Pending")))
      # Dedup other statuses
      (def seen @{})
      (each s other (put seen s true))
      (each [s _] (pairs seen)
        (def count
          (length (filter
                    (fn [p]
                      (def pcols (filter (fn [x] (not (empty? x))) (string/split " " p)))
                      (and (>= (length pcols) (+ status-col 1))
                           (= (in pcols status-col) s)))
                    data)))
        (when (> count 0) (array/push parts (string count " " s))))
      (break (string total " resources (" (string/join parts ", ") ")"))))

  # Compact table path: small result sets
  (def rows @[])
  (each line data
    (def parts (filter (fn [p] (not (empty? p))) (string/split " " line)))
    (when (> (length parts) 0)
      (def name (in parts 0))
      (def rest (take 4 (tuple/slice parts 1)))
      (array/push rows (string name " " (string/join rest " ")))))
  (def col-hint (string/join (take 4 cols) " "))
  (string "[" col-hint "]\n" (string/join rows "\n")))

# ---------------------------------------------------------------------------
# kubectl logs — dedup
# ---------------------------------------------------------------------------

(defn- kubectl-compress-logs [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (when (<= (length lines) 10) (break output))
  (var deduped @[])
  (var last-text "")
  (var last-count 0)
  (each line lines
    (def stripped (string/trim line))
    (when (not (empty? stripped))
      (if (= stripped last-text)
        (++ last-count)
        (do
          (when (> last-count 0)
            (array/push deduped (if (> last-count 1)
                                  (string last-text " (x" last-count ")")
                                  last-text)))
          (set last-text stripped)
          (set last-count 1)))))
  (when (> last-count 0)
    (array/push deduped (if (> last-count 1)
                          (string last-text " (x" last-count ")")
                          last-text)))
  (def total (length deduped))
  (if (> total 30)
    (let [tail (take 20 (drop (- total 20) deduped))]
      (string "... (" (length lines) " lines total)\n" (string/join tail "\n")))
    (string/join deduped "\n")))

# ---------------------------------------------------------------------------
# kubectl describe — section summaries
# ---------------------------------------------------------------------------

(defn- kubectl-compress-describe [output]
  (def lines (string/split "\n" output))
  (when (<= (length lines) 20) (break output))
  (var sections @[])
  (var section-name "")
  (var section-lines @[])
  (each line lines
    (if (and (not (string/has-prefix? " " line))
             (not (string/has-prefix? "\t" line))
             (and (>= (length line) 1)
                  (= (string/slice line (- (length line) 1)) ":"))
             (not (string/find "  " line)))
      (do
        (when (not (empty? section-name))
          (let [count (length section-lines)]
            (if (<= count 3)
              (array/push sections (string section-name "\n" (string/join section-lines "\n")))
              (array/push sections (string section-name " (" count " lines)")))))
        (set section-name (string/trim line))
        (set section-lines @[]))
      (array/push section-lines line)))
  (when (not (empty? section-name))
    (let [count (length section-lines)]
      (if (= section-name "Events:")
        (let [events (take 5 (drop (max 0 (- count 5)) section-lines))]
          (array/push sections (string "Events (last 5 of " count "):\n" (string/join events "\n"))))
        (if (<= count 5)
          (array/push sections (string section-name "\n" (string/join section-lines "\n")))
          (array/push sections (string section-name " (" count " lines)"))))))
  (string/join sections "\n"))

# ---------------------------------------------------------------------------
# kubectl apply — resource action counting
# ---------------------------------------------------------------------------

(defn- kubectl-compress-apply [output]
  (def trimmed (string/trim output))
  (when (empty? trimmed) (break "ok"))
  (var configured 0)
  (var created 0)
  (var unchanged 0)
  (var deleted 0)
  (each line (string/split "\n" trimmed)
    (if-let [c (match "([^ /]+/[^ /]+) (configured|created|unchanged|deleted)" (string/trim line))]
      (let [action (in c 1)]
        (case action
          "configured" (++ configured)
          "created" (++ created)
          "unchanged" (++ unchanged)
          "deleted" (++ deleted)))))
  (def total (+ configured created unchanged deleted))
  (when (= total 0)
    (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" trimmed)))
    (break (if (<= (length lines) 5) trimmed
             (string (string/join (take 5 lines) "\n")
                     "\n... (" (- (length lines) 5) " more lines)"))))
  (def summary @[])
  (when (> created 0) (array/push summary (string created " created")))
  (when (> configured 0) (array/push summary (string configured " configured")))
  (when (> unchanged 0) (array/push summary (string unchanged " unchanged")))
  (when (> deleted 0) (array/push summary (string deleted " deleted")))
  (string "ok (" total " resources: " (string/join summary ", ") ")"))

# ---------------------------------------------------------------------------
# kubectl delete / exec / top / rollout
# ---------------------------------------------------------------------------

(defn- kubectl-compress-delete [output]
  (def trimmed (string/trim output))
  (when (empty? trimmed) (break "ok"))
  (def deleted (filter (fn [l] (string/find "deleted" l)) (string/split "\n" trimmed)))
  (when (empty? deleted)
    (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" trimmed)))
    (break (if (<= (length lines) 3) trimmed
             (string (string/join (take 3 lines) "\n")
                     "\n... (" (- (length lines) 3) " more lines)"))))
  (string "deleted " (length deleted) " resources"))

(defn- kubectl-compress-exec [output]
  (def trimmed (string/trim output))
  (when (empty? trimmed) (break "ok"))
  (def lines (string/split "\n" trimmed))
  (if (<= (length lines) 20) trimmed
    (string "... (" (length lines) " lines)\n"
           (string/join (take 10 (drop (- (length lines) 10) lines)) "\n"))))

(defn- kubectl-compress-table [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 15) (string/join lines "\n")
    (string (string/join (take 15 lines) "\n")
            "\n... (" (- (length lines) 15) " more rows)")))

# ---------------------------------------------------------------------------
# Subcommand dispatch
# ---------------------------------------------------------------------------

(defn kubectl-compress [command output]
  (if (not (or (string/find "kubectl" command) (string/find "kubectx" command)))
    (break nil))
  (cond
    (string/find "logs" command) (kubectl-compress-logs output)
    (string/find "describe" command) (kubectl-compress-describe output)
    (string/find "apply" command) (kubectl-compress-apply output)
    (string/find "delete" command) (kubectl-compress-delete output)
    (string/find "get" command) (kubectl-compress-get output)
    (string/find "exec" command) (kubectl-compress-exec output)
    (string/find "top" command) (kubectl-compress-table output)
    (string/find "rollout" command) (kubectl-compress-table output)
    (string/find "scale" command) (kubectl-compress-table output)
    (kubectl-compress-table output)))

nil
