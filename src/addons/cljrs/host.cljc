(ns dirge.addon.host
  "dirge's addon host inside the embedded cljrs runtime. Rust calls
   use-protocol!, load-addon!, shutdown-addon!, reload-sources!, call-tool,
   run-command, run-hook and shutdown-all! with plain data and reads plain
   data back."
  (:require [clojure.edn :as edn]))

;; SPDX-License-Identifier: GPL-3.0-only

(defn json-safe
  "`x` folded into data a JSON writer can encode."
  [x]
  (cond
    (or (nil? x) (string? x) (boolean? x) (number? x) (keyword? x)) x
    (symbol? x)     (str x)
    (fn? x)         "#fn"
    (map? x)        (into {}
                          (map (fn [[k v]]
                                 [(if (or (string? k) (keyword? k)) k (str k))
                                  (json-safe v)]))
                          x)
    (set? x)        (mapv json-safe (sort-by str x))
    (sequential? x) (mapv json-safe x)
    :else           (str x)))

(def required-fns
  "IAddon functions the protocol namespace must provide."
  '[addon? initialize! shutdown! tools])

(def optional-fns
  "IAddon functions used when the protocol namespace provides them."
  '[hooks health unimplemented-method?])

(def ^:private unimplemented-re
  #"(?i)is abstract|does not define or inherit an implementation|no implementation of (method|protocol)|no protocol method|nothing implements")

(defn- default-unimplemented?
  [t]
  (boolean (some->> (ex-message t) (re-find unimplemented-re))))

(defn failure
  "{:error msg} for a caught throwable."
  [t]
  {:error (or (ex-message t) (str t))})

(defn tool-view
  "What dirge needs of a tool-def: everything but the live :handler."
  [tool]
  (select-keys tool [:name :description :inputSchema]))

(defn hook-names
  "The hook keys an addon registered, as strings without the colon."
  [hooks]
  (vec (sort (map (fn [k] (subs (str k) 1)) (keys hooks)))))

(defn index-tools
  "tool-defs keyed by :name."
  [tools]
  (into {} (map (juxt :name identity)) tools))

(defn command-name
  "The name typed after `/` for a :dirge/commands key: its name without
   leading slashes."
  [k]
  (or (re-find #"[^/].*" (name k)) ""))

(defn command-index
  "The slash commands in a hooks map's :dirge/commands entry, keyed by
   command-name: {\"name\" {:description d :handler f}}. Entries without a
   handler are dropped."
  [hooks]
  (into {}
        (for [[k spec] (get hooks :dirge/commands)
              :when (some? (:handler spec))]
          [(command-name k) {:description (or (:description spec) "")
                             :handler     (:handler spec)}])))

(defn command-views
  "What dirge needs of indexed commands: names and descriptions, sorted."
  [commands]
  (vec (for [[n {:keys [description]}] (sort-by key commands)]
         {:name n :description description})))

(defn- resolve-fns
  [protocol-ns names]
  (into {} (for [n names] [(keyword n) (resolve (symbol protocol-ns (str n)))])))

(defonce ^:private !protocol (atom nil))
(defonce ^:private !addons (atom {}))
(defonce ^:private !order (atom []))

(defn- pf
  [k]
  (get @!protocol k))

(defn use-protocol!
  "Bind the IAddon functions of `protocol-ns`: {:ok protocol-ns} or {:error msg}."
  [protocol-ns]
  (try
    (require (symbol protocol-ns))
    (let [required (resolve-fns protocol-ns required-fns)
          missing  (sort (for [[k v] required :when (nil? v)] (name k)))]
      (if (seq missing)
        {:error (str protocol-ns " does not define " (apply str (interpose ", " missing)))}
        (let [optional (resolve-fns protocol-ns optional-fns)]
          (reset! !protocol (merge required
                                   (into {} (remove (comp nil? val)) optional)
                                   {:unimplemented-method? (or (:unimplemented-method? optional)
                                                               default-unimplemented?)
                                    :protocol-ns           (symbol protocol-ns)}))
          {:ok protocol-ns})))
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(defn- optional
  "Call an optional IAddon method, answering `fallback` when the protocol or
   the addon does not provide it."
  [k addon fallback]
  (if-let [method (pf k)]
    (try
      (method addon)
      (catch #?(:clj Throwable :default :default) t
        (if ((pf :unimplemented-method?) t) fallback (throw t))))
    fallback))

(defn- ctor
  "The manifest's constructor fn, loading its namespace first."
  [{:addon/keys [init-ns init-fn]}]
  (require (symbol (str init-ns)))
  (or (resolve (symbol (str init-ns) (str (or init-fn "addon-ctor"))))
      (throw (ex-info (str "init-fn not found: " init-ns "/" init-fn) {}))))

(defn shutdown-addon!
  "Shut one addon down and forget it. Idempotent."
  [id]
  (when-let [{:keys [addon]} (get @!addons id)]
    (try ((pf :shutdown!) addon) (catch #?(:clj Throwable :default :default) _ nil))
    (swap! !addons dissoc id)
    (swap! !order (fn [ids] (vec (remove #{id} ids))))))

(defn- register!
  "Ask `addon` for its tools, hooks and commands now and keep them under
   `id`: the addon's summary."
  [id manifest addon]
  (let [tools    (vec ((pf :tools) addon))
        hooks    (or (optional :hooks addon {}) {})
        commands (command-index hooks)]
    (swap! !addons assoc id {:addon addon :manifest manifest
                             :tools (index-tools tools) :hooks hooks
                             :commands commands})
    {:id id
     :version (:addon/version manifest)
     :tools (mapv tool-view tools)
     :hooks (hook-names hooks)
     :commands (command-views commands)
     :health (optional :health addon {:status :ok})}))

(defn- install!
  [id manifest addon]
  (let [summary (register! id manifest addon)]
    (swap! !order (fn [ids] (conj (vec (remove #{id} ids)) id)))
    summary))

(defn refresh!
  "Ask every loaded addon again for its tools, hooks and commands, without
   shutting it down or initializing it, so definitions changed at a REPL
   take effect: one summary per addon in load order, or {:id id :error msg}
   for an addon that threw and keeps what it registered before."
  []
  (vec
   (for [id @!order
         :let [{:keys [addon manifest]} (get @!addons id)]]
     (try
       (json-safe (register! id manifest addon))
       (catch #?(:clj Throwable :default :default) t
         (assoc (failure t) :id id))))))

(defn load-addon!
  "Load the manifest at `path`: construct, initialize!, register. A manifest
   already loaded under the same id is shut down first, so this is also
   reload. Returns the addon's summary, or {:error msg}."
  [path host-config]
  (try
    (when-not @!protocol
      (throw (ex-info "no IAddon protocol bound; call use-protocol! first" {})))
    (let [manifest (edn/read-string (slurp path))
          id       (:addon/id manifest)
          config   (:addon/config manifest {})]
      (when-not (string? id)
        (throw (ex-info (str "manifest has no string :addon/id: " path) {})))
      (shutdown-addon! id)
      (let [addon ((ctor manifest) config)]
        (when-not ((pf :addon?) addon)
          (throw (ex-info (str id " constructor did not return an IAddon") {})))
        (let [init ((pf :initialize!) addon {:addon/id id
                                             :addon/config config
                                             :dirge/host host-config})]
          (if (false? (:success? init))
            {:error (str id " failed to initialize: " (pr-str (:errors init)))}
            (json-safe (install! id manifest addon))))))
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(defn call-tool
  "Run `tool-name` of `addon-id` on `params`: {:ok result} or {:error msg}."
  [addon-id tool-name params]
  (try
    (if-let [tool (get-in @!addons [addon-id :tools tool-name])]
      {:ok (json-safe ((:handler tool) params))}
      {:error (str "no tool " tool-name " in addon " addon-id)})
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(defn run-command
  "Run slash command `command` of `addon-id` with `ctx`: {:ok answer} or
   {:error msg}."
  [addon-id command ctx]
  (try
    (if-let [{:keys [handler]} (get-in @!addons [addon-id :commands command])]
      {:ok (json-safe (handler ctx))}
      {:error (str "no command " command " in addon " addon-id)})
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(defn run-hook-handler
  "Run command-hook `handler` of `addon-id`, the fn under that name in its
   :dirge/command-hooks entry, with `ctx` ({:payload hook-json}): {:ok answer}
   or {:error msg}. A string handler name also finds a keyword key."
  [addon-id handler ctx]
  (try
    (let [handlers (get-in @!addons [addon-id :hooks :dirge/command-hooks])]
      (if-let [f (or (get handlers handler) (get handlers (keyword handler)))]
        {:ok (json-safe (f ctx))}
        {:error (str "no command hook " handler " in addon " addon-id)}))
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(def hook-keyword-fields
  "Context fields a hook reads as keywords, by hook key. They reach the host
   as strings."
  {:dirge/session-end [:reason]
   :dirge/event       [:event]})

(defn hook-ctx
  "`ctx` as the hook keyed `k` reads it."
  [k ctx]
  (reduce (fn [c field]
            (cond-> c (string? (get c field)) (update field keyword)))
          ctx
          (get hook-keyword-fields k)))

(defn run-hook
  "Call every loaded addon's `hook-key` hook with `ctx`, in load order:
   a vector of {:addon id :ok result} / {:addon id :error msg}."
  [hook-key ctx]
  (let [k   (keyword hook-key)
        ctx (hook-ctx k ctx)]
    (vec
     (for [id @!order
           :let [f (get-in @!addons [id :hooks k])]
           :when f]
       (try
         {:addon id :ok (json-safe (f ctx))}
         (catch #?(:clj Throwable :default :default) t
           (assoc (failure t) :addon id)))))))

(defn- load-source!
  "load-file `path`, leaving *ns* where it was: nil, or {:error msg}."
  [path]
  (let [saved  (ns-name *ns*)
        result (try
                 (load-file path)
                 nil
                 (catch #?(:clj Throwable :default :default) t
                   (failure t)))]
    (in-ns saved)
    result))

(defn- require-targets
  "The namespaces one :require or :use spec names: a symbol, a vector
   headed by one, or a prefix list."
  [spec]
  (cond
    (symbol? spec) [spec]
    (vector? spec) (let [lib (first spec)] (when (symbol? lib) [lib]))
    (seq? spec)    (let [prefix (first spec)]
                     (for [lib  (rest spec)
                           :let [lib (if (vector? lib) (first lib) lib)]
                           :when (symbol? lib)]
                       (symbol (str prefix "." lib))))
    :else          nil))

(defn ns-deps
  "What an ns form declares: {:ns name :requires #{name}}, :requires being
   the namespaces its :require and :use clauses name. nil for any other
   form."
  [form]
  (when (and (seq? form) (= 'ns (first form)) (symbol? (second form)))
    {:ns       (second form)
     :requires (set (for [clause (drop 2 form)
                          :when  (and (seq? clause)
                                      (contains? #{:require :use} (first clause)))
                          spec   (rest clause)
                          lib    (require-targets spec)]
                      lib))}))

(defn load-order
  "`sources` ({:ns name :requires #{name}}) ordered so each follows the
   sources it requires, ties in input order: {:order [source] :cycle
   [source]}. :cycle holds, in input order, the sources no order can place:
   those in a require cycle and those requiring one."
  [sources]
  (let [known (set (map :ns sources))]
    (loop [order [] pending (vec sources)]
      (let [placed (set (map :ns order))
            ready? (fn [{:keys [ns requires]}]
                     (every? #(or (= % ns) (contains? placed %) (not (contains? known %)))
                             requires))
            ready  (filterv ready? pending)]
        (if (empty? ready)
          {:order order :cycle pending}
          (recur (into order ready) (vec (remove ready? pending))))))))

(defn- first-form
  "The first form of the source at `path`, or nil when it cannot be read."
  [path]
  (try
    (read-string (slurp path))
    (catch #?(:clj Throwable :default :default) _ nil)))

(defn- source-info
  "`source` ({:file path :ns name-or-nil}) as reloading reads it: :ns the
   namespace its ns form declares, else the one its path names; :by-path
   the latter; :requires what its ns form requires."
  [{:keys [file ns]}]
  (let [by-path  (when ns (symbol ns))
        declared (ns-deps (first-form file))]
    {:file     file
     :ns       (or (:ns declared) by-path)
     :by-path  by-path
     :requires (or (:requires declared) #{})}))

(defn- loaded?
  [ns]
  (boolean (and ns (find-ns ns))))

(defn- evaluate!
  "load-file each of `files` in order, retrying the ones that fail while a
   pass makes progress, since one may need a definition a later file adds.
   Answers [{:file path :error msg}] for those still failing."
  [files]
  (loop [pending (vec files)]
    (let [failed (vec (keep (fn [f]
                              (when-let [e (load-source! f)]
                                (assoc e :file f)))
                            pending))]
      (if (or (empty? failed) (= (count failed) (count pending)))
        failed
        (recur (mapv :file failed))))))

(defn- unloaded-row
  "Why `source`, whose namespace nothing has loaded, was not evaluated:
   {:file :error} when `require` could never load it from its path, else
   {:file :skipped}."
  [{:keys [file ns by-path]}]
  (cond
    (nil? ns)
    {:file file :error "not reloaded: no ns form, and outside every source root"}

    (and by-path (not= ns by-path))
    {:file file :error (str "not reloaded: declares " ns " but its path names " by-path)}

    :else
    {:file file :skipped (str "not reloaded: nothing has loaded " ns)}))

(defn reload-sources!
  "Evaluate again every source in `sources` ({:file path :ns name-or-nil})
   whose namespace is loaded, each after the sources it requires, so it runs
   the code now on disk. A source's namespace is the one its ns form
   declares, else `:ns`. The bound IAddon protocol namespace is never
   evaluated again. Answers one row per source not brought up to date:
   {:file path :error msg} for a load failure, a require cycle, or a file
   `require` could never load; {:file path :skipped msg} for the protocol
   namespace and for namespaces nothing has loaded."
  [sources]
  (let [infos                 (mapv source-info sources)
        protocol-ns           (pf :protocol-ns)
        protocol?             (fn [s] (and (some? protocol-ns) (= (:ns s) protocol-ns)))
        live                  (filterv #(and (loaded? (:ns %)) (not (protocol? %))) infos)
        {:keys [order cycle]} (load-order live)
        failed                (evaluate! (map :file order))
        cyclic                (apply str (interpose ", " (sort (distinct (map (comp str :ns) cycle)))))
        dormant               (filterv #(not (or (protocol? %) (loaded? (:ns %)))) infos)]
    (vec (concat
          failed
          (for [{:keys [file]} cycle]
            {:file file :error (str "not reloaded: require cycle among " cyclic)})
          (for [{:keys [file]} (filter protocol? infos)]
            {:file file :skipped (str "not reloaded: " protocol-ns " is the IAddon protocol namespace")})
          (map unloaded-row dormant)))))

(defn shutdown-all!
  "Shut every addon down, newest first."
  []
  (doseq [id (reverse @!order)]
    (shutdown-addon! id))
  true)
