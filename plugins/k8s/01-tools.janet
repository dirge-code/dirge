# K8s debugging plugin — LLM-callable tools.
#
# Registers structured tools for k8s/minikube debugging so the LLM
# doesn't need to remember kubectl flags. Each tool wraps kubectl
# with the detected context and namespace.

# --- lightweight JSON arg parser ---
# Janet has no built-in JSON parser in the restricted plugin env.
# Extract top-level string/bool/number fields from a JSON object.
# MUST be defined BEFORE tool handlers (Janet compiles top-to-bottom).

(defn- extract-str [s field]
  (def marker (string "\"" field "\""))
  (def mp (string/find marker s))
  (when mp
    (def after (string/slice s (+ mp (length marker))))
    (def q1 (string/find "\"" after))
    (when q1
      (def rest (string/slice after (+ q1 1)))
      (def q2 (string/find "\"" rest))
      (when q2 (string/slice rest 0 q2)))))

(defn- extract-bool [s field]
  (or (string/find (string "\"" field "\": true") s)
      (string/find (string "\"" field "\":true") s)))

(defn- extract-int [s field]
  (def marker (string "\"" field "\""))
  (def mp (string/find marker s))
  (when mp
    (def after (string/slice s (+ mp (length marker))))
    (def colon (string/find ":" after))
    (when colon
      (def rest (string/trim (string/slice after (+ colon 1))))
      (when (not (empty? rest))
        (scan-number rest)))))

(defn scan-number [s]
  (var n 0)
  (var i 0)
  (var len (length s))
  (while (< i len)
    (def c (string/slice s i (+ i 1)))
    (if (or (= c "0") (= c "1") (= c "2") (= c "3") (= c "4")
            (= c "5") (= c "6") (= c "7") (= c "8") (= c "9"))
      (do (set n (+ (* n 10) (- (first (string/bytes c)) 48)))
          (set i (+ i 1)))
      (break)))
  (if (> i 0) n nil))

(defn parse-args [args]
  (def out @{})
  (when args
    (each field ["pod" "container" "resource" "name" "namespace"
                 "api_group" "labels" "field" "command"]
      (def v (extract-str args field))
      (when v (put out (keyword field) v)))
    (each field ["all_namespaces" "previous"]
      (when (extract-bool args field)
        (put out (keyword field) true)))
    (def tail-num (extract-int args "tail"))
    (when tail-num (put out :tail tail-num)))
  out)

# --- tool handlers ---

(defn tool-get-pods [args]
  (def json (parse-args args))
  (def ns (get json :namespace k8s-namespace))
  (def labels (get json :labels ""))
  (def field (get json :field ""))
  (def all-ns (get json :all_namespaces false))
  (def cmd (string "kubectl get pods "
                   (if all-ns "--all-namespaces " "")
                   (if (and ns (not all-ns)) (string "-n " ns " ") "")
                   (if labels (string "-l '" labels "' ") "")
                   (if field (string "--field-selector=" field " ") "")
                   "-o wide 2>&1"))
  (compact-table (k8s-sh cmd)))

(defn tool-get-logs [args]
  (def json (parse-args args))
  (def pod (get json :pod ""))
  (def container (get json :container ""))
  (def tail (or (get json :tail 100) 100))
  (def previous (get json :previous false))
  (def ns (get json :namespace k8s-namespace))
  (if (empty? pod)
    "error: pod name required"
    (do
      (def cmd (string "kubectl logs " pod
                       (if ns (string " -n " ns) "")
                       (if container (string " -c " container) "")
                       (string " --tail=" tail)
                       (if previous " --previous" "")
                       " 2>&1"))
      (k8s-sh cmd))))

(defn tool-describe [args]
  (def json (parse-args args))
  (def resource (get json :resource ""))
  (def name (get json :name ""))
  (def ns (get json :namespace k8s-namespace))
  (if (empty? resource)
    "error: resource type required (e.g. pod, deployment, service)"
    (do
      (def cmd (string "kubectl describe " resource
                       (if name (string " " name) "")
                       (if ns (string " -n " ns) "")
                       " 2>&1"))
      (k8s-sh cmd))))

(defn tool-get-events [args]
  (def json (parse-args args))
  (def ns (get json :namespace k8s-namespace))
  (def all-ns (get json :all_namespaces (not ns)))
  (def cmd (string "kubectl get events "
                   (if all-ns "--all-namespaces " "")
                   (if (and ns (not all-ns)) (string "-n " ns " ") "")
                   "--sort-by=.lastTimestamp 2>&1 | tail -80"))
  (compact-table (k8s-sh cmd)))

(defn tool-get-nodes [args]
  (def cmd "kubectl get nodes -o wide 2>&1")
  (compact-table (k8s-sh cmd)))

(defn tool-get-services [args]
  (def json (parse-args args))
  (def ns (get json :namespace k8s-namespace))
  (def all-ns (get json :all_namespaces false))
  (def cmd (string "kubectl get services "
                   (if all-ns "--all-namespaces " "")
                   (if (and ns (not all-ns)) (string "-n " ns " ") "")
                   "-o wide 2>&1"))
  (compact-table (k8s-sh cmd)))

(defn tool-get-deployments [args]
  (def json (parse-args args))
  (def ns (get json :namespace k8s-namespace))
  (def all-ns (get json :all_namespaces false))
  (def cmd (string "kubectl get deployments "
                   (if all-ns "--all-namespaces " "")
                   (if (and ns (not all-ns)) (string "-n " ns " ") "")
                   "-o wide 2>&1"))
  (compact-table (k8s-sh cmd)))

(defn tool-top-pods [args]
  (def json (parse-args args))
  (def ns (get json :namespace k8s-namespace))
  (def all-ns (get json :all_namespaces false))
  (def cmd (string "kubectl top pods "
                   (if all-ns "--all-namespaces " "")
                   (if (and ns (not all-ns)) (string "-n " ns " ") "")
                   "2>&1"))
  (def out (k8s-sh cmd))
  (if (string/find "not available" out)
      "error: metrics-server not installed in cluster — try `minikube addons enable metrics-server`"
      (compact-table out)))

(defn tool-api-resources [args]
  (def json (parse-args args))
  (def api-group (get json :api_group ""))
  (def cmd (string "kubectl api-resources "
                   (if api-group (string "--api-group=" api-group " ") "")
                   "2>&1"))
  (compact-table (k8s-sh cmd)))

(defn tool-get-configmaps [args]
  (def json (parse-args args))
  (def ns (get json :namespace k8s-namespace))
  (def name (get json :name ""))
  (if name
    (k8s-sh (string "kubectl get configmap " name (if ns (string " -n " ns) "") " -o yaml 2>&1"))
    (compact-table (k8s-sh (string "kubectl get configmaps " (if ns (string " -n " ns) "") " 2>&1")))))

(defn tool-get-secrets-list [args]
  "List secrets (names only, not values — the LLM can then read individual secrets via bash if needed)."
  (def json (parse-args args))
  (def ns (get json :namespace k8s-namespace))
  (compact-table (k8s-sh (string "kubectl get secrets " (if ns (string " -n " ns) "") " 2>&1"))))

(defn tool-get-ingresses [args]
  (def json (parse-args args))
  (def ns (get json :namespace k8s-namespace))
  (def all-ns (get json :all_namespaces false))
  (def cmd (string "kubectl get ingress "
                   (if all-ns "--all-namespaces " "")
                   (if (and ns (not all-ns)) (string "-n " ns " ") "")
                   "2>&1"))
  (compact-table (k8s-sh cmd)))

# --- register all tools ---

(harness/register-tool
  "k8s_get_pods"
  "List Kubernetes pods. Use to see pod status, restarts, IPs, and node placement. The first step in debugging any k8s issue."
  "K8s Get Pods"
  "{\"type\":\"object\",\"properties\":{\"namespace\":{\"type\":\"string\",\"description\":\"Namespace to query (defaults to current context namespace)\"},\"labels\":{\"type\":\"string\",\"description\":\"Label selector e.g. 'app=nginx'\"},\"field\":{\"type\":\"string\",\"description\":\"Field selector e.g. 'status.phase=Running'\"},\"all_namespaces\":{\"type\":\"boolean\",\"description\":\"Query across all namespaces\"}}}"
  "tool-get-pods")

(harness/register-tool
  "k8s_get_logs"
  "Get logs from a specific pod. Use to diagnose crashes, errors, or unexpected behavior. Supports container selection, tail limit, and --previous for seeing logs from the last terminated container."
  "K8s Get Logs"
  "{\"type\":\"object\",\"properties\":{\"pod\":{\"type\":\"string\",\"description\":\"Pod name (required)\"},\"container\":{\"type\":\"string\",\"description\":\"Container name (if pod has multiple containers)\"},\"tail\":{\"type\":\"integer\",\"description\":\"Number of recent lines to return (default: 100)\"},\"previous\":{\"type\":\"boolean\",\"description\":\"Get logs from the PREVIOUS terminated container — ESSENTIAL for crash-loop debugging\"},\"namespace\":{\"type\":\"string\",\"description\":\"Namespace (defaults to current context namespace)\"}},\"required\":[\"pod\"]}"
  "tool-get-logs")

(harness/register-tool
  "k8s_describe"
  "Describe a Kubernetes resource in detail (pod, deployment, service, node, etc.). Shows events, conditions, resource limits, and configuration. Most informative single command for debugging a specific resource."
  "K8s Describe"
  "{\"type\":\"object\",\"properties\":{\"resource\":{\"type\":\"string\",\"description\":\"Resource type: pod, deployment, service, node, replicaset, ingress, pvc, etc. (required)\"},\"name\":{\"type\":\"string\",\"description\":\"Resource name\"},\"namespace\":{\"type\":\"string\",\"description\":\"Namespace (defaults to current context)\"}},\"required\":[\"resource\"]}"
  "tool-describe")

(harness/register-tool
  "k8s_get_events"
  "Get recent cluster events, sorted by time. Events capture scheduling failures, probe deaths, image pull errors, and OOM kills — often the fastest way to see why a pod is broken."
  "K8s Get Events"
  "{\"type\":\"object\",\"properties\":{\"namespace\":{\"type\":\"string\",\"description\":\"Namespace (defaults to all namespaces if omitted — events are often most useful cluster-wide)\"},\"all_namespaces\":{\"type\":\"boolean\",\"description\":\"Explicitly query all namespaces\"}}}"
  "tool-get-events")

(harness/register-tool
  "k8s_get_nodes"
  "List cluster nodes with status, roles, version, and resource capacity/allocation. Use when pods are stuck Pending to check for resource pressure."
  "K8s Get Nodes"
  "{\"type\":\"object\",\"properties\":{}}"
  "tool-get-nodes")

(harness/register-tool
  "k8s_get_services"
  "List Kubernetes services with type, cluster IP, external IP, and ports. Use to verify service discovery and external access."
  "K8s Get Services"
  "{\"type\":\"object\",\"properties\":{\"namespace\":{\"type\":\"string\",\"description\":\"Namespace (defaults to current context)\"},\"all_namespaces\":{\"type\":\"boolean\",\"description\":\"Query across all namespaces\"}}}"
  "tool-get-services")

(harness/register-tool
  "k8s_get_deployments"
  "List deployments with desired/available/ready replica counts. Use to check rollout status and replica health."
  "K8s Get Deployments"
  "{\"type\":\"object\",\"properties\":{\"namespace\":{\"type\":\"string\",\"description\":\"Namespace (defaults to current context)\"},\"all_namespaces\":{\"type\":\"boolean\",\"description\":\"Query across all namespaces\"}}}"
  "tool-get-deployments")

(harness/register-tool
  "k8s_top_pods"
  "Show CPU and memory usage per pod. Requires metrics-server in the cluster. Use to identify resource hogs or pods approaching their limits."
  "K8s Top Pods"
  "{\"type\":\"object\",\"properties\":{\"namespace\":{\"type\":\"string\",\"description\":\"Namespace (defaults to current context)\"},\"all_namespaces\":{\"type\":\"boolean\",\"description\":\"Query across all namespaces\"}}}"
  "tool-top-pods")

(harness/register-tool
  "k8s_api_resources"
  "List all available API resources on the cluster. Use to discover CRDs, API groups, and available resource types — helpful when you don't know what a cluster supports."
  "K8s API Resources"
  "{\"type\":\"object\",\"properties\":{\"api_group\":{\"type\":\"string\",\"description\":\"Filter to a specific API group (e.g. 'networking.k8s.io')\"}}}"
  "tool-api-resources")

(harness/register-tool
  "k8s_get_configmaps"
  "List ConfigMaps or get a specific one (in YAML). Use to inspect application configuration mounted from ConfigMaps."
  "K8s Get ConfigMaps"
  "{\"type\":\"object\",\"properties\":{\"namespace\":{\"type\":\"string\",\"description\":\"Namespace (defaults to current context)\"},\"name\":{\"type\":\"string\",\"description\":\"Specific ConfigMap name to retrieve in full\"}}}"
  "tool-get-configmaps")

(harness/register-tool
  "k8s_get_secrets_list"
  "List secrets in a namespace (NAMES ONLY — never exposes secret values). Use to verify secrets exist and check their types/ages. To read a secret value, use the bash tool."
  "K8s List Secrets"
  "{\"type\":\"object\",\"properties\":{\"namespace\":{\"type\":\"string\",\"description\":\"Namespace (defaults to current context)\"}}}"
  "tool-get-secrets-list")

(harness/register-tool
  "k8s_get_ingresses"
  "List ingress resources with hosts, TLS, and backend services. Use to debug routing and external access."
  "K8s Get Ingresses"
  "{\"type\":\"object\",\"properties\":{\"namespace\":{\"type\":\"string\",\"description\":\"Namespace (defaults to current context)\"},\"all_namespaces\":{\"type\":\"boolean\",\"description\":\"Query across all namespaces\"}}}"
  "tool-get-ingresses")
