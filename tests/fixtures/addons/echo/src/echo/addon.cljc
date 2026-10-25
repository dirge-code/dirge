(ns echo.addon
  (:require [fixture.addon-protocol :as p]))

(defn- notify!
  [msg]
  (when-let [f (resolve 'dirge.harness/notify)]
    (f msg :info)))

(defn- count-rows
  [params]
  {:content [{:type "text" :text (str "rows=" (count (:rows params)))}]})

(defn- harness
  [f]
  (resolve (symbol "dirge.harness" f)))

(defn- echo-command
  [{:keys [args]}]
  ((harness "panel") {:op :show :id "echo" :title "Echo" :lines [args]})
  {:text (str "echo: " args)})

(defn- ask-command
  [{:keys [args]}]
  (let [answer ((harness "mcp-call") "fixture" "lookup" {:q args})]
    (if-let [error (:error answer)]
      {:text (str "error: " error)}
      {:text   (apply str (map :text (:content answer)))
       :prompt (str "summarize " args)})))

(defn- run-command
  [{:keys [args]}]
  (let [answer ((harness "call-tool") args {:path "README.md"})]
    {:text (if-let [error (:error answer)]
             (str "error: " error)
             (:ok answer))}))

(defn- shout-command
  [{:keys [args]}]
  {:text (str "shout: " args)})

(defrecord EchoAddon [state]
  p/IAddon
  (addon-id [_] "echo")
  (initialize! [_ config]
    (reset! state config)
    (notify! "echo loaded")
    {:success? true :errors []})
  (shutdown! [_]
    (reset! state nil)
    {:success? true})
  (tools [_]
    [{:name        "count-rows"
      :description "Counts the rows it is handed"
      :inputSchema {:type "object" :properties {:rows {:type "array"}}}
      :handler     count-rows}])
  (hooks [_]
    {:dirge/system-prompt (fn [_] "echo addon active")
     :dirge/commands      {"echo" {:description "Echo the arguments into a panel"
                                   :handler     echo-command}
                           "ask"  {:description "Ask the fixture MCP server"
                                   :handler     ask-command}
                           "run"  {:description "Run a dirge tool on README.md"
                                   :handler     run-command}
                           "/shout" {:description "Registered with a leading slash"
                                     :handler     shout-command}}})
  (health [_]
    {:status (if @state :ok :down)}))

(defn addon-ctor
  [_config]
  (->EchoAddon (atom nil)))
