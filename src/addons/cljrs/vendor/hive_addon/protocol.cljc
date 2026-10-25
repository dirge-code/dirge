(ns hive-addon.protocol
  "IAddon protocol — THE single source of truth for addon contracts.

   A host loads addons that implement this protocol to gain capabilities
   (DIP): the host depends on this abstraction, never the concrete addons,
   and no addon compile-depends on the host. Any project — an MCP server or
   something unrelated — can host addons by consuming this leaf lib.

   All addons implement this protocol. A host's registry operates on
   instances via this abstraction barrier.

   Addon types:
   - :native      — Built-in Clojure addons (same JVM)
   - :mcp-bridge  — External MCP servers proxied via stdio/sse/http
   - :external    — Non-MCP integrations (REST APIs, CLIs, etc.)

   Lifecycle:
     (initialize! addon config) → Start services, open connections
     (shutdown! addon)          → Release resources, close connections
     (health addon)             → Query current health status")

;; Copyright (C) 2026 Pedro Gomes Branquinho (BuddhiLW) <pedrogbranquinho@gmail.com>
;;
;; SPDX-License-Identifier: MIT

;; =============================================================================
;; IAddon Protocol (Multiplexer)
;; =============================================================================

(defprotocol IAddon
  "Protocol for host Multiplexer addons.

   Implementors provide identity, lifecycle, tools, schema extensions,
   and health reporting. Designed for composability — each addon is an
   isolated unit that contributes capabilities to the host."

  (addon-id [this]
    "Return the unique string identifier for this addon.

     Convention: reverse-domain or namespaced keyword-style string.
     Examples: \"hive.memory\", \"bridge.haystack\", \"ext.github\"

     Must be stable across restarts — used as registry key and
     in DataScript entity references.")

  (addon-type [this]
    "Return the addon type keyword.

     One of:
       :native     — Clojure code running in the same JVM.
                     Direct function calls, no serialization overhead.
       :mcp-bridge — External MCP server proxied via transport.
                     JSON-RPC over stdio/sse/http.
       :external   — Non-MCP external integration.
                     REST APIs, CLI tools, custom protocols.

     Used by the host for routing, health-check strategies,
     and resource budgeting.")

  (capabilities [this]
    "Return a set of capability keywords this addon provides.

     Standard capabilities:
       :tools             — Contributes MCP tool definitions
       :schema            — Contributes DataScript schema extensions
       :resources         — Contributes MCP resource definitions
       :prompts           — Contributes MCP prompt templates
       :mcp-bridge        — Can proxy to external MCP servers
       :health-reporting  — Supports detailed health checks

     Custom capabilities are allowed (e.g. :vector-search, :llm-routing).
     The host uses capabilities for routing and discovery.")

  (initialize! [this config]
    "Initialize the addon with the given configuration map.

     Called once during addon registration. Config contents vary by addon
     type but always include at minimum:
       {:addon/id     \"...\"    ;; Echoed from manifest
        :addon/config {...}}    ;; Addon-specific config from manifest

     Native addons may receive additional runtime context:
       {:runtime/conn   <DataScript-conn>
        :runtime/event-bus <event-bus>}

     MCP bridge addons receive transport config:
       {:transport/type :stdio
        :transport/command [\"python\" \"-m\" \"server\"]
        :transport/env {\"API_KEY\" \"...\"}}

     Returns result map:
       {:success? true/false
        :errors   [\"...\"]       ;; Empty on success
        :metadata {...}}         ;; Optional addon-specific metadata

     Must be idempotent — calling on an already-initialized addon
     should return {:success? true :already-initialized? true}.")

  (shutdown! [this]
    "Shutdown the addon and release all resources.

     Called during addon deregistration or system shutdown.
     Must be idempotent — safe to call on already-shutdown addon.

     Should:
     - Close connections/processes
     - Deregister any extension points
     - Cancel background tasks

     Returns nil.")

  (tools [this]
    "Return a sequence of MCP tool definitions contributed by this addon.

     Each tool-def is a map:
       {:name        \"addon_tool_name\"
        :description \"What the tool does\"
        :inputSchema {:type \"object\"
                      :properties {\"param\" {:type \"string\"
                                             :description \"...\"}}
                      :required [\"param\"]}
        :handler     (fn [params] -> result-map)}

     Tools are namespaced by the host at registration time
     (e.g. \"haystack:convert\") — return unnamespaced names here.

     Returns empty seq if addon contributes no tools.")

  (schema-extensions [this]
    "Return a sequence of DataScript schema attribute definitions.

     Each schema-def is a map suitable for DataScript schema merge:
       {:addon.haystack/doc-id {:db/valueType :db.type/string
                                :db/cardinality :db.cardinality/one
                                :db/doc \"Haystack document ID\"}}

     Attributes should be namespaced with the addon's prefix to avoid
     collisions (e.g. :addon.haystack/*, :addon.github/*).

     The host merges these into the DataScript schema at
     initialization time.

     Returns empty seq if addon contributes no schema extensions.")

  (health [this]
    "Return the current health status of the addon.

     Returns a map:
       {:status  :ok|:degraded|:down
        :details {...}}   ;; Optional diagnostic info

     Status semantics:
       :ok       — Fully functional, all capabilities available
       :degraded — Partially functional, some capabilities impaired
       :down     — Not functional, no capabilities available

     Details may include transport-specific info:
       {:latency-ms 42
        :last-error \"connection reset\"
        :uptime-ms  360000
        :remote-tool-count 5}

     Called by the host health aggregator and exposed
     via the health MCP tool.")

  (excluded-tools [this]
    "Return a set of tool name strings this addon supersedes from other addons.

     When addon A declares #{\"read_file\"}, tools named \"read_file\" from all
     OTHER active addons are filtered out of tools/list. Addon A's own
     tool with that name remains visible.

     This enables transparent capability escalation: a smarter addon can
     overshadow a basic addon's tools by registering the same name and
     declaring the exclusion.

     Returns #{} (empty set) if no exclusions are needed.

     Legacy addons that don't implement this method are handled gracefully
     by the registry (defaults to #{}).")

  (hooks [this]
    "Return a map of hook-key → hook-fn registered by this addon into the
     host extension registry.

     Hook keys are namespaced keywords matching the host ext-key surface.
     Examples: :cu/a, :catchup/wrap, :gx/score, :sk/seed!, :ag/run.

     The addon registry walks this map after `initialize!` succeeds and
     registers each entry. On `shutdown!`, the registry deregisters every
     hook this addon registered (ownership tracked per-addon, so addons
     never clobber each other).

     This is the canonical way for new addons to contribute extensions
     — it replaces ad-hoc registration inside `initialize!`, making the
     contribution surface declarative and lifecycle-bound.

     Returns {} (empty map) if the addon contributes no hooks.

     Legacy addons that don't implement this method are handled
     gracefully by the registry (defaults to {})."))

;; =============================================================================
;; Predicates
;; =============================================================================

(defn addon?
  "Check if object implements the IAddon (multiplexer) protocol."
  [x]
  (satisfies? IAddon x))

(def valid-addon-types
  "Set of valid addon type keywords."
  #{:native :mcp-bridge :external})

(defn valid-addon-type?
  "Check if keyword is a valid addon type."
  [t]
  (contains? valid-addon-types t))

(def unimplemented-method-re
  "Messages the hosts use for a protocol method that is not implemented.

   Measured 2026-08-22. The JVM alone has TWO phrasings, which is why matching
   one measured example is not enough, and the suite caught the second:
     JVM defrecord \"Method p.Partial.b()Ljava/lang/Object; is abstract\"
     JVM reify     \"Receiver class ...$reify__8710 does not define or inherit an
                    implementation of the resolved method 'abstract java.lang.Object
                    excluded_tools()' of interface hive_addon.protocol.IAddon.\"
     JVM extend-*  \"No implementation of method: :b of protocol: ...\"
     cljw          \"No implementation of method 'b' on protocol 'IThing' for type 'Partial'\"
     cljrs         \"runtime error: No implementation of protocol IThing for type Partial\"
   cljs contributes \"no protocol method\" / \"nothing implements\".

   Matching the message rather than the exception CLASS is what makes this
   portable: the class name differs per host and `AbstractMethodError` cannot
   even be named off the JVM.

   It lives HERE, beside the protocol whose optional methods it is about, so
   that the two callers share one definition. hive-addon.schema needs it to
   audit an addon against the contract; hive-addon.opaque.serve needs it to let
   a kernel omit `excluded-tools`/`hooks` the same way the registry does, and
   serve cannot reach schema (it is malli-free, so a cljw-built kernel carries
   no schema runtime)."
  #"(?i)is abstract|does not define or inherit an implementation|no implementation of (method|protocol)|no protocol method|nothing implements")

(defn unimplemented-method?
  "True when `t` is the error a host raises for an IAddon method the addon does
   not implement, on any of the five hosts.

   NOTE: a genuine \"is abstract\" / \"no implementation\" error raised from INSIDE
   an implemented method body also reads as unimplemented here. That is an
   accepted edge: the alternative is naming exception classes, which is not
   portable at all."
  [t]
  (boolean (some->> (ex-message t) (re-find unimplemented-method-re))))

;; =============================================================================
;; Standard Capabilities
;; =============================================================================

(def standard-capabilities
  "Set of standard capability keywords recognized by the host.
   Addons may also declare custom capabilities beyond this set."
  #{:tools :schema :resources :prompts :mcp-bridge :health-reporting :terminal})

;; =============================================================================
;; Health Status Constants
;; =============================================================================

(def health-statuses
  "Valid health status keywords."
  #{:ok :degraded :down})

(defn healthy?
  "Check if addon health status is :ok."
  [health-map]
  (= :ok (:status health-map)))

(defn degraded?
  "Check if addon health status is :degraded."
  [health-map]
  (= :degraded (:status health-map)))

(defn down?
  "Check if addon health status is :down."
  [health-map]
  (= :down (:status health-map)))
