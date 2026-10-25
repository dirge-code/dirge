(ns layered.addon
  "An addon whose load-time value comes from a namespace it requires, and
   which sorts before that namespace by file name."
  (:require [fixture.addon-protocol :as p]
            [layered.util :as util]))

(def greeting
  (util/greet "reload"))

(defrecord LayeredAddon []
  p/IAddon
  (addon-id [_] "layered")
  (initialize! [_ _]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_]
    [{:name        "greeting"
      :description "The greeting computed when layered.addon loaded"
      :handler     (fn [_] greeting)}])
  (hooks [_]
    {})
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->LayeredAddon))
