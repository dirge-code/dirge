(ns flat.addon
  "An addon whose manifest sits at the repository root, not under resources/."
  (:require [fixture.addon-protocol :as p]))

(defn- version
  [_]
  "flat v1")

(defrecord FlatAddon []
  p/IAddon
  (addon-id [_] "flat")
  (initialize! [_ _]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_]
    [{:name        "flat-version"
      :description "Names the code now loaded"
      :handler     version}])
  (hooks [_]
    {})
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->FlatAddon))
