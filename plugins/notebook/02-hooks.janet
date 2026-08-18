# Skill prompt.
#
# The tool existing is not enough to change behaviour: a model's default is
# to treat every tool call as independent and re-establish context each
# time, which is exactly what the notebook exists to stop. The prompt has
# to show the incremental pattern, because that shift is the feature.

(def notebook-skill-prompt
  (string
    "\n## Notebook kernel (notebook_eval)\n\n"
    "`notebook_eval` runs Janet in a kernel whose state PERSISTS between "
    "calls. Treat it as a running workspace, not as a calculator.\n\n"
    "**Build up state instead of re-deriving it.** Load or compute "
    "something once, then work against it over several calls:\n"
    "```\n"
    "# call 1 — load once\n"
    "(def raw (slurp \"data/report.json\"))\n"
    "(def doc (harness/json-decode raw))\n"
    "(length doc)\n"
    "\n"
    "# call 2 — `doc` is still here\n"
    "(def failing (filter (fn [r] (not (get r \"ok\"))) doc))\n"
    "(length failing)\n"
    "\n"
    "# call 3 — and so is `failing`\n"
    "(map (fn [r] (get r \"name\")) (take 5 failing))\n"
    "```\n\n"
    "**Prefer this over one-off scripts.** If you would otherwise write a "
    "throwaway file to compute something, run it in a cell — nothing lands "
    "on disk and the intermediate values stay available.\n\n"
    "**Print to inspect.** Only the last form's value comes back, so use "
    "`(print ...)` for intermediate steps.\n\n"
    "**Keep helpers around.** A function you define stays defined:\n"
    "```\n"
    "(defn summarize [rows] {:n (length rows) :names (map |(get $ \"name\") rows)})\n"
    "```\n\n"
    "**Sessions.** State lives under a `session` name (default \"main\"). "
    "If you are one of several subagents running at once, pass your own "
    "`session` so your bindings do not collide. `notebook/shared` is a "
    "table visible from every session — use it, and only it, to hand "
    "results to another agent.\n\n"
    "**Limits.** A cell is cut off after 20 seconds; run long jobs with "
    "bash instead. The kernel is separate from the plugin VM, so cells "
    "cannot see or change plugin state.\n"))

(defn before-agent-start [ctx]
  (harness/append-system-prompt notebook-skill-prompt)
  nil)
