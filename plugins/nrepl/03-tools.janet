# nREPL LLM-callable tools.
#
# Registers `nrepl_eval` (evaluate Clojure) and `nrepl_connect`
# (open/redirect the connection). Both must exist as TOOLS: the agent
# cannot type a slash command, so a plugin whose only connect path is
# /nrepl-connect leaves the model stuck asking the human to do it.

(defn nrepl-eval-tool-handler [args]
  # args is the raw JSON string from the LLM, e.g. {"code": "(+ 1 2)"}
  (def code (json-extract-string args "code"))
  (if (or (nil? code) (= code ""))
    (string "nrepl_eval error: missing or empty \"code\" in args: " args)
    # Janet `if` takes exactly one else-form, so the multi-step body is
    # wrapped in `(do ...)`.
    (do
      # Not connected? Try to connect from .nrepl-port before giving up.
      # A REPL the agent started itself mid-session appears after on-init
      # has already run, so without this the connection never happens.
      (def connect-err
        (if nrepl-connected
          nil
          (try (do (nrepl-ensure-connected) nil) ([err] err))))
      (if connect-err
        (nrepl-not-connected-message connect-err)
        (try
        (do
          (def result (nrepl-eval code))
          (def out (result :out))
          (def err (result :err))
          (def value (result :result))
          (def ns (result :ns))
          (def repaired (result :repaired))
          (def parts @[])
          (when (not= out "")
            (array/push parts (string ";; stdout:\n" out)))
          (when (not= err "")
            (array/push parts (string ";; stderr:\n" err)))
          (when (not= value "")
            (array/push parts value))
          (when (not= ns "")
            (array/push parts (string ";; current ns: " ns)))
          (when (not= repaired nil)
            (array/push parts (string ";; note: auto-repaired unbalanced delimiters:\n;; " repaired)))
            (if (> (length parts) 0)
              (string/join parts "\n")
              "nil"))
          ([err]
           (string "nrepl_eval error: " err)))))))

(harness/register-tool
  "nrepl_eval"
  (string
    "Evaluate a Clojure expression on the connected nREPL server. "
    "Use this to run Clojure/Script code, inspect vars, run tests, "
    "or explore a Clojure project's runtime state. Connects automatically "
    "to the port in .nrepl-port, so if no REPL is running yet, start one "
    "in the background (it writes .nrepl-port) and call this again. "
    "Returns the evaluation result (value), stdout, stderr, and current ns.")
  "nREPL Eval"
  "{\"type\":\"object\",\"properties\":{\"code\":{\"type\":\"string\",\"description\":\"Clojure expression to evaluate\"}},\"required\":[\"code\"]}"
  "nrepl-eval-tool-handler"
  :parallel)

(defn nrepl-connect-tool-handler [args]
  # Both keys optional: no args at all means "connect from .nrepl-port",
  # which is the common case right after starting a REPL.
  (def host (or (json-extract-scalar args "host") nrepl-host))
  # Scalar, not string: a model writes `{"port": 51208}` at least as
  # often as it quotes it.
  (def port (or (json-extract-scalar args "port") (nrepl-discovered-port)))
  (if (nil? port)
    (string
      "nrepl_connect error: no port given and no .nrepl-port in the project "
      "root. Start an nREPL server first (a Clojure REPL writes that file), "
      "or call nrepl_connect again with an explicit port.")
    (try
      (nrepl-connect host port)
      ([err]
       (string "nrepl_connect error: could not connect to " host ":" port " — " err)))))

(harness/register-tool
  "nrepl_connect"
  (string
    "Connect to an nREPL server so nrepl_eval can run Clojure code. "
    "Call with no arguments to use .nrepl-port from the project root, or "
    "pass host/port to reach a specific server. nrepl_eval already "
    "auto-connects from .nrepl-port, so this is only needed for a "
    "non-default host/port or to reconnect after a server restart.")
  "nREPL Connect"
  (string
    "{\"type\":\"object\",\"properties\":{"
    "\"host\":{\"type\":\"string\",\"description\":\"nREPL host (default 127.0.0.1)\"},"
    "\"port\":{\"type\":\"integer\",\"description\":\"nREPL port (default: read from .nrepl-port)\"}"
    "},\"required\":[]}")
  "nrepl-connect-tool-handler"
  :sequential)
