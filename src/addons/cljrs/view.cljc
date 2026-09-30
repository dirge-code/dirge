(ns dirge.view
  "dirge's view state as a pure reducer: the swarm grid and the /swarm,
   /panel and /display commands. Rust sends plain-data events to
   `dispatch!` on the view isolate and reads back {:model :effects}; the
   UI loop never waits on it. `dirge::ui::view::native` is the same
   reducer in Rust; the parity test keeps the two answering alike.

   What the view owns is registered as data: `events` (event type ->
   handler), `commands` (slash command -> handler), `grid-keymap` (key ->
   verb), `grid-verbs`, `moves` and `cell-actions` (cell kind -> verb ->
   handler). The model's :view_commands and :grid_keys are derived from
   them, so a new command, event kind, key, verb or kind of cell is one
   more entry.

   A grid cell is {:kind \"panel\"|\"agent\" :id id}; the selection is
   kept by cell, so a producer refocus or a finishing sibling does not
   move it."
  (:require [clojure.string :as str]))

;; SPDX-License-Identifier: GPL-3.0-only

;; ---------------------------------------------------------------------------
;; Effects

(defn notify
  [level text]
  {:op :notify :level level :text text})

(defn reply
  ([action] {:op :reply :action action})
  ([action target] {:op :reply :action action :target target}))

;; ---------------------------------------------------------------------------
;; /swarm

(def swarm-usage "usage: /swarm [on|off]")

(def swarm-words
  {"" :toggle "toggle" :toggle
   "on" :open "open" :open "show" :open
   "off" :close "close" :close "hide" :close})

(defn parse-swarm
  "[:ok :toggle|:open|:close] or [:err message]."
  [args]
  (cond
    (empty? args) [:ok :toggle]
    (= 1 (count args))
    (let [a (str/trim (first args))]
      (if-let [cmd (get swarm-words a)]
        [:ok cmd]
        [:err (str "unknown /swarm argument '" a "' (" swarm-usage ")")]))
    :else [:err (str "/swarm takes at most one argument (" swarm-usage ")")]))

(defn swarm-cmd
  [state args]
  (let [[tag v] (parse-swarm args)
        open?   (some? (:swarm state))
        want?   (if (= v :toggle) (not open?) (= v :open))]
    (cond
      (= tag :err)      [state [(notify :error v)]]
      (and want? open?) [state []]
      want?             [(assoc state :swarm {:selected nil}) []]
      :else             [(assoc state :swarm nil)
                         [(notify :info "swarm grid closed")]])))

;; ---------------------------------------------------------------------------
;; /panel

(def reply-usage "usage: /panel next|prev|refresh|unfocus|focus <id>")

(def reply-verbs
  {"next" "next-tab" "next-tab" "next-tab"
   "prev" "prev-tab" "prev-tab" "prev-tab"
   "refresh" "refresh" "unfocus" "unfocus"})

(def panel-modes
  "Mode word -> which side panels it sets."
  {"on" :both "off" :both "auto" :both "debug" :right})

(defn parse-reply
  "[:ok reply-effect] or [:err message]."
  [args]
  (let [verb (if (seq args) (str/trim (first args)) "")
        more (vec (rest args))]
    (cond
      (= verb "focus")
      (cond
        (and (= 1 (count more)) (not (str/blank? (first more))))
        [:ok (reply "focus" (str/trim (first more)))]
        (empty? more) [:err (str "/panel focus needs an item id (" reply-usage ")")]
        :else         [:err (str "/panel focus takes one id (" reply-usage ")")])

      (contains? reply-verbs verb)
      (if (empty? more)
        [:ok (reply (get reply-verbs verb))]
        [:err (str "/panel " verb " takes no argument (" reply-usage ")")])

      (= verb "") [:err reply-usage]
      :else       [:err (str "unknown /panel action '" verb "' (" reply-usage ")")])))

(defn panel-cmd
  [state args]
  (let [arg    (if (seq args) (str/trim (first args)) "")
        status {:op :panel-status}]
    (cond
      (= arg "") [state [status]]

      (contains? panel-modes arg)
      [state [{:op :panel-mode :scope (get panel-modes arg) :mode arg} status]]

      :else
      (let [[tag v] (parse-reply args)]
        (if (= tag :ok)
          [state [v (notify :info (str "panel reply '" (:action v) "' requested"))]]
          [state [(notify :error (str v " (display modes: on|off|auto|debug)"))]])))))

;; ---------------------------------------------------------------------------
;; /display

(def display-usage
  "usage: /display <panes> where panes are left|main|right (e.g. /display main|right)")

(def panes
  "Pane word -> what it turns on. `main` always shows."
  {"left" {:left true} "right" {:right true} "main" {}})

(defn parse-display
  "[:ok {:left bool :right bool}] or [:err message]. Panes are separated
   by |, comma or whitespace, any case."
  [spec]
  (let [toks (map str/lower-case (remove empty? (str/split spec #"[|, \t]")))]
    (if (empty? toks)
      [:err display-usage]
      (loop [toks toks
             vis  {:left false :right false}]
        (cond
          (empty? toks) [:ok vis]
          (contains? panes (first toks)) (recur (rest toks) (merge vis (get panes (first toks))))
          :else [:err (str "unknown pane '" (first toks)
                           "' (use left, main, and/or right, e.g. /display left|main|right)")])))))

(defn shown
  [vis]
  (str/join "|" (concat (when (:left vis) ["left"]) ["main"] (when (:right vis) ["right"]))))

(defn display-cmd
  [state args]
  (let [spec (str/join " " args)]
    (if (str/blank? spec)
      [state [{:op :display-status}]]
      (let [[tag v] (parse-display spec)]
        (if (= tag :ok)
          [state [{:op :panes :left (:left v) :right (:right v)}
                  (notify :info (str "display: " (shown v)))]]
          [state [(notify :error v)]])))))

;; ---------------------------------------------------------------------------
;; Grid keys

(defn index-of
  [cells cell]
  (loop [i 0]
    (cond
      (>= i (count cells)) nil
      (= (nth cells i) cell) i
      :else                  (recur (inc i)))))

(defn grid-of
  "The grid as a key sees it: cells in paint order, the cursor (the
   selected cell, else the first), the column count."
  [state event]
  (let [cells (vec (:cells event))
        n     (count cells)]
    {:cells  cells
     :n      n
     :cur    (or (index-of cells (:selected (:swarm state))) 0)
     :cols   (max (or (:columns event) 1) 1)
     :last-i (max (dec n) 0)}))

(defn select-at
  [state grid i]
  (if (zero? (:n grid))
    state
    (assoc state :swarm {:selected (nth (:cells grid) (min i (:last-i grid)))})))

(def moves
  "Cursor move -> the index it lands on."
  {:left  (fn [g] (max (dec (:cur g)) 0))
   :right (fn [g] (min (inc (:cur g)) (:last-i g)))
   :up    (fn [g] (max (- (:cur g) (:cols g)) 0))
   :down  (fn [g] (if (<= (+ (:cur g) (:cols g)) (:last-i g))
                    (+ (:cur g) (:cols g))
                    (:cur g)))
   :home  (fn [_] 0)
   :end   (fn [g] (:last-i g))})

(def cell-actions
  "Cell kind -> verb -> (fn [state id] [state' effects]). Opening or
   messaging a subagent leaves the grid, so it closes it. A verb a kind
   does not list means nothing on that kind of cell."
  {"panel" {:focus   (fn [state id] [state [(reply "focus" id)]])}
   "agent" {:focus   (fn [state id] [(assoc state :swarm nil) [{:op :open-agent :id id}]])
            :message (fn [state id] [(assoc state :swarm nil) [{:op :message-agent :id id}]])}})

(defn on-cell
  "A grid verb that acts on the selected cell through `cell-actions`."
  [verb]
  (fn [state g _]
    (let [cell (get (:cells g) (:cur g))
          f    (get-in cell-actions [(:kind cell) verb])]
      (if f
        (f state (:id cell))
        [state []]))))

(def grid-verbs
  "Verb -> (fn [state grid arg] [state' effects])."
  {:close   (fn [state _ _] [(assoc state :swarm nil) []])
   :reply   (fn [state _ action] [state [(reply action)]])
   :focus   (on-cell :focus)
   :message (on-cell :message)
   :move    (fn [state g dir] [(select-at state g ((get moves dir) g)) []])
   :nth     (fn [state g i] [(if (< i (:n g)) (select-at state g i) state) []])})

(def grid-keymap
  "Key name -> [verb arg] while the grid is open."
  {"Esc" [:close] "q" [:close]
   "Tab" [:reply "next-tab"] "BackTab" [:reply "prev-tab"]
   "r" [:reply "refresh"] "u" [:reply "unfocus"]
   "Enter" [:focus] "m" [:message]
   "Left" [:move :left] "h" [:move :left]
   "Right" [:move :right] "l" [:move :right]
   "Up" [:move :up] "k" [:move :up]
   "Down" [:move :down] "j" [:move :down]
   "Home" [:move :home] "End" [:move :end]
   "1" [:nth 0] "2" [:nth 1] "3" [:nth 2] "4" [:nth 3] "5" [:nth 4]
   "6" [:nth 5] "7" [:nth 6] "8" [:nth 7] "9" [:nth 8]})

(defn grid-step
  [state event]
  (let [[verb arg] (get grid-keymap (:key event))
        f          (get grid-verbs verb)]
    (if (or (nil? (:swarm state)) (nil? f))
      [state []]
      (f state (grid-of state event) arg))))

;; ---------------------------------------------------------------------------
;; Registries and entry points

(def commands
  "Slash command (no slash) -> (fn [state args] [state' effects])."
  {"display" display-cmd
   "panel"   panel-cmd
   "swarm"   swarm-cmd})

(defn command-step
  [state event]
  (if-let [f (get commands (:name event))]
    (f state (vec (:args event)))
    [state [(notify :error (str "not a view command: /" (:name event)))]]))

(def events
  "Event type -> (fn [state event] [state' effects])."
  {"init"    (fn [state _] [state []])
   "command" command-step
   "grid"    grid-step})

(defn model
  [state]
  {:swarm         (when-let [s (:swarm state)] {:selected (:selected s)})
   :grid_keys     (vec (sort (keys grid-keymap)))
   :view_commands (vec (sort (keys commands)))})

(defn step
  "[state' effects] for `event`."
  [state event]
  (if-let [f (get events (:type event))]
    (f state event)
    [state [(notify :error (str "unknown view event: " (:type event)))]]))

(def state (atom {:swarm nil}))

(defn dispatch!
  "Fold `event` into the view state; {:model :effects}."
  [event]
  (let [[s effects] (step @state event)]
    (reset! state s)
    {:model (model s) :effects effects}))
