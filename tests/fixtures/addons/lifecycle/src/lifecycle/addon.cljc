(ns lifecycle.addon
  (:require [fixture.addon-protocol :as p]))

(defn- harness
  [f]
  (resolve (symbol "dirge.harness" f)))

(defn- lookup
  "The fixture MCP server's answer to `q`, or why the call was refused."
  [q]
  (let [answer ((harness "mcp-call") "fixture" "lookup" {:q q})]
    (or (:error answer)
        (apply str (map :text (:content answer))))))

(defn- session-start
  [{:keys [first-prompt? mcp-servers]}]
  {:context (str "start " (lookup "session")
                 " first=" first-prompt?
                 " servers=" (apply str mcp-servers))})

(defn- session-end
  [{:keys [reason]}]
  (lookup (if (keyword? reason)
            (str "end " (name reason))
            (str "end reason not a keyword: " (pr-str reason)))))

(defn- on-prompt
  [{:keys [first-prompt?]}]
  (str "prompt " (lookup "prompt") " first=" first-prompt?))

(defrecord LifecycleAddon []
  p/IAddon
  (addon-id [_] "lifecycle")
  (initialize! [_ _config]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_] [])
  (hooks [_]
    {:dirge/session-start session-start
     :dirge/session-end   session-end
     :dirge/system-prompt (fn [_] (str "system " (lookup "system")))
     :dirge/on-prompt     on-prompt})
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->LifecycleAddon))
