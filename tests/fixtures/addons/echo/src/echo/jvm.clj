(ns echo.jvm)

(defn addon-ctor
  [_config]
  (throw (ex-info "JVM-only addon must never load in dirge" {})))
