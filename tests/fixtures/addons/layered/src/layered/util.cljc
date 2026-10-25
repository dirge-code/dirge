(ns layered.util
  "A namespace layered.addon requires and reads at load time.")

(defn greet
  [who]
  (str "hello " who))
