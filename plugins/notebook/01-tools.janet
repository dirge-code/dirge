# The agent-facing notebook tools.
#
# `notebook_eval` evaluates a Janet cell in a persistent VM whose state
# survives across tool calls. That persistence is the entire point: the
# agent accumulates context — parsed data, helper fns, intermediate
# results — the way a person builds up a notebook, instead of re-deriving
# it on every call or writing throwaway scripts to disk.
#
# The kernel is a SECOND Janet VM, separate from the one plugins and hooks
# run on. A runaway cell cannot stall the permission gate, and agent code
# cannot redefine a plugin's functions.

(defn- nb-arg [args key]
  (def v (harness/json-extract args key))
  (if (or (nil? v) (= v "")) nil v))

(defn- nb-session [args]
  (or (nb-arg args "session") "main"))

(defn- nb-render [result repaired session]
  (def parts @[])
  (def out (get result :output ""))
  (when (not= out "")
    (array/push parts out))
  (array/push parts
              (if (get result :ok)
                (string "=> " (get result :value))
                (string "ERROR: " (get result :value))))
  (when repaired
    (array/push parts
                (string "# note: repaired unbalanced delimiters before eval:\n# "
                        repaired)))
  (array/push parts (string "# session: " session))
  (string/join parts "\n"))

(defn notebook-eval-handler [args]
  (def code (nb-arg args "code"))
  (if (nil? code)
    (string "notebook_eval error: missing or empty \"code\" in args: " args)
    (do
      (def session (nb-session args))
      (def fixed (repair-delimiters code))
      (def repaired (if (= fixed code) nil fixed))
      (def result (harness/notebook "eval" session fixed))
      (nb-render result repaired session))))

(defn notebook-reset-handler [args]
  (def session (nb-session args))
  (def scope (or (nb-arg args "scope") "session"))
  (def result
    (if (= scope "kernel")
      (harness/notebook "respawn" session)
      (harness/notebook "reset" session)))
  (if (get result :ok)
    (if (= scope "kernel")
      "Notebook kernel restarted. ALL sessions and notebook/shared are gone."
      (string "Session " session " cleared. Other sessions and notebook/shared are untouched."))
    (string "notebook_reset error: " (get result :value))))

(harness/register-tool
  "notebook_eval"
  (string
    "Evaluate Janet code in a PERSISTENT notebook kernel. State survives "
    "between calls: anything you `def` in one call is still there in the "
    "next one, for the whole session.\n"
    "\n"
    "Use it the way you would use a notebook — build up state in small "
    "steps instead of re-deriving everything each call, and instead of "
    "writing one-off scripts to disk. Load a file once, then query it "
    "across several calls. Keep a running result table. Define a helper "
    "and reuse it.\n"
    "\n"
    "Returns the value of the last form, plus anything the cell printed. "
    "Use `(print ...)` to see intermediate values.\n"
    "\n"
    "State is scoped to `session` (default \"main\"). If you are a subagent "
    "running alongside others, pass your own distinct `session` so your "
    "bindings do not collide with theirs. To share deliberately across "
    "sessions, put values in the `notebook/shared` table — that table is "
    "the only state visible from another session.\n"
    "\n"
    "This is Janet, not Clojure: `#` starts a comment, `(def x 1)` binds, "
    "`(var x 1)`/`(set x 2)` for mutables, `@{}`/`@[]` are mutable "
    "table/array literals. `(harness/json-decode str)` parses JSON into "
    "native data. Long computations are cut off after 20 seconds — use "
    "bash for those, not a cell.")
  "Notebook"
  (string
    "{\"type\":\"object\",\"properties\":{"
    "\"code\":{\"type\":\"string\",\"description\":\"Janet code to evaluate. Multiple forms allowed; the last form's value is returned.\"},"
    "\"session\":{\"type\":\"string\",\"description\":\"State namespace. Defaults to 'main'. Subagents running in parallel should each pass a distinct name.\"}"
    "},\"required\":[\"code\"]}")
  "notebook-eval-handler")

(harness/register-tool
  "notebook_reset"
  (string
    "Clear notebook state when it has become confusing or wrong — a bad "
    "`def` you cannot shadow, or a kernel that stopped responding.\n"
    "\n"
    "scope=\"session\" (default) drops just this session's bindings and "
    "leaves other sessions and `notebook/shared` alone. Prefer it.\n"
    "\n"
    "scope=\"kernel\" restarts the whole VM and destroys ALL notebook "
    "state everywhere. Only use it if cells have stopped returning at all "
    "(for example a cell that ran an external command that hung), because "
    "that is the one failure a session reset cannot fix.")
  "Notebook Reset"
  (string
    "{\"type\":\"object\",\"properties\":{"
    "\"session\":{\"type\":\"string\",\"description\":\"Session to clear. Defaults to 'main'.\"},"
    "\"scope\":{\"type\":\"string\",\"enum\":[\"session\",\"kernel\"],\"description\":\"'session' clears one namespace; 'kernel' restarts the VM and loses everything.\"}"
    "},\"required\":[]}")
  "notebook-reset-handler")
