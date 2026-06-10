# K8s debugging plugin — slash commands and hooks.
#
# /k8s     — quick cluster overview
# /k8s-pods, /k8s-logs, /k8s-desc, /k8s-events, /k8s-ns — focused queries
#
# Also hooks on-init to detect k8s availability and before-agent-start
# to inject k8s context into the system prompt.

# --- slash command handlers ---

(defn cmd-k8s-overview [args]
  "Quick cluster overview: nodes, pods, and recent events."
  (if (not k8s-available)
    "k8s not available — is minikube running? Try `minikube status`"
    (do
      (def pods (compact-table (k8s-sh "kubectl get pods --all-namespaces -o wide 2>&1")))
      (def nodes (compact-table (k8s-sh "kubectl get nodes -o wide 2>&1")))
      (def events (compact-table (k8s-sh "kubectl get events --all-namespaces --sort-by=.lastTimestamp 2>&1 | tail -30")))
      (string "## Nodes\n" nodes "\n\n## Pods (all namespaces)\n" pods
              "\n\n## Recent Events\n" events))))

(defn cmd-k8s-pods [args]
  (if (not k8s-available)
    "k8s not available — is minikube running?"
    (do
      (def ns (if (= args "") k8s-namespace args))
      (def cmd (if (or (= ns "-A") (= ns "--all"))
                 "kubectl get pods --all-namespaces -o wide 2>&1"
                 (string "kubectl get pods -n " ns " -o wide 2>&1")))
      (compact-table (k8s-sh cmd)))))

(defn cmd-k8s-logs [args]
  (if (not k8s-available)
    "k8s not available — is minikube running?"
    (do
      (def parts (string/split " " args))
      (def pod (first parts))
      (def rest (string/join (array/slice parts 1) " "))
      (if (or (not pod) (empty? pod))
        "usage: /k8s-logs <pod> [--previous] [-c container]"
        (do
          (def prev (string/find "--previous" rest))
          (def cont (string/find "-c " rest))
          (def container (if cont (string/trim (string/slice rest (+ cont 3))) ""))
          (def cmd (string "kubectl logs " pod
                           (if k8s-namespace (string " -n " k8s-namespace) "")
                           " --tail=150"
                           (if prev " --previous" "")
                           (if container (string " -c " container) "")
                           " 2>&1"))
          (k8s-sh cmd))))))

(defn cmd-k8s-desc [args]
  (if (not k8s-available)
    "k8s not available — is minikube running?"
    (do
      (def parts (string/split " " args))
      (def resource (first parts))
      (def name (if (> (length parts) 1) (get parts 1) ""))
      (if (or (not resource) (empty? resource))
        "usage: /k8s-desc <resource> [name]\ne.g. /k8s-desc pod my-app-7d4f8b9c-abcde"
        (do
          (def cmd (string "kubectl describe " resource
                           (if name (string " " name) "")
                           (if k8s-namespace (string " -n " k8s-namespace) "")
                           " 2>&1"))
          (k8s-sh cmd))))))

(defn cmd-k8s-events [args]
  (if (not k8s-available)
    "k8s not available — is minikube running?"
    (do
      (def ns (if (= args "") k8s-namespace args))
      (def cmd (if (or (= ns "-A") (= ns "--all") (not ns))
                 "kubectl get events --all-namespaces --sort-by=.lastTimestamp 2>&1 | tail -60"
                 (string "kubectl get events -n " ns " --sort-by=.lastTimestamp 2>&1 | tail -60")))
      (compact-table (k8s-sh cmd)))))

(defn cmd-k8s-ns [args]
  "Set or show the current namespace."
  (if (= args "")
    (string "current namespace: " (or k8s-namespace "(default)"))
    (do
      (set k8s-namespace args)
      (string "namespace set to: " k8s-namespace))))

(defn cmd-k8s-context [args]
  "Show k8s connection status."
  (if k8s-available
    (string "k8s available\n  context: " k8s-context
            "\n  namespace: " (or k8s-namespace "(default)")
            "\n  minikube ip: " (or (k8s-sh "minikube ip 2>&1") "unknown"))
    "k8s not available — run `minikube start` or check kubectl config"))

# --- hooks ---

(defn on-init [ctx]
  "Detect k8s availability at session start."
  (when (k8s-detect)
    (k8s-namespace-detect)
    (harness/notify
      (string "k8s ready — context: " k8s-context
              (if k8s-namespace (string ", namespace: " k8s-namespace) ""))
      :info)))

(defn before-agent-start [ctx]
  "Inject k8s context into the system prompt when available."
  (when k8s-available
    (harness/append-system-prompt
      (string "\n## Kubernetes cluster available\n"
              "- Context: `" k8s-context "`\n"
              "- Namespace: " (or k8s-namespace "(default)") "\n"
              "- Use the `k8s_get_*` tools for structured cluster queries: "
              "k8s_get_pods, k8s_get_logs, k8s_describe, k8s_get_events, "
              "k8s_get_nodes, k8s_get_services, k8s_get_deployments, "
              "k8s_top_pods, k8s_get_configmaps, k8s_get_secrets_list, "
              "k8s_get_ingresses, k8s_api_resources.\n"
              "- For any kubectl command not covered by a tool, use the bash tool "
              "with `kubectl --context=" k8s-context "`.\n"
              "- For minikube-specific operations (minikube dashboard, "
              "minikube service, minikube addons), use the bash tool."))))

# --- register commands ---

(harness/register-command "k8s" "cmd-k8s-overview")
(harness/register-command "k8s-pods" "cmd-k8s-pods")
(harness/register-command "k8s-logs" "cmd-k8s-logs")
(harness/register-command "k8s-desc" "cmd-k8s-desc")
(harness/register-command "k8s-events" "cmd-k8s-events")
(harness/register-command "k8s-ns" "cmd-k8s-ns")
(harness/register-command "k8s-context" "cmd-k8s-context")
