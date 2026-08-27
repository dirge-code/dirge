# GitHub issue/PR triage — open-source flow plugin (slice 6).
#
# Polls a repo's OPEN issues (which include PRs — GitHub shares one number
# space) and files a durable issue on the LOCAL dirge board for every item whose
# number is higher than the last-seen max. This builds a standing triage queue
# the agent works through. Nothing is ever written back to GitHub — "file an
# issue" here means the dirge board, not the remote repo.
#
# State: ~/.dirge/flow-state/gh-triage — one "owner/repo<TAB>maxNumber" line per
# watched repo.

(def WATCH
  "Repos to triage, as @[[owner repo] ...] pairs."
  @[["dirge-code" "dirge"]])

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
(defn- state-path [] (string (state-dir) "/gh-triage"))

(defn- to-num [x]
  "Coerce a state value to a number; non-numeric degrades to 0."
  (if (number? x) x (or (scan-number x) 0)))

(defn- read-state []
  "Return the state file as a table of repoKey -> last-seen max item number."
  (var out @{})
  (try
    (let [f (file/open (state-path) :r)
          data (file/read f :all)]
      (file/close f)
      (each line (string/split "\n" (or data ""))
        (let [parts (string/split "\t" line)]
          (when (= 2 (length parts))
            (put out (get parts 0) (to-num (get parts 1)))))))
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
  "Emit a vigil event when the bridge exists (no-op in non-vigil builds)."
  (if-let [emit (dyn 'vigil/emit)]
    (emit event-name data)))

(defn- emit-triage-issue [repo-key number title url is-pr]
  (let [kind (if is-pr "PR" "issue")
        issue-title (string "Triage: " repo-key "#" number " (" kind ") " title)
        issue-body (string "Open " kind " needs triage.\n" (or url "") "\n\n" title)]
    (harness/emit-issue issue-title issue-body "normal")))

(defn poll-triage [&opt _]
  "One-shot: fetch open issues/PRs and file issues for any new item numbers."
  (let [state (read-state)]
    (each pair WATCH
      (let [owner (get pair 0)
            repo (get pair 1)
            repo-key (string owner "/" repo)
            json (sh-capture (string "curl -s \"https://api.github.com/repos/" owner "/" repo "/issues?state=open&per_page=100\""))
            parsed (when json (harness/json-decode json))
            items (if (indexed? parsed) parsed @[])
            last-seen (to-num (get state repo-key))]
        (var max-seen last-seen)
        (each item items
          (let [number (get item "number")]
            (when (and (number? number) (> number last-seen))
              (emit-triage-issue repo-key number (get item "title") (get item "html_url") (truthy? (get item "pull_request")))
              (when (> number max-seen) (set max-seen number))
              (wake-agent "triage-watch"
                          {:repo repo-key :number (string number) :title (get item "title") :url (get item "html_url")}))))
        (when (> max-seen last-seen) (put state repo-key max-seen))))
    (write-state state)
    (harness/notify "gh-triage: poll complete" :info)))

(harness/register-command "poll-triage" "poll-triage")
(poll-triage)
