# K8s debugging plugin — shared state, helpers, and context detection.
#
# Detects minikube/kubectl availability and provides shared utilities
# used by all files in this plugin.

# --- cached detection state ---
(var k8s-available false)
(var k8s-context "minikube")
(var k8s-namespace "")

(defn- sh [command]
  "Run a shell command and return stdout, or nil on failure."
  (def proc (os/spawn ["/bin/sh" "-c" command] :p {:out :pipe}))
  (def buf @"")
  (ev/gather
    (ev/read (proc :out) :all buf)
    (os/proc-wait proc))
  (string/trim buf))

(defn- k8s-sh [command]
  "Run a kubectl command with the current context and namespace."
  (def ctx (if k8s-context (string " --context=" k8s-context) ""))
  (def ns (if (and k8s-namespace (not (string/find "-n " command))
                   (not (string/find "--namespace" command))
                   (not (string/find "--all-namespaces" command)))
            (string " -n " k8s-namespace)
            ""))
  (sh (string command ctx ns)))

(defn k8s-detect []
  "Check for kubectl + minikube context. Returns true if ready."
  (def kubectl (sh "which kubectl 2>/dev/null || echo ''"))
  (when kubectl
    (def ctxs (sh "kubectl config get-contexts -o name 2>/dev/null || echo ''"))
    (when (and ctxs (string/find "minikube" ctxs))
      (set k8s-context "minikube")
      (set k8s-available true))
    (when (not k8s-available)
      (def first-ctx (first (string/split "\n" (or ctxs ""))))
      (when (and first-ctx (not (empty? first-ctx)))
        (set k8s-context first-ctx)
        (set k8s-available true))))
  k8s-available)

(defn k8s-namespace-detect []
  "Detect current namespace from kubectl config."
  (def ns (sh (string "kubectl config view --minify -o jsonpath='{..namespace}' 2>/dev/null"
                      (if k8s-context (string " --context=" k8s-context) ""))))
  (when (and ns (not (empty? ns)) (not (= ns "null")))
    (set k8s-namespace ns)))

# Asset: is a pod in CrashLoopBackOff / Error / ImagePullBackOff?
(defn pod-is-unhealthy? [status]
  (or (string/find "CrashLoopBackOff" status)
      (string/find "Error" status)
      (string/find "ImagePullBackOff" status)
      (string/find "ErrImagePull" status)
      (string/find "OOMKilled" status)
      (string/find "Failed" status)))

# Asset: compact table from kubectl get output.
(defn compact-table [output]
  "Limit kubectl table output to 60 lines for LLM context efficiency."
  (def lines (string/split "\n" output))
  (if (> (length lines) 62)
    (string (string/join (array/slice lines 0 60) "\n")
            "\n... [truncated " (- (length lines) 60) " more rows]")
    output))
