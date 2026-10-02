(ns foreign.addon
  "An addon written for another host: its manifest sits under that host's
   META-INF directory and names that host's protocol."
  (:require [fixture.addon-protocol :as p]))

(defrecord ForeignAddon []
  p/IAddon
  (addon-id [_] "foreign")
  (initialize! [_ _]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_]
    [{:name        "foreign-ping"
      :description "Answers pong"
      :handler     (fn [_] "pong")}])
  (hooks [_]
    {})
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->ForeignAddon))
