(ns native.addon
  "An addon implementing hive-addon's IAddon, which dirge embeds, with no
   protocol library on its source roots."
  (:require [hive-addon.protocol :as p]))

(defrecord NativeAddon []
  p/IAddon
  (addon-id [_] "native")
  (initialize! [_ _]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_]
    [{:name        "native-ping"
      :description "Answers pong"
      :handler     (fn [_] "pong")}])
  (hooks [_]
    {})
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->NativeAddon))
