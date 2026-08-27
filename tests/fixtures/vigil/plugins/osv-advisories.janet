# Security advisory watcher — open-source flow plugin (slice 6).
#
# Polls the GitHub advisories API (sorted newest-first) for a chosen ecosystem
# and files a durable issue on the LOCAL dirge board for every advisory newer
# than the last-seen one. Advisory issues are priority "high"/"normal" based on
# severity. Nothing is ever written back to GitHub — the board is the only sink.
#
# State: ~/.dirge/flow-state/osv-advisories — the latest seen GHSA id, so each
# poll only files advisories that appeared after the previous run.

(def ECOSYSTEM "cargo")
(def PER-PAGE 10)

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
(defn- state-path [] (string (state-dir) "/osv-advisories"))

(defn- read-marker []
  "Return the last-seen GHSA id, or nil on first run."
  (try
    (let [f (file/open (state-path) :r)
          data (file/read f :all)]
      (file/close f)
      (when (and data (not= data "")) (string/trim data)))
    ([_] nil)))

(defn- write-marker [ghsa]
  (os/execute ["/bin/sh" "-c" (string "mkdir -p " (state-dir) " 2>/dev/null")])
  (try
    (let [f (file/open (state-path) :w)]
      (file/write f ghsa)
      (file/close f)
      true)
    ([_] false)))

(defn- wake-agent [event-name data]
  "Emit a vigil event when the bridge exists (no-op in non-vigil builds)."
  (if-let [emit (dyn 'vigil/emit)]
    (emit event-name data)))

(defn- severity-priority [sev]
  (if (or (= sev "critical") (= sev "high")) "high" "normal"))

(defn- emit-advisory-issue [item]
  (let [ghsa (get item "ghsa_id")
        summary (get item "summary")
        severity (get item "severity")
        url (get item "html_url")
        cves (get item "cves")
        cve-str (if (indexed? cves) (string/join cves ", ") "")
        title (string "Security advisory: " ghsa " (" (or severity "unknown") ") " summary)
        body (string ghsa "\n" (or url "") "\nSeverity: " (or severity "unknown") "\nCVEs: " cve-str "\n\n" summary)]
    (harness/emit-issue title body (severity-priority severity))))

(defn poll-advisories [&opt _]
  "One-shot: fetch recent advisories and file issues for any newer than the marker."
  (let [last-seen (read-marker)
        json (sh-capture (string "curl -s \"https://api.github.com/advisories?ecosystem=" ECOSYSTEM "&sort=published&direction=desc&per_page=" PER-PAGE "\""))
        parsed (when json (harness/json-decode json))
        items (if (indexed? parsed) parsed @[])
        new-marker nil]
    (each item items
      (let [ghsa (get item "ghsa_id")]
        (if (= ghsa last-seen)
          (break)
          (do
            (emit-advisory-issue item)
            (when (nil? new-marker) (set new-marker ghsa))
            (wake-agent "advisory-watch"
                        {:ghsa ghsa :severity (get item "severity") :summary (get item "summary") :url (get item "html_url")})))))
    (when new-marker (write-marker new-marker))
    (harness/notify "osv-advisories: poll complete" :info)))

(harness/register-command "poll-advisories" "poll-advisories")
(poll-advisories)
