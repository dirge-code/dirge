# Airflow poller for vigil functional testing.
#
# Runs a one-shot curl at load time and registers /poll-airflow.
# Detected failures are pushed into the vigil-keeper via (vigil/emit ...).
#
# Requires: Airflow running (podman-compose up -d), vigil-keeper active.
# Vigil config: sanity-airflow.json

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

(defn- emit-airflow-failures [json-str]
  "Parse Airflow JSON and emit failed DAG runs into the vigil-keeper."
  (when json-str
    (try
      (let [parsed (parse json-str)
            runs (if (indexed? (parsed :dag_runs)) (parsed :dag_runs) @[])]
        (each run runs
          (vigil/emit "airflow-remediate"
                      {:dag_id (run :dag_id)
                       :run_id (run :dag_run_id)
                       :state "failed"})))
      ([_] nil))))

(defn poll-airflow []
  "One-shot: curl Airflow API, parse JSON, emit failures to vigil."
  (let [json-str (sh-capture "curl -s -u admin:admin http://localhost:8081/api/v1/dags/~/dagRuns?state=failed")]
    (if json-str
      (do
        (emit-airflow-failures json-str)
        (harness/notify "airflow-poller: poll complete" :info))
      (harness/notify "airflow-poller: curl failed (is Airflow running?)" :warn))))

(harness/register-command "poll-airflow" "poll-airflow")
(poll-airflow)
