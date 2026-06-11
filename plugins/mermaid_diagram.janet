# Mermaid diagram plugin — generates architecture diagrams as intermediate
# representations before implementation. Based on findings from arXiv
# 2604.00171 (diagrams-as-IR improves generation quality), VisDocSketcher
# 2509.11942 (multi-agent decomposition beats single-agent), and 2605.15184v1
# (inline delivery beats file-based routing).
#
# Flow: /diagram <task> → model generates Mermaid → validate → inject
# blueprint into context → implement phase follows the blueprint.

(def hooks ["on-init" "on-response"])

(var phase :idle)
(var diagram nil)
(var attempt 0)
(def max-retries 3)
(var original-task nil)

# --- Prompts (arXiv 2604.00171: diagrams as intermediate representations) ---

(defn- generation-prompt [task]
  (string
    "ARCHITECTURE PLANNING — produce a Mermaid diagram as an intermediate "
    "representation before any code is written.\n\n"
    "This diagram will serve as the DEFINITIVE architectural blueprint for "
    "implementation. It bridges the gap between the problem statement and "
    "the code — make it complete and internally consistent.\n\n"
    "1. Identify all components/modules involved and their responsibilities\n"
    "2. Map every data and control flow edge between them\n"
    "3. List files to create or modify\n"
    "4. Output the architecture as a Mermaid diagram inside a code fence:\n\n"
    "```mermaid\n"
    "flowchart TD\n"
    "    A[Component] -->|data flow| B[Component]\n"
    "    ...\n"
    "```\n\n"
    "Use flowchart for structural architecture or sequenceDiagram for API flows. "
    "Every node must have a label. Every edge must have a direction. "
    "Keep it focused — at most 20 nodes.\n\n"
    "Task: " task))

(defn- implementation-prompt []
  (string
    "IMPLEMENT the architecture shown in the blueprint above.\n"
    "The diagram is the definitive reference for component structure, "
    "data flow, and file layout. Follow it.\n\n"
    "Use TDD: write failing tests first, then minimal implementation "
    "to make them pass. Refer to the diagram for relationships between "
    "components."))

(defn- retry-feedback [task errors]
  (string
    "Your previous Mermaid diagram had issues:\n"
    (string/join (map (fn [e] (string "- " e)) errors) "\n")
    "\nFix these and regenerate the diagram for:\n" task))

# --- Extraction and validation ---

(defn- extract-mermaid [text]
  (def start (string/find "```mermaid" text))
  (if (nil? start)
    nil
    (let [body-start (+ start 10)
          end (string/find "```" text body-start)]
      (if (nil? end)
        nil
        (string/trim (string/slice text body-start end))))))

(defn- count-char [s ch]
  (var n 0)
  (for i 0 (length s)
    (if (= (string/slice s i (+ i 1)) ch)
      (set n (+ n 1))))
  n)

(defn- validate-diagram [text]
  (var errors @[])

  (def has-flowchart (string/find "flowchart" text))
  (def has-seq (string/find "sequenceDiagram" text))
  (def has-class (string/find "classDiagram" text))
  (def has-state (string/find "stateDiagram" text))
  (def has-er (string/find "erDiagram" text))

  (unless (or has-flowchart has-seq has-class has-state has-er)
    (array/push errors "Missing diagram type header — add e.g. 'flowchart TD' or 'sequenceDiagram'"))

  # Cover the common mermaid connectors across diagram types: flowchart
  # (--> --- -.-> ==>), sequence (->> -->>), and er/class/state, which use
  # `--`-based links. The old check only matched --> / ->> / --- and so
  # falsely rejected valid er/classDiagrams (and flowchart -.-> / ==>).
  (def has-edge (or (string/find "--" text)
                    (string/find "->>" text)
                    (string/find "==>" text)
                    (string/find "-.-" text)))
  (unless has-edge
    (array/push errors "No connections between nodes — add edges like 'A --> B' or 'A->>B'"))

  (def bracket-diff (- (count-char text "[") (count-char text "]")))
  (def paren-diff (- (count-char text "(") (count-char text ")")))
  (def brace-diff (- (count-char text "{") (count-char text "}")))
  (when (not= bracket-diff 0)
    (array/push errors (string "Unbalanced square brackets " bracket-diff " — check node labels")))
  (when (not= paren-diff 0)
    (array/push errors (string "Unbalanced parentheses " paren-diff " — check node labels")))
  (when (not= brace-diff 0)
    (array/push errors (string "Unbalanced braces " brace-diff " — check {decision}/{{hexagon}} nodes")))

  {:valid (empty? errors) :errors errors})

# --- Hooks ---

(defn mermaid_diagram-on-init [ctx]
  (set phase :idle)
  (set diagram nil)
  (set attempt 0)
  nil)

(defn mermaid_diagram-on-response [ctx]
  (if (not= phase :generating)
    nil
    (let [response (or (ctx :response) "")
          mermaid (extract-mermaid response)]
      (if (nil? mermaid)
        (if (< attempt max-retries)
          (do
            (set attempt (+ attempt 1))
            (harness/notify
              (string "No Mermaid diagram found (attempt " attempt "/" max-retries ")")
              :warn)
            (harness/request-prompt
              (string "Your response did not include a Mermaid diagram. "
                      "Output a ```mermaid ... ``` block with a flowchart "
                      "or sequence diagram for: " original-task))
            nil)
          (do
            (set phase :idle)
            (set attempt 0)
            (harness/notify "Failed to get a Mermaid diagram after retries" :error)
            nil))
        (let [result (validate-diagram mermaid)]
          (if (result :valid)
            (do
              (set diagram mermaid)
              (set attempt 0)
              (harness/notify "Mermaid diagram validated — injecting as architecture blueprint" :info)
              # arXiv 2605.15184v1: inline injection beats file-based routing.
              # Inject the diagram into the system prompt so the model sees it
              # as a definitive intermediate representation during implementation.
              (harness/append-system-prompt
                (string
                  "## Architecture Blueprint (definitive intermediate representation)\n\n"
                  "The following Mermaid diagram was generated and validated during "
                  "the architecture planning phase. It describes component structure, "
                  "data flow, and file layout. Use it as the authoritative reference "
                  "during implementation.\n\n"
                  "```mermaid\n" diagram "\n```"))
              # Also add as a visible custom message so it persists in the
              # transcript across the full implementation phase.
              (harness/add-custom-message
                "mermaid_blueprint"
                (string "```mermaid\n" diagram "\n```")
                true)
              (set phase :idle)
              (harness/request-prompt (implementation-prompt))
              nil)
            (if (< attempt max-retries)
              (do
                (set attempt (+ attempt 1))
                (harness/notify
                  (string "Diagram validation failed: " (string/join (result :errors) "; "))
                  :warn)
                (harness/request-prompt (retry-feedback original-task (result :errors)))
                nil)
              (do
                (set phase :idle)
                (set attempt 0)
                (harness/notify "Diagram validation failed after retries" :error)
                nil))))))))

# --- Slash command ---

(defn diagram-handler [args]
  (def task (if (string? args) (string/trim args) ""))
  (if (empty? task)
    (string "Usage: /diagram <task description>\n"
            "Generates a Mermaid architecture diagram as an intermediate "
            "representation (arXiv 2604.00171) before writing code. The "
            "diagram is validated then injected as the implementation blueprint.")
    (do
      (set phase :generating)
      (set attempt 0)
      (set original-task task)
      (harness/request-prompt (generation-prompt task))
      (string "Generating architecture diagram for: " task
              "\n(The diagram will be validated then used as an "
              "implementation blueprint.)"))))

(harness/register-command "diagram" "diagram-handler")
