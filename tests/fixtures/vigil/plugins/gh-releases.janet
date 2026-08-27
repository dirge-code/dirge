# GitHub release watcher — open-source flow plugin (slice 6).
#
# Polls the GitHub REST API for the latest release of each watched repo. When a
# repo's tag is newer than the last-seen tag, it files a durable issue on the
# LOCAL dirge board via (harness/emit-issue ...) — nothing is ever written back
# to GitHub. If a vigil-keeper is running, it also emits a vigil event so the
# agent wakes and triages the release; without vigil the issue still lands on
# the board for loop / interactive mode to pick up.
#
# State: ~/.dirge/flow-state/gh-releases — one "owner/repo<TAB>tag" line per
# watched repo. Unauthenticated GitHub API: 60 req/hr per IP.

(def WATCH
  "Repos to watch for new releases, as @[[owner repo] ...] pairs."
  @[["dirge-code" "dirge"]
    ["fish-shell" "fish-shell"]])

(defn- sh-capture [cmd-str]
  "Run via /bin/sh -c, capturing trimmed stdout to a temp file."
  (let [tmp (string "/tmp/dirge-flow-" (os/time))]
    (os/execute ["/bin/sh" "-c" (string cmd-str " > " tmp " 2>/dev/null")])
    (try
      (let [f (file/open tmp :r)
            data (file/read f :all)]
        (file/close f)
        (os/execute ["/bin/rm" "-f" tmp])
        (when (and data (not= data "")) (string/trim data)))
      ([_] (do (os/execute ["/bin/rm" "-f" tmp]) nil)))))

(defn- home [] (or (os/getenv "HOME") "/tmp"))
(defn- state-dir [] (string (home) "/.dirge/flow-state"))
(defn- state-path [] (string (state-dir) "/gh-releases"))

(defn- read-state []
  "Return the state file as a table of repoKey -> last-seen tag."
  (var out @{})
  (try
    (let [f (file/open (state-path) :r)
          data (file/read f :all)]
      (file/close f)
      (each line (string/split "\n" (or data ""))
        (let [parts (string/split "\t" line)]
          (when (= 2 (length parts))
            (put out (get parts 0) (get parts 1))))))
    ([_] nil))
  out)

(defn- write-state [table]
  (os/execute ["/bin/sh" "-c" (string "mkdir -p " (state-dir) " 2>/dev/null")])
  (try
    (let [f (file/open (state-path) :w)]
      (file/write f (string/join
                      (map (fn [[k v]] (string k "\t" v)) (pairs table))
                      "\n"))
      (file/close f)
      true)
    ([_] false)))

(defn- wake-agent [event-name data]
  "Emit a vigil event when the bridge exists. (dyn ...) avoids a compile-time
   unknown-symbol error in non-vigil builds, and vigil/emit itself no-ops when
   no keeper is running."
  (if-let [emit (dyn 'vigil/emit)]
    (emit event-name data)))

(defn- emit-release-issue [owner repo tag url body]
  (let [title (string "New release: " owner "/" repo " " tag)
        issue-body (string owner "/" repo " released " tag "\n" (or url "") "\n\n" (or body ""))
        id (harness/emit-issue title issue-body "normal")]
    (when id
      (harness/notify (string "gh-releases: filed " id " for " owner "/" repo " " tag) :info))
    id))

(defn poll-releases [&opt _]
  "One-shot: fetch latest releases and file issues for any new tags."
  (let [state (read-state)]
    (var any-new false)
    (each pair WATCH
      (let [owner (get pair 0)
            repo (get pair 1)
            repo-key (string owner "/" repo)
            json (sh-capture (string "curl -s https://api.github.com/repos/" owner "/" repo "/releases/latest"))
            parsed (when json (harness/json-decode json))
            tag (when parsed (get parsed "tag_name"))]
        (when (and tag (not= tag (get state repo-key)))
          (emit-release-issue owner repo tag (get parsed "html_url") (get parsed "body"))
          (put state repo-key tag)
          (set any-new true)
          (wake-agent "release-watch"
                      {:repo repo-key :tag tag :url (get parsed "html_url")}))))
    (when any-new (write-state state))
    (harness/notify "gh-releases: poll complete" :info)))

(harness/register-command "poll-releases" "poll-releases")
(poll-releases)
