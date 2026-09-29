(ns fixture.addon-protocol
  "A minimal IAddon protocol for dirge's addon host tests.")

(defprotocol IAddon
  (addon-id [this])
  (initialize! [this config])
  (shutdown! [this])
  (tools [this])
  (hooks [this])
  (health [this]))

(defn addon?
  [x]
  (satisfies? IAddon x))
