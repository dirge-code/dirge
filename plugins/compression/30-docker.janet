# Docker output compressors — ported from lean-ctx
#
# Functions defined in the shared Janet environment:
#   docker-compress [command output] → compressed string or nil

# ---------------------------------------------------------------------------
# docker build
# ---------------------------------------------------------------------------

(defn- docker-compress-build [output]
  (var steps 0)
  (var last-step "")
  (var errors @[])
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (when (or (string/has-prefix? "Step " t)
              (and (string/has-prefix? "#" t) (string/find "[" t)))
      (++ steps)
      (set last-step t))
    (when (or (string/find "ERROR" t) (string/find "error:" t))
      (array/push errors t)))
  (when (not (empty? errors))
    (break (string steps " steps, " (length errors) " errors:\n" (string/join errors "\n"))))
  (if (> steps 0)
    (string steps " steps, last: " last-step)
    "built"))

# ---------------------------------------------------------------------------
# docker ps
# ---------------------------------------------------------------------------

(defn- docker-compress-ps [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 1) (break "no containers"))
  (var containers @[])
  (each line (tuple/slice lines 1)
    (def parts (filter (fn [p] (not (empty? p))) (string/split "  " line)))
    (if (>= (length parts) 3)
      (let [name (string/trim (in parts (- (length parts) 1)))
            image (string/trim (in parts 1))
            status (string/trim (in parts (if (>= (length parts) 7) 4 3)))]
        (array/push containers (string name " (" image "): " status)))
      (let [words (filter (fn [w] (not (empty? w))) (string/split " " line))]
        (when (>= (length words) 2)
          (array/push containers (string (in words 0) " (" (in words 1) ")"))))))
  (def result (if (empty? containers) "no containers" (string/join containers "\n")))
  # Record container entities for graph search (#393)
  (each c containers
    (harness/record-entity "container" c {}))
  result)

# ---------------------------------------------------------------------------
# docker images
# ---------------------------------------------------------------------------

(defn- docker-compress-images [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 1) (break "no images"))
  (var images @[])
  (each line (tuple/slice lines 1)
    (def parts (filter (fn [p] (not (empty? p))) (string/split " " line)))
    (when (>= (length parts) 5)
      (def repo (in parts 0))
      (def tag (in parts 1))
      (def size (in parts (- (length parts) 1)))
      (when (not= repo "<none>")
        (array/push images (string repo ":" tag " (" size ")")))))
  (def result (if (empty? images) "no images"
    (string (length images) " images:\n" (string/join images "\n"))))
  # Record image entities for graph search (#393)
  (each img images
    (harness/record-entity "image" img {}))
  result)

# ---------------------------------------------------------------------------
# docker logs — dedup repeated lines, collapse timestamps
# ---------------------------------------------------------------------------

(defn- docker-compress-logs [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 10) (break output))
  (var deduped @[])
  (var last-text "")
  (var last-count 0)
  (each line lines
    (def normalized (string/trim line))
    (when (not (empty? normalized))
      (if (= normalized last-text)
        (++ last-count)
        (do
          (when (> last-count 0)
            (array/push deduped (if (> last-count 1)
                                  (string last-text " (x" last-count ")")
                                  last-text)))
          (set last-text normalized)
          (set last-count 1)))))
  (when (> last-count 0)
    (array/push deduped (if (> last-count 1)
                          (string last-text " (x" last-count ")")
                          last-text)))
  (def total (length deduped))
  (if (> total 30)
    (let [shown (take 15 deduped)
          tail (take 5 (drop (- total 15) deduped))]
      (string (string/join shown "\n")
              "\n... (" total " lines, " (- total 20) " omitted)\n"
              (string/join tail "\n")))
    (string/join deduped "\n")))

# ---------------------------------------------------------------------------
# docker compose ps
# ---------------------------------------------------------------------------

(defn- docker-compose-ps [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 1) (break "no services"))
  (var services @[])
  (each line (tuple/slice lines 1)
    (def parts (filter (fn [p] (not (empty? p))) (string/split " " line)))
    (when (>= (length parts) 3)
      (def name (in parts 0))
      (def rest (take (- (length parts) 1) (tuple/slice parts 1)))
      (array/push services (string name ": " (string/join rest " ")))))
  (if (empty? services) "no services"
    (string (length services) " services:\n" (string/join services "\n"))))

# ---------------------------------------------------------------------------
# docker compose up/down
# ---------------------------------------------------------------------------

(defn- docker-compose-action [output]
  (def trimmed (string/trim output))
  (when (empty? trimmed) (break "ok"))
  (var created 0)
  (var started 0)
  (var stopped 0)
  (var removed 0)
  (each line (string/split "\n" trimmed)
    (def l (string/ascii-lower (string/trim line)))
    (when (or (string/find "created" l) (string/find "creating" l)) (++ created))
    (when (or (string/find "started" l) (string/find "starting" l)) (++ started))
    (when (or (string/find "stopped" l) (string/find "stopping" l)) (++ stopped))
    (when (or (string/find "removed" l) (string/find "removing" l)) (++ removed)))
  (def parts @[])
  (when (> created 0) (array/push parts (string "created: " created)))
  (when (> started 0) (array/push parts (string "started: " started)))
  (when (> stopped 0) (array/push parts (string "stopped: " stopped)))
  (when (> removed 0) (array/push parts (string "removed: " removed)))
  (if (empty? parts) "ok" (string/join parts ", ")))

# ---------------------------------------------------------------------------
# docker network / volume
# ---------------------------------------------------------------------------

(defn- docker-compress-network [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 1) (break "no networks"))
  (var networks @[])
  (each line (tuple/slice lines 1)
    (def parts (filter (fn [p] (not (empty? p))) (string/split " " line)))
    (when (>= (length parts) 3)
      (array/push networks (string (in parts 0) " (" (in parts 2) ")"))))
  (if (empty? networks) "no networks"
    (string (length networks) " networks: " (string/join networks ", "))))

(defn- docker-compress-volume [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 1) (break "no volumes"))
  (var volumes @[])
  (each line (tuple/slice lines 1)
    (def parts (filter (fn [p] (not (empty? p))) (string/split " " line)))
    (when (>= (length parts) 2)
      (array/push volumes (in parts 0))))
  (if (empty? volumes) "no volumes"
    (string (length volumes) " volumes: " (string/join volumes ", "))))

# ---------------------------------------------------------------------------
# docker inspect
# ---------------------------------------------------------------------------

(defn- docker-compress-inspect [output]
  (def trimmed (string/trim output))
  (when (empty? trimmed) (break "ok"))
  (def lines (string/split "\n" trimmed))
  (if (<= (length lines) 20)
    trimmed
    (string (string/join (take 10 lines) "\n")
            "\n... (JSON, " (length lines) " lines total)")))

# ---------------------------------------------------------------------------
# docker exec / run
# ---------------------------------------------------------------------------

(defn- docker-compress-exec [output]
  (def trimmed (string/trim output))
  (when (empty? trimmed) (break "ok"))
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" trimmed)))
  (if (<= (length lines) 50)
    trimmed
    (let [head (take 25 lines)
          tail (take 25 (drop (- (length lines) 25) lines))]
      (string (string/join head "\n")
              "\n... (" (- (length lines) 50) " lines omitted)\n"
              (string/join tail "\n")))))

# ---------------------------------------------------------------------------
# docker system df
# ---------------------------------------------------------------------------

(defn- docker-compress-system-df [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (empty? lines) (break "ok"))
  (var summary @[])
  (each line lines
    (def t (string/trim line))
    (when (and (not (string/has-prefix? "TYPE" t))
               (not (string/has-prefix? "Images" t))
               (not (string/find "Build Cache" t)))
      (def parts (filter (fn [p] (not (empty? p))) (string/split "  " line)))
      (if (>= (length parts) 3)
        (array/push summary (string (in parts 0) ": " (in parts 1) " active, " (in parts 2) " size"))
        (when (> (length parts) 0)
          (array/push summary (string/join parts " "))))))
  (if (empty? summary) (string/join lines "\n") (string/join summary "\n")))

# ---------------------------------------------------------------------------
# docker info / version
# ---------------------------------------------------------------------------

(defn- docker-compress-info [output]
  (def lines (string/split "\n" output))
  (var pairs @{})
  (each line lines
    (if-let [pos (string/find ": " line)]
      (let [key (string/trim (string/slice line 0 pos))
            val (string/trim (string/slice line (+ pos 2)))]
        (put pairs key val))))
  (def important @["Server Version" "Kernel Version" "Operating System"
                   "OSType" "Architecture" "CPUs" "Total Memory"
                   "Docker Root Dir" "Default Runtime"])
  (var summary @[])
  (each k important
    (if-let [v (get pairs k)]
      (array/push summary (string k ": " v))))
  (if (empty? summary)
    (string/join (take 10 lines) "\n")
    (string/join summary "\n")))

(defn- docker-compress-version [output]
  (def lines (string/split "\n" output))
  (var summary @[])
  (each line lines
    (def t (string/trim line))
    (when (and (not (empty? t))
               (or (string/has-prefix? "Version:" t)
                   (string/has-prefix? "API version:" t)
                   (string/has-prefix? "OS/Arch:" t)))
      (array/push summary t)))
  (if (empty? summary) (string/join (take 5 lines) "\n") (string/join summary "\n")))

# ---------------------------------------------------------------------------
# Subcommand dispatch
# ---------------------------------------------------------------------------

(defn docker-compress [command output]
  (if (or (not (string/find "docker" command))
          (string/find "docker-compose" command))
    (break nil))
  (cond
    (string/find "build" command)
    (docker-compress-build output)
    (and (string/find "compose" command) (string/find " ps" command))
    (docker-compose-ps output)
    (and (string/find "compose" command)
         (or (string/find " up" command) (string/find " down" command)
             (string/find " start" command) (string/find " stop" command)))
    (docker-compose-action output)
    (string/find " ps" command)
    (docker-compress-ps output)
    (string/find "images" command)
    (docker-compress-images output)
    (string/find "logs" command)
    (docker-compress-logs output)
    (string/find "network" command)
    (docker-compress-network output)
    (string/find "volume" command)
    (docker-compress-volume output)
    (string/find "inspect" command)
    (docker-compress-inspect output)
    (or (string/find "exec" command) (string/find " run" command))
    (docker-compress-exec output)
    (and (string/find "system" command) (string/find "df" command))
    (docker-compress-system-df output)
    (string/find "info" command)
    (docker-compress-info output)
    (string/find "version" command)
    (docker-compress-version output)
    nil))

nil
