(ns live.addon
  "An addon whose tools and hooks change while it runs: `grow` adds a tool
   and asks dirge to re-read it, `:dirge/event` keeps what it heard, and
   `:acme/ping` is a hook no dirge hook point names."
  (:require [fixture.addon-protocol :as p]))

(defonce !extra (atom []))
(defonce !heard (atom []))

(defn- harness
  [f]
  (resolve (symbol "dirge.harness" f)))

(defn- text
  [s]
  {:content [{:type "text" :text s}]})

(defn- grow
  [{:keys [name]}]
  (swap! !extra conj name)
  ((harness "refresh!"))
  (text (str "grew " name)))

(defn- heard
  [_]
  (text (apply str (interpose "," (map (fn [e] (clojure.core/name (:event e))) @!heard)))))

(defn- extra-tool
  [n]
  {:name n
   :description (str "added at runtime: " n)
   :inputSchema {:type "object"}
   :handler (fn [_] (text (str "hello from " n)))})

(defrecord LiveAddon []
  p/IAddon
  (addon-id [_] "live")
  (initialize! [_ _config]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_]
    (into [{:name "grow"
            :description "Adds a tool named `name`"
            :inputSchema {:type "object" :properties {:name {:type "string"}}}
            :handler grow}
           {:name "heard"
            :description "The events heard so far"
            :inputSchema {:type "object"}
            :handler heard}]
          (map extra-tool @!extra)))
  (hooks [_]
    {:dirge/event (fn [ctx] (swap! !heard conj ctx) nil)
     :acme/ping   (fn [{:keys [n]}] (str "pong " n))})
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->LiveAddon))
