# K8s debugging plugin — safety gate for destructive operations.
#
# Intercepts bash commands that would mutate or destroy cluster resources
# and asks for confirmation. Protects against:
#   - kubectl delete (pods, deployments, services, namespaces, etc.)
#   - helm uninstall / helm delete
#   - kubectl drain / cordon / uncordon
#   - minikube delete / stop
#
# Pattern: first-wins harness/block — if the user denies, the command
# never runs.

(def destructive-patterns
  ["kubectl delete"
   "helm uninstall"
   "helm delete"
   "kubectl drain"
   "kubectl cordon"
   "kubectl uncordon"
   "kubectl taint"
   "minikube delete"
   "minikube stop"])

(defn- extract-command [args-json]
  (when args-json
    (def marker "\"command\"")
    (def mp (string/find marker args-json))
    (when mp
      (def after (string/slice args-json (+ mp (length marker))))
      (def q1 (string/find "\"" after))
      (when q1
        (def rest (string/slice after (+ q1 1)))
        (def q2 (string/find "\"" rest))
        (when q2 (string/slice rest 0 q2))))))

(defn- destructive? [command]
  (var hit false)
  (loop [p :in destructive-patterns]
    (when (string/find p command) (set hit true)))
  hit)

(defn on-tool-start [ctx]
  (when (= (ctx :tool) "bash")
    (def cmd (extract-command (ctx :args)))
    (when (and cmd (destructive? cmd))
      (def ok (harness/confirm
                "destructive k8s operation"
                (string "run `" cmd "`?")))
      (when (not ok)
        (harness/block "user denied destructive k8s command")))))
