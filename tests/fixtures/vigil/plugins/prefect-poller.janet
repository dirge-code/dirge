# Prefect poller for vigil functional testing.
#
# Runs a one-shot curl at load time and registers /poll-prefect.
# Detected failures are pushed into the vigil-keeper via (vigil/emit ...).
#
# Requires: Prefect Server running (podman-compose up -d), vigil-keeper active.
# Vigil config: sanity-prefect.json

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

(defn- emit-prefect-failures [json-str]
  "Parse Prefect JSON and emit failed flow runs into the vigil-keeper."
  (when json-str
    (try
      (let [parsed (parse json-str)
            runs (if (indexed? (parsed :data)) (parsed :data) @[])]
        (each run runs
          (vigil/emit "prefect-remediate"
                      {:run_id (run :id)
                       :flow_name (run :name)
                       :state "FAILED"})))
      ([_] nil))))

(defn poll-prefect []
  "One-shot: curl Prefect API, parse JSON, emit failures to vigil."
  (let [json-str (sh-capture
                  "curl -s -X POST -H 'Content-Type: application/json' -d '{\"flow_runs\":{\"state\":{\"type\":{\"any_\":[\"FAILED\",\"CRASHED\"]}}}}' http://localhost:4200/api/flow_runs/filter")]
    (if json-str
      (do
        (emit-prefect-failures json-str)
        (harness/notify "prefect-poller: poll complete" :info))
      (harness/notify "prefect-poller: curl failed (is Prefect running?)" :warn))))

(harness/register-command "poll-prefect" "poll-prefect")
(poll-prefect)
