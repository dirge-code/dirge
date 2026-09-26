# vigil_lev.janet — lev rite gate for the vigil feature line.
#
# System-1 gate ahead of a vigil observance: before the agent is woken for a
# toll/watcher/harbinger reap, this hook POSTs the coalesced event state to a
# lev sidecar (`POST /v1/systemone`) and asks one yes/no question — "does the
# current state require agent intervention?". If lev's `noul` probability is
# below the configured threshold the observance is blocked via `harness/block`;
# otherwise, or if lev is unreachable, the gate fails open (returns nil).
#
# Config (env, read at call time):
#   LEV_URL       base URL of the lev server, default http://127.0.0.1:8080
#   LEV_API_KEY   optional; sent as `Authorization: Bearer <key>` when set
#   LEV_THRESHOLD block when the noul probability is below this, default 0.8
#
# `lev-verdict` is the testable seam: it returns a block-reason string, or nil
# to pass. It accepts an optional cfg table (:endpoint / :api-key / :threshold)
# so tests can point it at a mock lev without touching process env vars.

(def hooks ["on-vigil-rite"])

(defn- lev-endpoint []
  (or (os/getenv "LEV_URL") "http://127.0.0.1:8080"))

(defn- lev-api-key []
  (os/getenv "LEV_API_KEY"))

(defn- lev-threshold []
  (or (scan-number (or (os/getenv "LEV_THRESHOLD") "0.8")) 0.8))

# Assumes the key holds no `"` or `\` (true of lev's tokens); a pathological
# key would need JSON escaping here.
(defn- lev-headers [api-key]
  (if api-key
    (string "{\"Authorization\":\"Bearer " api-key "\"}")
    ""))

(defn lev-verdict
  "POST the vigil state to lev and return a block reason, or nil to pass.
   `ctx` is the on-vigil-rite context (:vigil :trigger :event_count :payload).
   `cfg` optionally overrides :endpoint / :api-key / :threshold for tests."
  [ctx &opt cfg]
  (let [endpoint (or (get cfg :endpoint) (lev-endpoint))
        api-key (or (get cfg :api-key) (lev-api-key))
        threshold (or (get cfg :threshold) (lev-threshold))
        state (or (ctx :payload) "{}")
        body (string
               "{\"state\":" state
               ",\"questions\":{\"judgment\":{"
               "\"type\":\"noul\","
               "\"instructions\":\"The current state requires agent intervention.\""
               "}}}")
        resp (harness/http-post (string endpoint "/v1/systemone") body (lev-headers api-key))
        decoded (if resp (harness/json-decode resp) nil)
        prob (if decoded (get-in decoded ["answers" "judgment" "noul"]) nil)]
    (if (and (number? prob) (< prob threshold))
        (string "lev rite gate: noul " prob " below threshold " threshold)
        nil)))

(defn on-vigil-rite [ctx]
  (when-let [reason (lev-verdict ctx)]
    (harness/block reason)))
