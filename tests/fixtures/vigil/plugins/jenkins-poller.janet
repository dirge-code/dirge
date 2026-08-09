# Jenkins poller for vigil functional testing.
#
# Runs a one-shot curl at load time and registers /poll-jenkins.
# Detected failures are pushed into the vigil-keeper via (vigil/emit ...).
#
# Requires: Jenkins running (podman-compose up -d), vigil-keeper active.
# Vigil config: sanity-jenkins.json

(defn- sh-capture [cmd-str]
  "Run command via /bin/sh -c with output redirection to a temp file.
  Returns trimmed stdout string or nil on failure."
  (let [tmp (string "/tmp/dirge-poll-" (os/time))]
    (os/execute ["/bin/sh" "-c" (string cmd-str " > " tmp " 2>/dev/null")])
    (try
      (let [f (file/open tmp :r)
            data (file/read f :all)]
        (file/close f)
        (os/execute ["/bin/rm" "-f" tmp])
        (when (and data (not= data "")) (string/trim data)))
      ([_] (do (os/execute ["/bin/rm" "-f" tmp]) nil)))))

(defn- emit-jenkins-failures [json-str]
  "Parse Jenkins JSON and emit failed builds into the vigil-keeper."
  (when json-str
    (try
      (let [parsed (parse json-str)
            jobs (if (indexed? (parsed :jobs)) (parsed :jobs) @[])]
        (each job jobs
          (when (= "FAILURE" (get-in job [:lastBuild :result]))
            (vigil/emit "jenkins-remediate"
                        {:job (job :name)
                         :build_number (string (get-in job [:lastBuild :number]))
                         :url (get-in job [:lastBuild :url])
                         :status "FAILURE"}))))
      ([_] nil))))

(defn poll-jenkins []
  "One-shot: curl Jenkins API, parse JSON, emit failures to vigil."
  (let [json-str (sh-capture "curl -s http://localhost:8080/api/json?tree=jobs[name,lastBuild[number,result,url]]")]
    (if json-str
      (do
        (emit-jenkins-failures json-str)
        (harness/notify "jenkins-poller: poll complete" :info))
      (harness/notify "jenkins-poller: curl failed (is Jenkins running?)" :warn))))

(harness/register-command "poll-jenkins" "poll-jenkins")
(poll-jenkins)
