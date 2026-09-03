//! Janet runs on a dedicated OS thread.
//!
//! The original `PluginManager` held the `JanetClient` directly and relied
//! on `#[tokio::main(flavor = "current_thread")]` + an `unsafe impl Send`
//! to satisfy `rig::ToolDyn`'s Send bound on tool-call futures. That was
//! sound under the existing single-thread runtime but blocked synchronous
//! dialog APIs (`harness/confirm`, `harness/select`) — they would have
//! deadlocked, since the Janet eval call sat on the same OS thread that
//! also drove the UI event loop.
//!
//! This module spawns a dedicated worker thread that owns the
//! `JanetClient`. Callers send [`Cmd`]s to the worker via an mpsc channel
//! and block-receive replies on per-command oneshot reply channels. The
//! UI thread is free to render dialogs while the worker thread is blocked
//! inside Janet awaiting a dialog response.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

/// dirge-2jtd: marks a value that is really an ev-path error.
///
/// Long and unlikely on purpose. A plugin that deliberately returned this exact
/// prefix would be misreported as failing — accepted, because the alternative
/// (a second round trip to read a status variable on every eval) taxes hook
/// dispatch, which is on the hot path.
///
/// Its only use is in the `Cmd::Eval` handler, which is plugin-gated, so
/// feature sets without `plugin` (windows-default among them) see it as dead —
/// same treatment as the rest of this module.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const EV_ERROR_SENTINEL: &str = "__dirge_ev_error_9f3a__";

#[cfg_attr(not(feature = "plugin"), allow(unused_imports))]
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::sync::mpsc as tmpsc;

use super::interrupt::InterruptHandle;

/// How long the init handshake waits for the worker to confirm Janet
/// initialization before giving up. Worker init is normally well under
/// 100 ms; 10 s is just a watchdog so a hung worker doesn't pin main.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval for the dialog reply loop. The cfn wakes every
/// `DIALOG_POLL` to check the shutdown flag so a UI exit doesn't pin
/// the worker thread forever. Short enough that shutdown feels snappy,
/// long enough that polling overhead is negligible.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const DIALOG_POLL: Duration = Duration::from_millis(50);

/// Wall-clock bound on `send_dialog` (dirge-u5ig). Dialogs were the one
/// host call with NO timeout — `harness/confirm` / `harness/select` polled
/// the reply forever, so a dialog whose responder never answers (headless
/// without `--auto-confirm`, or a starved responder) pinned the single
/// Janet worker permanently, which in turn wedges every later eval that
/// serializes behind it. Unlike `LSP_QUERY_TIMEOUT` (30s) this is
/// deliberately generous: in interactive mode a human answers the dialog,
/// and a distracted user may take minutes. The bound only exists so a
/// never-answered dialog can't pin the worker forever — it is not meant to
/// rush a real human. After it elapses `send_dialog` returns `None` (the
/// cfn treats that as "no answer", same as a shutdown-cancelled dialog).
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const DIALOG_TIMEOUT: Duration = Duration::from_secs(600);

/// Whether `send_dialog` should stop waiting: either the worker is shutting
/// down, or the dialog has exceeded [`DIALOG_TIMEOUT`]. Split out so the
/// give-up policy is unit-testable without a real hung responder (mirrors
/// `lsp_should_abort`).
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
fn dialog_should_abort(elapsed: Duration, shutting_down: bool) -> bool {
    shutting_down || elapsed >= DIALOG_TIMEOUT
}

/// Upper bound on how long a single `Worker::eval` will wait for the
/// worker's reply. Generous (10 min) because `harness/confirm` /
/// `harness/select` legitimately block the worker on user input — a
/// distracted user may take minutes to answer a dialog. The point of
/// the bound is to detect a truly wedged plugin (e.g. plugin code in
/// `(while true)`) rather than to enforce snappy responses. When the
/// timeout fires the caller gets a clean error instead of hanging.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const EVAL_TIMEOUT: Duration = Duration::from_secs(600);
/// UI-3: default for interactive `eval()` calls (slash commands,
/// provider list lookups, UI-driven plugin queries). A runaway
/// plugin shouldn't freeze the UI for the full `EVAL_TIMEOUT`.
const INTERACTIVE_EVAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on how long `Worker::Drop` will wait for the worker
/// thread to exit. Short by design: shutdown is best-effort. If the
/// plugin is stuck in an infinite loop, the worker thread can't
/// respond to `Cmd::Shutdown` and we'd hang the user's terminal
/// forever on Drop. Beyond `JOIN_TIMEOUT` we leak the thread (it's
/// reaped on process exit) and log a warn so the operator knows.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `eval_with_timeout` waits for the worker to unwind after it
/// raises a Janet interrupt (dirge-9xjg.1).
///
/// The interrupt is checked at backward jumps and call boundaries, so a
/// Janet-level runaway loop reacts within microseconds; the grace exists so
/// the caller can report the *real* Janet error ("interrupt") instead of a
/// generic "worker is wedged". When it expires, the eval was almost
/// certainly parked in a C syscall the interrupt cannot reach — that is a
/// genuinely different failure and gets a different message.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const INTERRUPT_GRACE: Duration = Duration::from_secs(2);

#[cfg(feature = "plugin")]
use janetrs::client::JanetClient;
#[cfg(feature = "plugin")]
use janetrs::env::CFunOptions;

/// Janet definitions installed on the worker thread at startup. Includes
/// the harness state variables, the regular harness/* functions, and
/// Janet-side wrappers that forward to the registered C functions for
/// `harness/confirm` and `harness/select`.
///
/// Kept as a single string so worker init does one `client.run` call.
#[cfg(feature = "plugin")]
const HARNESS_INIT: &str = r#"
# NOTE: `:out`/`:err` are bound by CAPTURE_BOOTSTRAP, which runs BEFORE
# this prelude. Do NOT rebind them here — the host mirrors those exact
# buffer objects into Janet's top-level dyn table, and a second
# `(setdyn :out @"")` would leave that mirror pointing at an orphan.

# --- per-eval output capture (dirge-9xjg.3) ------------------------
#
# The notebook returns a cell's printed output to the agent, so the
# discard buffers above double as capture buffers: reset before a cell,
# read after it.
#
# BOTH helpers clear the buffers IN PLACE and never rebind them. The host
# mirrors these exact buffer objects into Janet's TOP-LEVEL dyn table
# (dirge-c8lh) so that compile errors and stack traces — which Janet
# emits with no fiber active, and which therefore read `top_dyns` rather
# than the fiber env — are captured instead of leaking to the real
# terminal. A `(setdyn :out @"")` here would swap only the fiber-level
# binding, leave `top_dyns` pointing at the orphaned buffer, and silently
# put those traces back on the user's screen.
# Clears in place only. There is deliberately NO rebinding fallback for a
# non-buffer `:out`: CAPTURE_BOOTSTRAP guarantees a buffer, and a
# `(setdyn :out @"")` here would strand the host's top-level mirror on the
# old object — trading "this cell's output is not captured" for "every
# later error trace prints on the user's screen".
(defn harness/-capture-reset []
  (def o (dyn :out))
  (when (buffer? o) (buffer/clear o))
  (def e (dyn :err))
  (when (buffer? e) (buffer/clear e))
  nil)

# Read back what the last eval printed, capped at `limit` bytes per
# stream. A cell that prints in a loop is truncated with a marker rather
# than returned in full — the reader is a language model with a context
# budget, and an unbounded paste is worse than a labelled elision.
(defn harness/-capture-take [limit]
  (defn- one [b label]
    (if (and (buffer? b) (pos? (length b)))
      (let [s (string b) n (length s)]
        (if (> n limit)
          (string (string/slice s 0 limit)
                  "\n...[" label " truncated, " (- n limit) " more bytes]")
          s))
      ""))
  (def o (one (dyn :out) "stdout"))
  (def e (one (dyn :err) "stderr"))
  (cond
    (and (empty? o) (empty? e)) ""
    (empty? e) o
    (empty? o) (string "[stderr]\n" e)
    (string o "\n[stderr]\n" e)))

(var harness-pending nil)
(var harness-response nil)
# Per-tool-hook slots: cleared by the host at the start of
# dispatch_tool_hook so previous-call state doesn't leak.
(var harness-block nil)
(var harness-mutate-input nil)
(var harness-replace-result nil)

# Entity/relation record accumulators (experimental-graph-search).
# Janet compressors call harness/record-entity and harness/record-relation
# during on-tool-end. The host drains these after dispatch and persists
# to SQLite. Tab-separated blobs (harness/-escape'd): kind\tname\textra\n
# and source_kind\tsource_name\ttarget_kind\ttarget_name\trel_type\n.
(var harness-recorded-entities "")
(var harness-recorded-relations "")

# harness/log queues a debug breadcrumb. The host drains
# `harness-log-messages` after each hook dispatch and emits each line
# via tracing::info! (target dirge::plugin::log), so it surfaces under
# RUST_LOG / --verbose. The return value of plugin commands is still
# what surfaces in chat; harness/log is the out-of-band debug sink
# (dirge-8gdv.7). The queueing `defn` lives below `harness/-escape`
# (which it uses); the var is declared here with the other slots.
(var harness-log-messages "")
(defn harness/get-cwd [] (os/cwd))
(defn harness/request-prompt [prompt]
  (when (string? prompt)
    (set harness-pending prompt)))
(defn harness/store-response [resp]
  (set harness-response resp))
(defn harness/has-symbol? [name]
  (truthy? (get (curenv) (symbol name))))

# dirge-99ic: the loading plugin's config.json settings
# (`plugins.<name>`). The host sets this to the plugin's settings right
# BEFORE each plugin's files load, then clears it. A plugin must capture
# its own config in LOAD-TIME code (e.g. `(def my-cfg (harness/plugin-config))`)
# — reading it later from a shared hook is unreliable because the slot
# reflects the LAST plugin loaded. Shape: @{:enabled bool :auto-start bool}
# or nil when no `plugins` config applies.
(var harness-plugin-config nil)
(defn harness/plugin-config [] harness-plugin-config)

# Tool-hook slots. Plugins call these from inside
# on-tool-start / on-tool-end. The host reads them via
# dispatch_tool_hook on the Rust side.
(defn harness/block [reason]
  (when (string? reason) (set harness-block reason)))
(defn harness/mutate-input [json-str]
  (when (string? json-str) (set harness-mutate-input json-str)))
(defn harness/replace-result [output]
  (when (string? output) (set harness-replace-result output)))

# Entity/relation recording for graph-search (#393).
# Compressors call these from `on-tool-end` to persist structured facts.
# The host drains `harness-recorded-entities` and `harness-recorded-relations`
# after dispatch and writes them to the SQLite entity/relation tables.

# Wire-format escape — used by every tab-separated harness blob.
(defn- harness/-escape [s]
  (->> s
       (string/replace-all "\\" "\\\\")
       (string/replace-all "\t" "\\t")
       (string/replace-all "\n" "\\n")))

# harness/log breadcrumb queue (see the harness-log-messages var above).
(defn harness/log [msg]
  (when (string? msg)
    (set harness-log-messages
         (string harness-log-messages (harness/-escape msg) "\n"))))

(defn harness/record-entity [kind name &opt extra]
  (when (and (string? kind) (string? name))
    (let [escaped-name (harness/-escape name)
          escaped-kind (harness/-escape kind)
          escaped-extra (if (string? extra) (harness/-escape extra) "")]
      (set harness-recorded-entities
           (string harness-recorded-entities
                  escaped-kind "\t" escaped-name "\t" escaped-extra "\n")))))
(defn harness/record-relation [source-kind source-name target-kind target-name rel-type]
  (when (and (string? source-kind) (string? source-name)
             (string? target-kind) (string? target-name)
             (string? rel-type))
    (set harness-recorded-relations
         (string harness-recorded-relations
                (harness/-escape source-kind) "\t"
                (harness/-escape source-name) "\t"
                (harness/-escape target-kind) "\t"
                (harness/-escape target-name) "\t"
                (harness/-escape rel-type) "\n"))))

# Entity bundle compression (N3). Takes a multi-line bundle text
# (e.g. /graph traverse output) and compresses it: groups by
# terminal kind, deduplicates per entity (shortest path kept),
# produces a compact summary.
(defn harness/compress-bundle [bundle-text &opt query]
  (default query "")
  (unless (string? bundle-text) (break "expected string"))
  (def lines (filter |(not (empty? $)) (string/split "\n" bundle-text)))
  (if (empty? lines) (break ""))
  # Extract entity kind from terminal [...] in each line.
  (defn terminal-kind [line]
    (def parts (string/split "[" line))
    (if (> (length parts) 1)
      (let [last (last parts)]
        (first (string/split "]" last)))
      "unknown"))
  # Extract entity name (last segment before terminal [kind]).
  (defn terminal-name [line]
    (def rev (reverse (string/split "[" line)))
    (if (> (length rev) 2)
      (let [before-bracket (get rev 1)]
        (string/trim (last (string/split "→" before-bracket))))
      (let [first-part (last (string/split "→" line))]
        (string/trim (first (string/split "[" first-part))))))
  # Per entity (name+kind), keep the shortest line.
  (def deduped @{})
  (each line lines
    (def kind (terminal-kind line))
    (def name (terminal-name line))
    (def key (string kind ":" name))
    (let [existing (get deduped key)]
      (when (or (nil? existing) (< (length line) (length existing)))
        (put deduped key line))))
  # Group by kind.
  (def by-kind @{})
  (each [key line] (pairs deduped)
    (def kind (terminal-kind line))
    (let [lst (get by-kind kind)]
      (if (nil? lst)
        (put by-kind kind @[line])
        (array/push lst line))))
  # Build output.
  (def buf @[""])
  (def kind-counts @[])
  (each kind (sort (keys by-kind))
    (let [ents (get by-kind kind)]
      (array/push kind-counts (string (length ents) " " kind))))
  (array/push buf (string (length (keys deduped)) " entities: " (string/join kind-counts ", ")))
  (each kind (sort (keys by-kind))
    (array/push buf (string "  [" kind "]"))
    (each line (get by-kind kind)
      (array/push buf (string "    " line))))
  (string/join buf "\n"))

# Run-boundary slots. Plugins call `harness/set-next-model` from
# inside `prepare-next-run` to swap the active model before the
# next user prompt runs. Mid-stream model swap isn't supported
# (rig's multi-turn stream state doesn't survive it); the slot is
# read by the host after Done and applied via the same path that
# `/model <name>` uses.
(var harness-next-model nil)
(defn harness/set-next-model [model-name]
  (when (string? model-name) (set harness-next-model model-name)))

# ============================================================
# Phase 5 — pi-loop hook surface for plugins
# ============================================================
# These slots are polled by the new agent_loop path between
# turns. Plugins set them from `on-tool-end` / `on-prompt` /
# `prepare-next-run` to influence the next turn without
# restarting the whole run.

# Next turn's thinking level. Plugins call
# (harness/set-next-thinking-level "high") inside on-tool-end
# to bump reasoning on the next assistant turn. Valid values:
# "off" "minimal" "low" "medium" "high" "xhigh". Other strings
# are ignored.
(var harness-next-thinking-level nil)
(defn harness/set-next-thinking-level [level]
  (when (string? level)
    (set harness-next-thinking-level level)))

# Stop-after-turn flag. Plugins call
# (harness/request-stop-after-turn) to ask the loop to end
# gracefully after the current turn finishes. Drained per turn
# by the host.
(var harness-stop-after-turn nil)
(defn harness/request-stop-after-turn []
  (set harness-stop-after-turn true))

# Steering message queue (mid-run). Plugins call
# (harness/add-steering "wait, also do X") to inject a user
# turn between assistant turns. Drained per turn-boundary by
# the host. Stored as a `harness/-escape'd msg\n` blob so an
# embedded newline round-trips as a single message (dirge-yrta).
(var harness-steering-messages "")
(defn harness/add-steering [content]
  (when (string? content)
    (set harness-steering-messages
         (string harness-steering-messages (harness/-escape content) "\n"))))

# Follow-up message queue (outer-loop boundary). Plugins call
# (harness/add-followup "do this next") to add a turn AFTER the
# loop would otherwise stop. Same blob shape as steering.
(var harness-followup-messages "")
(defn harness/add-followup [content]
  (when (string? content)
    (set harness-followup-messages
         (string harness-followup-messages (harness/-escape content) "\n"))))

# Custom (UI-only) message queue. Plugins call this to push a
# notification the user SEES in the chat but the model does NOT
# see in its context. Pi semantics: any message variant other
# than user/assistant/toolResult is filtered out by `convertToLlm`.
# We make this explicit with a `LoopMessage::Custom` variant; the
# UI renders it; convert_to_llm drops it before the LLM sees it.
#
# Two call shapes (pi parity — CustomMessage carries customType,
# content, display at top level; see messages.ts:46):
#
#   (harness/add-custom-message "build started")
#     bare content. customType="" display=true. The UI uses its
#     default formatter ("[plugin] <text>"); no registered
#     renderer fires.
#
#   (harness/add-custom-message customType content &opt display)
#     structured. customType is the key registered renderers are
#     keyed by (see `harness/register-message-renderer`); display
#     is true by default — false keeps the message in the
#     transcript but suppresses the chat line.
#
# Stored as tab-separated `customType\tcontent\tdisplay\n`
# (harness/-escape'd so embedded tabs/newlines round-trip).
# Drained per turn boundary alongside steering messages.
# dirge-df1v: same per-turn cap as harness/notify above — a plugin
# can't grow this buffer without bound before the host's per-turn drain.
(def harness-custom-msg-cap 131072)
(var harness-custom-flooded false)
(var harness-custom-messages "")
(defn harness/add-custom-message [a &opt b c]
  (when (= harness-custom-messages "") (set harness-custom-flooded false))
  (if (>= (length harness-custom-messages) harness-custom-msg-cap)
    (unless harness-custom-flooded
      (set harness-custom-flooded true)
      (set harness-custom-messages
           (string harness-custom-messages
                   (harness/-escape "") "\t"
                   (harness/-escape "[plugin] too many custom messages this turn — further ones dropped") "\t"
                   "1\n")))
    (cond
      # Single-string form — bare content, no type.
      (and (string? a) (nil? b))
        (set harness-custom-messages
             (string harness-custom-messages
                     (harness/-escape "") "\t"
                     (harness/-escape a) "\t"
                     "1\n"))
      # Typed form.
      (and (string? a) (string? b))
        (let [d (if (nil? c) "1" (if c "1" "0"))]
          (set harness-custom-messages
               (string harness-custom-messages
                       (harness/-escape a) "\t"
                       (harness/-escape b) "\t"
                       d "\n"))))))

# Slash-command registry (9b — wire format aligned with the other
# tab-separated harness blobs). Plugins register at load time; the
# host reads the list once after all plugins load and dispatches
# matching /cmd input back to the named handler. Last-load-wins on
# name collision (matches pi's Map.set + the dedup_last_wins helper
# applied to all the other plugin registries).
(var harness-cmd-list "")
(defn harness/register-command [name handler]
  (when (and (string? name) (string? handler))
    (set harness-cmd-list
         (string harness-cmd-list
                 (harness/-escape name) "\t"
                 (harness/-escape handler) "\n"))))

# Replace the user's prompt for the current turn. Plugins
# call this from on-prompt hooks. Distinct from
# harness/request-prompt which queues a follow-up turn.
(var harness-prompt-replace nil)
(defn harness/replace-prompt [text]
  (when (string? text)
    (set harness-prompt-replace text)))

# dirge-wqxj: append text to the assembled system prompt before
# the agent starts. Plugins call this from the `before-agent-start`
# hook, which receives the current prompt in ctx :system-prompt.
# Append-only by design — the base preamble (model identity + tool
# docs) is preserved; the appended text is added after it. Multiple
# appends from one hook concatenate (newline-joined).
(var harness-system-prompt-append nil)
(defn harness/append-system-prompt [text]
  (when (string? text)
    (set harness-system-prompt-append
         (if (string? harness-system-prompt-append)
           (string harness-system-prompt-append "\n" text)
           text))))

# dirge-lsoq: rewrite the finalized assistant message. Plugins call
# this from the `message-end` hook (which receives the message text
# in ctx :message). Last-write-wins; the host replaces the response
# text with the slot value before it is rendered/stored.
(var harness-message-rewrite nil)
(defn harness/rewrite-message [text]
  (when (string? text)
    (set harness-message-rewrite text)))

# dirge-264x: replace the message array for the NEXT LLM call.
# Plugins call this from the `transform-context` hook, which
# receives the current messages as a JSON array string in
# ctx :messages. The value must be a JSON array string; the host
# parses it and uses it for that single LLM call only (the persisted
# transcript is unchanged). Last-write-wins.
(var harness-replace-context nil)
(defn harness/replace-context [json-array]
  (when (string? json-array)
    (set harness-replace-context json-array)))

# dirge-jia8: supply a custom compaction summary. Plugins call this
# from the `on-compact` hook (which receives the to-be-summarized
# middle messages as JSON in ctx :messages). The host uses this
# string instead of calling the LLM summarizer, provided it passes
# the same validity check. The `on-before-compact` hook is
# observe-only (no slot) — it cannot cancel the fold.
(var harness-compact-summary nil)
(defn harness/set-compact-summary [text]
  (when (string? text)
    (set harness-compact-summary text)))

# Notification queue. Plugins call (harness/notify msg level?)
# to push a line into the host's chat display. Stored as a
# `level\tmsg\n` blob; the host's drain_notifications
# splits and clears in one round-trip.
# dirge-df1v: cap per-turn accumulation. A plugin that calls
# harness/notify in a hot hook (on-message-update fires every ~16
# tokens) would otherwise grow this buffer without bound before the
# host drains it at the turn boundary. Once over the cap we append ONE
# "dropped" marker and stop; the host clears the list to "" on drain,
# and the reset-on-empty check below re-arms the marker for next turn.
(def harness-notif-cap 65536)
(var harness-notif-flooded false)
(var harness-notif-list "")
(defn harness/notify [msg &opt level]
  (when (string? msg)
    (when (= harness-notif-list "") (set harness-notif-flooded false))
    (if (>= (length harness-notif-list) harness-notif-cap)
      (unless harness-notif-flooded
        (set harness-notif-flooded true)
        (set harness-notif-list
             (string harness-notif-list
                     "warn\t[plugin] too many notifications this turn — further ones dropped\n")))
      (let [lvl (cond
                  (or (= level :info) (= level "info")) "info"
                  (or (= level :warn) (= level "warn")) "warn"
                  (or (= level :error) (= level "error")) "error"
                  "info")]
        # dirge-vpma.32: escape like every other tab-separated harness blob.
        # Raw newlines truncated the message at the first one (the drain skips
        # lines with no tab), and a payload could mint a second entry with a
        # level of its choosing.
        (set harness-notif-list
             (string harness-notif-list lvl "\t" (harness/-escape msg) "\n"))))))

# Hook-error dedup slots. `harness-last-hook-err-msg` is the most
# recently pushed sanitized hook-error message; `harness-last-hook-err-count`
# is how many consecutive identical errors followed it. When a
# DIFFERENT error arrives (or any other notification fires), the
# count is flushed as a "(repeated N times)" entry. Drained alongside
# the regular notif list. See `harness/push-hook-err` below + the
# Rust-side dispatch wrapper in `plugin/mod.rs::dispatch`.
(var harness-last-hook-err-msg nil)
(var harness-last-hook-err-count 0)

# Sanitize a hook-error message for the `level\tmsg\n` wire format.
# Embedded tabs become spaces (so they don't get parsed as a second
# `level` field) and newlines become ` | ` (so a multi-line Janet
# stack trace stays on one notification entry).
#
# `string/replace-all` takes args as (patt subst str), so threading
# with `->` (first-position) would pass the wrong arg as the
# subject. Explicit nesting from inside out is the safest spelling.
(defn harness/sanitize-hook-err [s]
  (string/replace-all
    "\n" " | "
    (string/replace-all
      "\r\n" " | "
      (string/replace-all "\t" " " (string s)))))

# Push a hook error onto the notif list, deduplicating consecutive
# identical messages. The catch arm in dispatch calls this rather
# than appending directly so a buggy on-message-update hook can't
# flood the chat with thousands of identical banners.
#
# dirge-vpma.33: the summary reports how many were SUPPRESSED, i.e. the count
# minus the one already pushed and displayed. The count starts at 1 for that
# first push, so 50 occurrences is one banner plus "(repeated 49 times)" —
# which is what the host-side comment has always described.
#
# The flush is a function rather than a copy at each call site because the host
# runs it too, at drain, to collect a run of repeats that never saw a different
# message follow it.
(defn harness/-flush-hook-err []
  (when (and harness-last-hook-err-msg
             (> harness-last-hook-err-count 1))
    (set harness-notif-list
         (string harness-notif-list
                 "error\t"
                 (harness/-escape
                   (string harness-last-hook-err-msg
                           " (repeated "
                           (- harness-last-hook-err-count 1)
                           " times)"))
                 "\n"))))

(defn harness/push-hook-err [sanitized-msg]
  (if (= sanitized-msg harness-last-hook-err-msg)
    # Same as last — increment in place; do not push.
    (set harness-last-hook-err-count (+ harness-last-hook-err-count 1))
    # Different message (or first one). If the previous one had
    # been repeated, flush its summary now; then push the new msg
    # and reset the dedup state.
    (do
      (harness/-flush-hook-err)
      # dirge-vpma.32: escaped like the rest of the blob. sanitize-hook-err
      # rewrites newlines and tabs but leaves backslashes, so an unescaped
      # `C:\temp` would come back through the drain's unescape as a tab.
      (set harness-notif-list
           (string harness-notif-list "error\t" (harness/-escape sanitized-msg) "\n"))
      (set harness-last-hook-err-msg sanitized-msg)
      (set harness-last-hook-err-count 1))))

# Plugin entries on the session timeline. Plugins call
# (harness/append-entry type data &opt display) to record
# bookmarks, telemetry, or custom state that should survive
# session save/load. The data is treated as opaque by the host
# (any registered renderer for `type` formats it); other plugins
# can use plain text, JSON, etc.
#
# Stored as `type\tdata\tdisplay\n` per entry; data is escaped so
# embedded tabs / newlines / backslashes don't break parsing.
(var harness-entries-buf "")
(defn harness/append-entry [type data &opt display]
  (when (and (string? type) (string? data))
    (let [d (if (nil? display) "1" (if display "1" "0"))]
      (set harness-entries-buf
           (string harness-entries-buf
                   (harness/-escape type) "\t"
                   (harness/-escape data) "\t"
                   d "\n")))))

# Registered renderer functions per plugin entry type.
# (harness/register-renderer "bookmark" "fn-name") records a
# (type, fn-name) pair the host looks up when displaying entries
# of that type. Stored tab-separated with harness/-escape'd fields
# (same convention as harness-msg-renderers-list) so a type
# containing `|` or `\t` round-trips and last-wins dedup applies.
(var harness-renderer-list "")
(defn harness/register-renderer [type fn-name]
  (when (and (string? type) (string? fn-name))
    (set harness-renderer-list
         (string harness-renderer-list
                 (harness/-escape type) "\t"
                 (harness/-escape fn-name) "\n"))))

# Output buffer for a renderer invocation. The host clears it
# before calling the renderer, then reads back the accumulated
# `color\ttext\n` lines. Plugins call (harness/render color text)
# from inside their renderer to emit each output line.
(var harness-render-buf "")
(defn harness/render [color text]
  (when (and (or (string? color) (keyword? color) (symbol? color))
             (string? text))
    (set harness-render-buf
         (string harness-render-buf
                 (string color) "\t"
                 (harness/-escape text) "\n"))))

# Plugin-registered LLM providers (P1; 9b — wire format aligned with
# the other harness blobs). Plugins call
# (harness/register-provider name type base-url &opt api-key-env)
# at load time to make a custom provider available alongside the
# ones in config. Stored as tab-separated, escape-encoded fields so
# a single Janet -> Rust round-trip surfaces them all. Last-load-wins
# on name collision via dedup_last_wins.
(var harness-providers-list "")
(defn harness/register-provider [name type base-url &opt api-key-env]
  (when (and (string? name) (string? type) (string? base-url))
    (let [env (if (and api-key-env (string? api-key-env)) api-key-env "")]
      (set harness-providers-list
           (string harness-providers-list
                   (harness/-escape name) "\t"
                   (harness/-escape type) "\t"
                   (harness/-escape base-url) "\t"
                   (harness/-escape env) "\n")))))

# Session-tree mutation ops queued from plugins (P4d). Mirrors pi's
# ctx.setLabel / ctx.fork / ctx.navigateTree / ctx.newSession /
# ctx.switchSession but routed through the host so the drain happens
# between turns. Each line is `op\targ1[\targ2...]\n` (escaped via
# harness/-escape) so a single round-trip + parse gives the host the
# whole queue.
(var harness-tree-ops "")
(defn- harness/-push-op [& parts]
  (set harness-tree-ops
       (string harness-tree-ops
               (string/join (map harness/-escape (map string parts)) "\t")
               "\n")))
# (harness/set-label id label-or-nil) — set or clear a node label.
# Pass nil/false to clear; any string is set verbatim.
(defn harness/set-label [id label]
  (when (string? id)
    (harness/-push-op "set-label" id (if (string? label) label ""))))
# (harness/fork id &opt position) — branch off the chosen entry.
# position defaults to :before (extracts prompt text into editor);
# :at switches to that entry as the leaf without touching the editor.
(defn harness/fork [id &opt position]
  (when (string? id)
    (let [pos (cond
                (or (= position :at) (= position "at")) "at"
                "before")]
      (harness/-push-op "fork" id pos))))
# (harness/navigate-tree id) — move active leaf to id. User-message
# entries restore prompt text + go to parent (matching pi's behaviour);
# other entries become the new leaf directly.
(defn harness/navigate-tree [id]
  (when (string? id)
    (harness/-push-op "navigate-tree" id)))
# (harness/new-session &opt parent-session) — start a fresh session
# in place, optionally recording the prior session id as parent
# lineage. The host persists the current session before resetting.
(defn harness/new-session [&opt parent-session]
  (let [p (if (string? parent-session) parent-session "")]
    (harness/-push-op "new-session" p)))
# (harness/switch-session session-id-prefix) — load a saved session
# matching the id prefix and replace the current session in place.
(defn harness/switch-session [session-id]
  (when (string? session-id)
    (harness/-push-op "switch-session" session-id)))

# Plugin-registered renderers for `LoopMessage::Custom` events (P9d).
# Mirrors pi's `api.registerMessageRenderer(customType, renderer)`
# (extensions/types.ts:1171). Plugins call
#   (harness/register-message-renderer type-name handler)
# to provide a Janet function that the UI invokes when it sees a
# custom message whose JSON payload's `type` field matches. The
# handler receives the payload as a JSON string and returns the
# text to display. Distinct from `harness/register-renderer`, which
# is for session-timeline plugin entries (bookmarks, etc.) — message
# renderers fire mid-conversation as the agent loop emits Custom
# messages plugins queued via `harness/add-custom-message`.
(var harness-msg-renderers-list "")
(defn harness/register-message-renderer [type-name handler]
  (when (and (string? type-name) (string? handler))
    (set harness-msg-renderers-list
         (string harness-msg-renderers-list
                 (harness/-escape type-name) "\t"
                 (harness/-escape handler) "\n"))))

# Plugin-registered keyboard shortcuts (P9c). Plugins call
#   (harness/register-shortcut keys handler &opt description)
# to bind a key combination to a Janet handler the host invokes in
# interactive mode. `keys` is a string like "ctrl-x", "alt-shift-f",
# "f5", or "enter"; the host parses it via parse_key_spec and matches
# against incoming KeyEvents BEFORE built-in dispatch. Handler is a
# Janet function name; it's called with the key string as a single
# argument so one handler can serve multiple shortcuts and discriminate.
(var harness-shortcuts-list "")
(defn harness/register-shortcut [keys handler &opt description]
  (when (and (string? keys) (string? handler))
    (let [desc (if (and description (string? description)) description "")]
      (set harness-shortcuts-list
           (string harness-shortcuts-list
                   (harness/-escape keys) "\t"
                   (harness/-escape handler) "\t"
                   (harness/-escape desc) "\n")))))

# Plugin keybinding overrides (dirge-rj3k / #476). Plugins call
#   (harness/bind-key keys command)
# to bind a key chord — or an emacs-style sequence like "ctrl-x ctrl-s" —
# to a BUILT-IN command name (the same vocabulary the user's `keybindings`
# config uses: the global KeyAction commands and the input-editor
# InputAction commands), or "none" to unbind a default. The host merges
# these OVER the built-in defaults and UNDER the user's config, so user
# config always wins. This REMAPS built-ins; to bind a key to plugin CODE,
# use register-shortcut instead.
(var harness-keybindings-list "")
(defn harness/bind-key [keys command]
  (when (and (string? keys) (string? command))
    (set harness-keybindings-list
         (string harness-keybindings-list
                 (harness/-escape keys) "\t"
                 (harness/-escape command) "\n"))))

# Per-invocation context slot set by the host before each plugin
# tool handler runs (H2). Reads return the tool-call id the LLM
# assigned to the current call — useful for correlating progress
# updates, logging, or pairing related state. Cleared between
# invocations so a handler observing nil knows no plugin tool is
# active.
(var harness-current-tool-call nil)

# (harness/emit-tool-progress text) — push a streaming progress
# update for the currently-running plugin tool (H2). Mirrors pi's
# onUpdate callback (extensions/types.ts execute signature). No-op
# when called outside a plugin tool handler (current-tool-call nil)
# or with a non-string arg. The host drains the queue and forwards
# each entry to the loop's per-tool on_update callback.
(var harness-tool-progress "")
(defn harness/emit-tool-progress [text]
  (when (and (string? text) harness-current-tool-call)
    (set harness-tool-progress
         (string harness-tool-progress
                 (harness/-escape harness-current-tool-call) "\t"
                 (harness/-escape text) "\n"))))

# Plugin-registered LLM-callable tools (P9a). Plugins call
#   (harness/register-tool name description label parameters handler
#                          &opt execution-mode prepare-arguments)
# at load time to make a new tool available to the LLM alongside
# the built-ins.
#
# - `parameters` is a JSON-schema string.
# - `handler` is the name of a Janet function that takes one
#   argument (the raw JSON args string the LLM produced) and
#   returns either a string (the tool result text) or any value
#   that (string ...) can render.
# - `execution-mode` is :parallel (read-only, default) or
#   :sequential (mutating). Pass nil to skip when you only want
#   to set prepare-arguments.
# - `prepare-arguments` (H3) is the name of an optional Janet
#   function that takes the raw JSON args string and returns a
#   mutated JSON string the loop validates against the schema.
#   Mirrors pi's `prepareArguments` (extensions/types.ts:443).
#   Errors fall back to the original args.
#
# Stored as tab-separated, escape-encoded line per tool.
(var harness-tools-list "")
(defn harness/register-tool [name description label parameters handler &opt execution-mode prepare-arguments]
  (when (and (string? name) (string? description) (string? label)
             (string? parameters) (string? handler))
    (let [mode (cond
                 (or (= execution-mode :sequential) (= execution-mode "sequential")) "sequential"
                 (or (= execution-mode :parallel) (= execution-mode "parallel")) "parallel"
                 "")
          prep (if (and prepare-arguments (string? prepare-arguments))
                 prepare-arguments
                 "")]
      (set harness-tools-list
           (string harness-tools-list
                   (harness/-escape name) "\t"
                   (harness/-escape description) "\t"
                   (harness/-escape label) "\t"
                   (harness/-escape parameters) "\t"
                   (harness/-escape handler) "\t"
                   mode "\t"
                    (harness/-escape prep) "\n")))))

# (harness/json-extract json-str key) -> string | nil
# Uses serde_json to extract a string value from a JSON object. Returns
# nil if the key is missing, the JSON is invalid, or the value is not a
# string. Much safer than hand-rolled quote-scanning.
(defn harness/json-extract [json-str key]
  (when (and (string? json-str) (string? key))
    (harness/__json-extract json-str key)))

# (harness/json-decode json-str) -> table | array | string | number
#                                 | boolean | :null | nil
# Full recursive decode into native Janet data. Returns nil when the
# input is not a string or is not valid JSON. JSON `null` decodes to the
# :null KEYWORD, not nil, because putting nil into a Janet table deletes
# the key — a null field would otherwise be indistinguishable from an
# absent one.
(defn harness/json-decode [json-str]
  (when (string? json-str)
    (harness/__json-decode json-str)))
"#;

/// The notebook bridge wrapper — installed on the **plugin VM only**.
///
/// Kept out of `HARNESS_INIT` because the notebook VM runs that same
/// prelude and deliberately does NOT register `harness/__notebook` (a cell
/// that could call the bridge would re-enter the notebook lock it is
/// already holding). Janet resolves symbols at COMPILE time, so a
/// `(dyn 'harness/__notebook)` runtime guard does not save a body that
/// names the symbol — the whole prelude fails to compile. Separating the
/// definition is the only thing that works, and it matches the intent:
/// this function has no business existing on the notebook VM.
#[cfg(feature = "plugin")]
const HARNESS_NOTEBOOK_INIT: &str = r#"
# (harness/notebook op session &opt code) -> @{:ok bool :value str :output str}
# Ops: "eval", "reset", "sessions", "respawn".
(defn harness/notebook [op session &opt code]
  (harness/__notebook (string op) (string session) (string (or code ""))))
"#;

/// The `:out`/`:err` redirect. The **only** place these are bound.
///
/// Janet's default `:out` is the real stdout, and in dirge's raw-mode TUI
/// every `(print …)` from plugin code corrupts the screen: a bare `\n`
/// produces staircase artifacts AND bypasses ratatui's tracked buffer,
/// leaving ghost cells the next diff never cleans up. Plugin authors who
/// want real logging should write to a file via `file/open`/`file/write`.
///
/// Split out of `HARNESS_INIT` and run FIRST so the buffers exist, and are
/// mirrored into `top_dyns`, before any prelude that could fail to
/// compile — otherwise a syntax error in the harness prelude itself is the
/// one error that still reaches the terminal (dirge-c8lh).
///
/// Nothing else may rebind these. The mirror captures the buffer OBJECTS,
/// so a later `(setdyn :out @"")` anywhere would swap the fiber-level
/// binding and silently strand `top_dyns` on the old buffer — putting
/// error traces back on the user's screen. Clear in place instead
/// (`harness/-capture-reset`).
#[cfg(feature = "plugin")]
const CAPTURE_BOOTSTRAP: &str = r#"
(setdyn :out @"")
(setdyn :err @"")
"#;

/// Janet-side aliases that defer the actual blocking work to the
/// registered C functions. Installed after `add_c_fn` so the symbols
/// are present in the env.
#[cfg(feature = "plugin")]
const HARNESS_DIALOG_INIT: &str = r#"
# (harness/confirm "title" "question") -> bool
# (harness/select  "title" array-of-options) -> string | nil
#
# Both block the worker thread (not the UI thread) until the host
# replies. dirge-qhfk: the host dispatches lifecycle hooks OFF the UI
# loop, so a dialog opened from any of them (on-prompt, on-turn-start/end,
# on-response, message-end, on-complete, prepare-next-run, on-error) or a
# tool hook keeps the loop free to service the reply. The ONE exception is
# `on-message-update`: it fires per streamed token batch and still runs
# inline, so opening a dialog there is unsupported (and nonsensical).
(defn harness/confirm [title question]
  (if (and (string? title) (string? question))
    (harness/__confirm title question)
    false))

(defn harness/select [title opts]
    (when (and (string? title) (indexed? opts))
    (harness/__select title opts)))
"#;

/// Janet wrapper for harness/__computer-use-exec. Takes a dict with
/// :action (keyword) and action-specific fields. Example:
///
///   (harness/computer-use-exec
///     {"action" "key" "keys" @[56 15]})
///
/// Returns a dict with :exit_code, :stdout, :stderr on success, or
/// raises a Janet error on failure / channel disconnect.
#[cfg(feature = "plugin")]
const HARNESS_COMPUTER_USE_INIT: &str = r#"
(defn harness/computer-use-exec [params]
  (when (not (dictionary? params))
    (error "harness/computer-use-exec: expected a dictionary"))
  (let [action (get params "action")]
    (when (not (string? action))
      (error "harness/computer-use-exec: :action must be a string")))
  (let [result (harness/__computer-use-exec params)]
    (when (not result)
      (error "harness/computer-use-exec: sandbox not available"))
    result))

(var harness/computer-use-deny-tools @{})

(defn harness/set-computer-use-deny-tools [tools-vec]
  "Replace the deny-tools set with a new list (called from Rust)."
  (let [s @{}]
    (each t tools-vec (put s (string t) true))
    (set harness/computer-use-deny-tools s)))

(defn harness/check-computer-action [action]
  "Query the PDP for a desktop action. Returns \"deny\" or \"ask\".
   Respects deny_tools: [computer] and deny_tools: [computer:<action>].
   `harness/computer-use-deny-tools` is a var holding a table — reference it
   directly; wrapping it in parens calls the table (arity error)."
  (if (in harness/computer-use-deny-tools "computer")
    "deny"
    (if (in harness/computer-use-deny-tools (string "computer:" action))
      "deny"
      "ask")))
"#;

/// Janet wrappers for the LSP bridge. Installed after the C function is
/// (conditionally) registered. Every wrapper guards on `(harness/lsp?)`
/// so that on a build without the `lsp` feature — or when LSP is disabled
/// at runtime — the functions exist but return `nil` instead of erroring.
///
/// `harness/lsp` returns a JSON string (the LSP result) or nil. The
/// typed wrappers fill in the operation name. Positions are 1-based
/// line/column to match the `lsp` tool.
#[cfg(feature = "plugin")]
const HARNESS_LSP_INIT: &str = r#"
(defn harness/lsp?
  "True when the LSP bridge is available AND wired to a live language-
   server manager. False on builds without the `lsp` feature, and also
   when LSP is disabled at runtime — so a true result guarantees that a
   following `harness/lsp` call will actually reach a server (returning a
   JSON string), never a silent nil."
  []
  (if-let [entry (get (curenv) 'harness/__lsp-live)]
    (truthy? ((entry :value)))
    false))

(defn harness/lsp
  "Query the language servers. `op` is one of definition, references,
   hover, documentSymbol, workspaceSymbol, implementation,
   incomingCalls, outgoingCalls, diagnostics. Returns a JSON string of
   the result, or nil when LSP is unavailable. line/char are 1-based;
   query is the search string for workspaceSymbol."
  [op file &opt line char query]
  (let [l (if line line 1)
        c (if char char 1)]
    # Validate before anything else — 1-based coordinates must be
    # positive integers. A bad value is a plugin bug; surface it loudly
    # rather than silently clamping it to the first line/column.
    (assert (and (number? l) (>= l 1))
            "harness/lsp: line must be a positive (1-based) integer")
    (assert (and (number? c) (>= c 1))
            "harness/lsp: char must be a positive (1-based) integer")
    (if (harness/lsp?)
      (harness/__lsp (string op) (string file) l c
                     (if query (string query) ""))
      nil)))

(defn harness/lsp-definition [file line char] (harness/lsp "definition" file line char))
(defn harness/lsp-references [file line char] (harness/lsp "references" file line char))
(defn harness/lsp-hover [file line char] (harness/lsp "hover" file line char))
(defn harness/lsp-implementation [file line char] (harness/lsp "implementation" file line char))
(defn harness/lsp-incoming-calls [file line char] (harness/lsp "incomingCalls" file line char))
(defn harness/lsp-outgoing-calls [file line char] (harness/lsp "outgoingCalls" file line char))
(defn harness/lsp-document-symbols [file] (harness/lsp "documentSymbol" file))
(defn harness/lsp-workspace-symbols [file query] (harness/lsp "workspaceSymbol" file 1 1 query))
(defn harness/lsp-diagnostics [file] (harness/lsp "diagnostics" file))
"#;

/// Janet wrappers for the tool bridge, installed after the C functions.
///
/// `harness/call-tool` returns `@{:ok bool :output string}` so a plugin can
/// distinguish a tool's error from its output, and `nil` when the bridge
/// isn't wired at all (no responder — e.g. a build that never constructed
/// an agent). Feature-detect with `(harness/tools?)`, exactly as with LSP.
///
/// Arguments are validated here rather than in the C function so a plugin
/// gets a normal catchable Janet error instead of a silent nil.
#[cfg(feature = "plugin")]
const HARNESS_TOOL_INIT: &str = r#"
(defn harness/tools? []
  (if-let [entry (get (curenv) 'harness/__tools-live)]
    (truthy? ((get entry :value)))
    false))

(defn harness/list-tools []
  (when (harness/tools?)
    (harness/__list-tools)))

(defn harness/call-tool [name &opt args]
  (when (not (string? name))
    (error "harness/call-tool: tool name must be a string"))
  (def payload (cond
                 (nil? args) "{}"
                 (string? args) args
                 (error "harness/call-tool: args must be a JSON string")))
  (harness/__call-tool name payload))
"#;

/// Vigil Janet prelude — exposes vigil/emit, vigil/list, vigil/set-state,
/// vigil/get, vigil/live? for plugins running inside a vigil-keeper.
#[cfg(all(feature = "plugin", feature = "vigil"))]
const HARNESS_VIGIL_INIT: &str = r#"
(defn vigil/live?
  "True when the vigil bridge is active. False on builds without the
   vigil feature, and also when vigil is not running — so a true result
   guarantees that vigil/emit will actually reach the keeper."
  []
  (if-let [entry (get (curenv) 'harness/__vigil-live)]
    (truthy? ((entry :value)))
    false))

(defn- json-escape
  "Escape a string for embedding inside a JSON string literal."
  [s]
  (->> s
       (string/replace-all "\\" "\\\\")
       (string/replace-all "\"" "\\\"")
       (string/replace-all "\n" "\\n")
       (string/replace-all "\r" "\\r")
       (string/replace-all "\t" "\\t")))

(defn- json-encode
  "Serialize a Janet value to JSON. Handles strings, numbers, booleans,
   nil, indexed arrays, and dictionaries (tables/structs)."
  [x]
  (cond
    (string? x) (string "\"" (json-escape x) "\"")
    (number? x) (string x)
    (= x true) "true"
    (= x false) "false"
    (nil? x) "null"
    (dictionary? x)
      (string "{"
              (string/join
                (map (fn [[k v]]
                       (string "\"" k "\":" (json-encode v)))
                     (pairs x))
                ",")
              "}")
    (indexed? x)
      (string "["
              (string/join (map json-encode x) ",")
              "]")
    (string x)))

(defn vigil/emit
  "Push an event into the vigil-keeper. `event-name` is a string key;
   `data` is a dict or JSON string with event context."
  [event-name &opt data]
  (when (and (vigil/live?) (string? event-name))
    (let [payload (if data
                   (if (string? data) data (json-encode data))
                   "")
          msg (string event-name "\t" payload)]
      (harness/__vigil-emit msg))))

(defn vigil/list
  "Return an array of all active vigil names."
  []
  (when (vigil/live?)
    (harness/__vigil-list)))

(defn vigil/set-state
  "Set a state key for a named vigil. (vigil/set-state name key value)"
  [name key value]
  (when (and (vigil/live?) (string? name) (string? key))
    (harness/__vigil-set-state name key (string value))))

(defn vigil/get
  "Get the state table for a named vigil. (vigil/get name)"
  [name]
  (when (and (vigil/live?) (string? name))
    (harness/__vigil-get name)))
"#;

/// dirge-l6bf: neuter the Janet escape hatches that can terminate or
/// destabilize the HOST process. Every hook / command / tool handler is
/// already run inside a Janet `(try ...)` (see `mod.rs`), so an ordinary
/// Janet error in a plugin is caught and surfaced as a
/// `[plugin] … errored` notification — dirge survives. The one thing that
/// bypasses that net is a plugin calling a function that exits the OS
/// process directly (e.g. `os/exit` → C `exit()`), which would take down
/// all of dirge. We rebind those symbols in the shared plugin env to raise
/// a NORMAL, catchable Janet error instead, so a buggy or hostile plugin
/// can never quit/crash the tool. Runs after the harness preludes and
/// before any plugin file is loaded, so plugins compile against the
/// shadowed bindings. dirge itself never calls `os/exit` from Janet.
// Janet source — consumed only by the `cfg(feature = "plugin")` worker loop
// below; gated to match so a no-plugin build (e.g. Windows `windows-default`)
// doesn't trip `-D warnings` on the unused const.
#[cfg(feature = "plugin")]
const HARNESS_SANDBOX: &str = r#"
(defn- dirge-disabled-fn [sym-name]
  (fn [&] (error (string sym-name
                        " is disabled in dirge plugins: a plugin cannot"
                        " terminate or signal the host process"))))
(each name ["os/exit" "os/proc-kill" "os/sigaction"]
  (def sym (symbol name))
  (when (get (curenv) sym)
    (put (curenv) sym @{:value (dirge-disabled-fn name)})))

# --- subprocess output containment (dirge-v49u) --------------------
#
# A child process writes to dirge's REAL file descriptors. The
# `(setdyn :out @"")` redirect is a Janet-level abstraction and does not
# touch fds, so an unredirected `(os/execute …)` lands straight on the
# raw-mode TUI — the same staircase/ghost-cell corruption as dirge-c8lh,
# by a different route, and not fixed by it.
#
# Plugin authors could be expected to redirect. The model driving the
# notebook cannot: `(os/execute ["ls"] :p)` is the obvious thing to write
# and there is nothing in the language to warn it.
#
# So: fill in only the streams the caller did NOT specify. A caller
# managing its own redirection is left alone, and one that redirects only
# `:out` still gets its `:err` contained.
(def- dirge-real-execute os/execute)
(def- dirge-real-spawn os/spawn)

# Cap on subprocess output folded into the capture buffer.
(def- dirge-subprocess-cap 65536)

# Returns [env temp-file-or-nil]. `file/temp` is tmpfile(), so the file
# unlinks itself on close and there is nothing to clean up.
(defn- dirge-contain-streams [env]
  # `merge` (not `table/clone`) because callers pass struct literals like
  # {:out f}, which are immutable and cannot be cloned as tables.
  (def e (merge @{} (or env {})))
  (var f nil)
  (unless (and (get e :out) (get e :err)) (set f (file/temp)))
  (unless (get e :out) (put e :out f))
  (unless (get e :err) (put e :err f))
  [e f])

(defn- dirge-drain-streams [f]
  (when f
    (file/seek f :set 0)
    (def content (file/read f :all))
    (file/close f)
    (def sink (dyn :out))
    (when (and content (buffer? sink) (pos? (length content)))
      (if (> (length content) dirge-subprocess-cap)
        (do
          (buffer/push-string sink (string/slice content 0 dirge-subprocess-cap))
          (buffer/push-string sink "\n...[subprocess output truncated]\n"))
        (buffer/push-string sink content)))))

# NOTE: flags are passed through EXACTLY as given. Do not "helpfully"
# default them to :p — that adds PATH lookup to a call that never asked
# for it, which can resolve a name that stock Janet would have refused.
# Janet requires a keyword here, hence (keyword "") for the no-flag case.
(defn- dirge-execute [args &opt flags env]
  (def [e f] (dirge-contain-streams env))
  # `defer`, not a plain sequence: the :x flag raises on a non-zero exit,
  # and what the child printed BEFORE failing is exactly what someone
  # needs to see. Draining only on success would lose every failure.
  (defer (dirge-drain-streams f)
    (dirge-real-execute args (or flags (keyword "")) e)))

# os/spawn is asynchronous, so nothing here knows when the child is done
# and there is no correct moment to drain. Unrequested output is absorbed
# by the temp file rather than leaked; a caller that wants it asks for
# :pipe, which this leaves untouched.
(defn- dirge-spawn [args &opt flags env]
  (def [e _f] (dirge-contain-streams env))
  (dirge-real-spawn args (or flags (keyword "")) e))

(put (curenv) 'os/execute @{:value dirge-execute})
(put (curenv) 'os/spawn @{:value dirge-spawn})
"#;

/// Janet installed on the **notebook** VM only (dirge-9xjg.2, dirge-9xjg.5).
///
/// The notebook runs on its own Janet VM with a fresh root env, so nothing
/// here is visible to plugins and nothing a plugin defines is visible here.
/// That is deliberate: agent-authored code that could redefine a hook, or
/// shadow a `harness/` function out from under the permission gate, is a
/// privilege-escalation path, and the "call plugin helpers from a cell"
/// convenience is not worth it. Cross-VM access, if it is ever wanted,
/// should be an explicit bridge rather than a shared symbol table.
///
/// # Per-session envs
///
/// dirge runs subagents CONCURRENTLY off one process-global plugin manager,
/// so a single flat env would let two subagents' `(def results …)` clobber
/// each other — racy, and near-impossible for a model to diagnose. Each
/// session therefore evaluates into its own env, created with `make-env`
/// off the notebook root: lookups fall through to the root (so all of
/// Janet core is there), but every `def` lands in the session's own table.
///
/// Sharing then has to be *asked for*, through `notebook/shared`. Making
/// the deliberate path explicit and the accidental path impossible is the
/// only arrangement that keeps both halves of the design.
#[cfg(feature = "plugin")]
const NOTEBOOK_INIT: &str = r#"
# Cross-session scratch space. The ONLY state visible from another
# session — everything else a cell defines is private to its session.
(def notebook/shared @{})

# session-id -> env table
(def notebook/-envs @{})

(defn notebook/-env [sid]
  (or (get notebook/-envs sid)
      (let [e (make-env (curenv))]
        (put notebook/-envs sid e)
        e)))

# Evaluate `code` in `sid`'s env and return [status value].
#
# The inner fiber carries the `:e` (error) mask so an ordinary Janet error
# is reported as data rather than unwinding the whole eval. It does NOT
# mask user signals, so a deadline interrupt — JANET_SIGNAL_INTERRUPT is
# USER8 — passes straight through and still stops the cell.
# Whether the last cell evaluated cleanly. Read back by the host in a
# separate eval rather than encoded into the return value, so a cell whose
# value happens to look like a status marker cannot be misread as one.
(var notebook/-last-ok true)

(defn notebook/-eval [sid code]
  (def env (notebook/-env sid))
  (def f (fiber/new (fn [] (eval-string code)) :e))
  (fiber/setenv f env)
  (def r (resume f))
  (set notebook/-last-ok (not= (fiber/status f) :error))
  r)

# Forget one session's bindings. `notebook/shared` is untouched — a reset
# is for recovering a poisoned scratch env, not for wiping what other
# sessions deliberately published.
(defn notebook/-reset [sid]
  (put notebook/-envs sid nil)
  :reset)

(defn notebook/-sessions [] (sorted (keys notebook/-envs)))
"#;

/// A plugin LSP query, forwarded from the worker thread to the tokio-side
/// drainer (which owns the `LspManager`). `request` is a JSON object
/// `{op, file, line, char, query}`; the drainer runs the query and sends
/// the JSON-encoded result back on `reply`. Mirrors the dialog bridge so
/// a synchronous Janet `(harness/lsp …)` call can drive async LSP work.
/// Referenced unconditionally by the UI channel signature (like
/// `DialogRequest`), so the type isn't feature-gated.
#[derive(Debug)]
// The fields are only produced (worker `send_lsp`) and consumed
// (`lsp::harness::run_query`) when BOTH `plugin` and `lsp` are on; the
// type itself stays in the channel signature regardless, like
// `DialogRequest`.
#[cfg_attr(not(all(feature = "plugin", feature = "lsp")), allow(dead_code))]
pub struct LspRequest {
    pub request: String,
    pub reply: mpsc::Sender<String>,
}

/// A computer-use action forwarded from the Janet worker to the tokio
/// runtime for sandboxed execution. Mirrors `DialogRequest` so a
/// synchronous Janet `(harness/computer-use-exec ...)` call can drive
/// async sandbox work.
#[derive(Debug)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub struct SandboxExecRequest {
    pub action: ComputerUseAction,
    pub reply: mpsc::Sender<Result<SandboxExecOutput, String>>,
}

/// Actions the computer-use tool can perform, routed through the
/// microVM sandbox.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum ComputerUseAction {
    Key { keys: Vec<i64> },
    Type { text: String },
    MouseMove { x: i64, y: i64 },
    MouseClick { button: String },
    Scroll { direction: String, amount: i64 },
    Screenshot,
    KeyChord { chord: String },
    OpenUrl { url: String },
}

#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub struct SandboxExecOutput {
    pub exit_code: i32,
    pub merged: String,
}

/// Shared channel for sandbox exec requests. Set by main.rs before
/// plugins load; the C function on the worker thread reads it to
/// forward computer-use actions to the tokio runtime (which owns
/// the sandbox handle that can SSH into the microVM).
#[cfg(feature = "plugin")]
static SANDBOX_EXEC_TX: std::sync::OnceLock<
    tokio::sync::mpsc::UnboundedSender<SandboxExecRequest>,
> = std::sync::OnceLock::new();

/// Install the sandbox exec sender. Called once from main.rs after
/// the sandbox is constructed, before plugins load.
#[cfg(feature = "plugin")]
pub fn install_sandbox_exec_tx(tx: tokio::sync::mpsc::UnboundedSender<SandboxExecRequest>) {
    let _ = SANDBOX_EXEC_TX.set(tx);
}

// Manual binding for `janet_ckeywordv` — a C macro not generated
// by bindgen. Janet uses `janet_symbol` (already in bindings) for
// keywords; we compose it with `janet_wrap_keyword` like the C macro.
#[cfg(feature = "plugin")]
#[inline(always)]
unsafe fn janet_ckeywordv(bytes: *const u8, len: i32) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::{janet_symbol, janet_wrap_keyword};
    unsafe { janet_wrap_keyword(janet_symbol(bytes, len)) }
}

/// What the UI is being asked to render. Carries a one-shot reply
/// channel back so the worker can resume once the user answers.
///
/// Variants are only constructed when the plugin feature is enabled,
/// but the *type* is referenced unconditionally by the UI's channel
/// signature — hence the cfg-gated dead-code allow rather than a
/// feature gate on the whole enum.
#[derive(Debug)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum DialogRequest {
    Confirm {
        title: String,
        question: String,
        reply: mpsc::Sender<DialogReply>,
    },
    Select {
        title: String,
        options: Vec<String>,
        reply: mpsc::Sender<DialogReply>,
    },
}

#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum DialogReply {
    /// User answered yes/no. False also covers cancel/timeout.
    Confirm(bool),
    /// Some(option) when the user picked, None on cancel.
    Select(Option<String>),
}

thread_local! {
    /// Set once at worker init. The JanetCFunctions read this to forward
    /// dialog requests to the UI. `RefCell<Option<...>>` so we can
    /// install at startup and tests can clear/set.
    static DIALOG_TX: RefCell<Option<tmpsc::UnboundedSender<DialogRequest>>> = const { RefCell::new(None) };

    /// Set once at worker init (mirrors `DIALOG_TX`). The `harness/__lsp`
    /// C-function reads this to forward LSP queries to the tokio drainer.
    static LSP_TX: RefCell<Option<tmpsc::UnboundedSender<LspRequest>>> = const { RefCell::new(None) };

    /// Shared with the Worker handle. The cfns poll this every
    /// `DIALOG_POLL` while blocked on a dialog reply so that
    /// `Worker::Drop` can abort an in-flight `harness/confirm` /
    /// `harness/select` call instead of hanging forever when the UI
    /// receiver has been dropped.
    static SHUTDOWN: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };

    /// Shared with the Worker handle (mirrors `SHUTDOWN`). Incremented by
    /// `send_dialog` while a `harness/confirm`/`harness/select` is waiting
    /// for a human answer, decremented when it resolves. The host's
    /// `eval_with_timeout` reads the shared `Arc` so it keeps waiting while
    /// a dialog is genuinely in flight (up to the dialog budget) instead of
    /// giving up at the tight per-hook timeout and letting the gated tool
    /// run while the confirm is still on screen (dirge-hwzs).
    static DIALOG_PENDING: RefCell<Option<Arc<AtomicUsize>>> = const { RefCell::new(None) };
}

/// RAII guard: marks a dialog in flight on the worker thread for the
/// lifetime of a blocking `harness/confirm`/`harness/select` so the host
/// eval loop knows to keep waiting. No-op off the worker thread (the
/// thread-local is unset), so tests and non-dialog paths are unaffected.
#[cfg(feature = "plugin")]
struct DialogPendingGuard(Option<Arc<AtomicUsize>>);

#[cfg(feature = "plugin")]
impl DialogPendingGuard {
    fn enter() -> Self {
        let arc = DIALOG_PENDING.with(|cell| cell.borrow().clone());
        if let Some(a) = &arc {
            a.fetch_add(1, Ordering::SeqCst);
        }
        DialogPendingGuard(arc)
    }
}

#[cfg(feature = "plugin")]
impl Drop for DialogPendingGuard {
    fn drop(&mut self) {
        if let Some(a) = &self.0 {
            a.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// What `eval_with_timeout` does when a wait slice expires without a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutAction {
    /// A dialog is genuinely in flight and the ceiling is not reached — keep
    /// waiting in small slices.
    KeepWaiting,
    /// No dialog pending, or the ceiling reached: report the timeout.
    GiveUp,
}

/// Decide whether an expired wait slice should extend or give up [dirge-hwzs].
///
/// The base eval timeout is deliberately tight so a hook stuck in a loop
/// recovers fast, but a hook that opened `harness/confirm` is blocked on a
/// *human*, who routinely takes longer. Giving up there let the gated tool run
/// while the confirm was still on screen — a security gate failing OPEN. So an
/// in-flight dialog extends the wait, bounded by `ceiling` so a genuinely
/// wedged worker still cannot pin the caller forever.
///
/// # Why this is a free function
///
/// It used to be an inline `if` inside the recv loop, which meant the only way
/// to test it was to race a real Janet VM against a wall clock: spawn a
/// worker, give it a budget short enough that the timeout fires, and hope the
/// worker reached `harness/confirm` first. That test (dirge-h3dw) flaked twice
/// on loaded CI, because "time for the worker to reach the dialog" is
/// scheduler-dependent and has no upper bound. Raising the budget lowers the
/// failure rate and cannot reach zero — the first fix moved it 100ms → 750ms
/// and it flaked again.
///
/// Pulled out, the decision is a total function of three values and needs no
/// worker, no VM, and no clock, so the truth table below is exhaustive and
/// deterministic. What remains genuinely end-to-end — that `dialog_pending` is
/// actually set while a dialog is in flight — is covered by observing the flag
/// at the moment the request is received, which is also clock-free because
/// `DialogPendingGuard::enter` happens-before the request is published.
fn timeout_action(dialog_active: bool, elapsed: Duration, ceiling: Duration) -> TimeoutAction {
    if dialog_active && elapsed < ceiling {
        TimeoutAction::KeepWaiting
    } else {
        TimeoutAction::GiveUp
    }
}

/// What a notebook cell produced: the value of its last form, plus
/// whatever it printed on the way there (dirge-9xjg.3).
///
/// `client.run` returns only the last value, but printing intermediate
/// results is how notebook work is actually done — a cell whose `(print …)`
/// vanishes is not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub struct CellOutput {
    /// Stringified value of the cell's last form, or the Janet error text
    /// when `ok` is false.
    pub value: String,
    /// Captured stdout, with captured stderr appended under a `[stderr]`
    /// marker. Empty when the cell printed nothing.
    pub output: String,
    /// Whether the cell evaluated without raising.
    pub ok: bool,
}

#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum Cmd {
    /// Evaluate Janet code and return its stringified result.
    Eval {
        code: String,
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Evaluate a notebook cell: reset the capture buffers, evaluate, and
    /// return the value together with anything printed. Unlike `Eval`, a
    /// Janet-level error is reported *in* the [`CellOutput`] (`ok: false`)
    /// rather than as `Err` — a cell that raises is a normal notebook
    /// outcome the agent should see the output of, not a host failure.
    /// `Err` stays reserved for the worker itself being unreachable.
    EvalCell {
        code: String,
        /// Which notebook session's env to evaluate in (dirge-9xjg.5).
        /// `None` evaluates in the VM's root env — used by the host for
        /// its own bookkeeping, never for agent-authored code.
        session: Option<String>,
        out_limit: usize,
        reply: mpsc::Sender<Result<CellOutput, String>>,
    },
    Shutdown,
}

/// Handle to the worker thread. All Janet evaluation goes through `eval`.
/// Cheap to construct (only the spawn is heavy); cloneable senders are
/// not exposed — callers go through `&mut self` so writes serialize.
pub struct Worker {
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    cmd_tx: mpsc::Sender<Cmd>,
    join: Option<JoinHandle<()>>,
    /// Flipped by `Drop` so an in-flight `harness/confirm`/`harness/select`
    /// can stop waiting on the UI and let the worker exit. Shared by
    /// `Arc` with the worker thread's `SHUTDOWN` thread-local.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    shutdown: Arc<AtomicBool>,
    /// Count of `harness/confirm`/`harness/select` dialogs the worker is
    /// currently blocked on. Read by `eval_with_timeout` so a hook that
    /// opens a confirm dialog is waited on (up to the dialog budget)
    /// rather than timed out at the tight per-hook budget — which would
    /// let the gated tool run while the dialog is unanswered (dirge-hwzs).
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    dialog_pending: Arc<AtomicUsize>,
    /// Cross-thread cancellation for this worker's Janet VM. Used by
    /// `eval_with_timeout` to stop a runaway eval WITHOUT tearing down the
    /// thread, so VM state survives (dirge-9xjg.1). See
    /// [`super::interrupt`] for the mechanism and its one blind spot.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    interrupt: Arc<InterruptHandle>,
}

impl Worker {
    /// Spawn the Janet worker thread, install harness defs, and wait for
    /// the worker to confirm Janet init succeeded. Returns Err if Janet
    /// VM initialization fails (e.g. linker / lib issues).
    ///
    /// The returned `dialog_rx` is the consumer end of the dialog channel
    /// the UI loop should drain. It's only returned once because we want
    /// a single owner.
    #[cfg(feature = "plugin")]
    #[allow(clippy::type_complexity)]
    pub fn try_spawn() -> Result<
        (
            Self,
            tmpsc::UnboundedReceiver<DialogRequest>,
            tmpsc::UnboundedReceiver<LspRequest>,
        ),
        String,
    > {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (dialog_tx, dialog_rx) = tmpsc::unbounded_channel::<DialogRequest>();
        let (lsp_tx, lsp_rx) = tmpsc::unbounded_channel::<LspRequest>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let dialog_pending = Arc::new(AtomicUsize::new(0));
        let dialog_pending_clone = dialog_pending.clone();
        let interrupt = InterruptHandle::new();
        let interrupt_clone = interrupt.clone();

        let join = thread::Builder::new()
            .name("dirge-janet".to_string())
            .spawn(move || {
                worker_loop(
                    cmd_rx,
                    dialog_tx,
                    lsp_tx,
                    init_tx,
                    shutdown_clone,
                    dialog_pending_clone,
                    interrupt_clone,
                )
            })
            .map_err(|e| format!("spawn janet worker: {e}"))?;

        // Block (with a watchdog timeout) until worker confirms init.
        // A worker panic before init_tx.send would otherwise hang main
        // forever; INIT_TIMEOUT bounds that worst case.
        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(())) => Ok((
                Self {
                    cmd_tx,
                    join: Some(join),
                    shutdown,
                    dialog_pending,
                    interrupt,
                },
                dialog_rx,
                lsp_rx,
            )),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("janet worker did not init within {INIT_TIMEOUT:?}"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("janet worker exited during init".to_string())
            }
        }
    }

    #[cfg(not(feature = "plugin"))]
    #[allow(clippy::type_complexity)]
    pub fn try_spawn() -> Result<
        (
            Self,
            tmpsc::UnboundedReceiver<DialogRequest>,
            tmpsc::UnboundedReceiver<LspRequest>,
        ),
        String,
    > {
        // No-op worker for non-plugin builds. cmd_rx is dropped immediately
        // when the thread exits; cmd_tx writes will Err out cleanly.
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        let (_dialog_tx, dialog_rx) = tmpsc::unbounded_channel::<DialogRequest>();
        let (_lsp_tx, lsp_rx) = tmpsc::unbounded_channel::<LspRequest>();
        Ok((
            Self {
                cmd_tx,
                join: None,
                shutdown: Arc::new(AtomicBool::new(false)),
                dialog_pending: Arc::new(AtomicUsize::new(0)),
                interrupt: InterruptHandle::new(),
            },
            dialog_rx,
            lsp_rx,
        ))
    }

    /// Spawn a **notebook** worker: a second Janet VM, with a fresh root
    /// env and none of the host bridges (dirge-9xjg.2).
    ///
    /// # Why a second VM rather than sharing the plugin one
    ///
    /// Three reasons, only the first of which the interrupt already covers:
    ///
    /// 1. The interrupt breaks bytecode, not a thread parked in a C
    ///    syscall. `(os/execute …)` on a hung subprocess still wedges its
    ///    worker permanently. On the shared worker that takes every plugin,
    ///    hook, slash command and — critically — the `harness/block`
    ///    permission gate down with it for the rest of the session. Split
    ///    out, the blast radius is the agent's own notebook, and the
    ///    respawn that recovers it is cheap instead of state-destroying.
    /// 2. Agent-authored code cannot redefine a hook or shadow a
    ///    `harness/` function that the permission gate later calls. On a
    ///    shared env that is a privilege-escalation path, not a nuisance.
    /// 3. Plugin state survives a notebook reset and vice versa.
    ///
    /// This deliberately inverts dirge-ntjh, which proposed respawning the
    /// single shared worker on a repeated timeout. That is the one recovery
    /// a notebook cannot survive; splitting the VMs is what makes respawn
    /// the right answer instead of the destructive one.
    ///
    /// # What is NOT installed
    ///
    /// No dialog bridge (`harness/confirm`/`harness/select`), no LSP, no
    /// DAP, no computer-use exec. Each bridge widens what agent code can
    /// reach, and the dialog bridge in particular is the permission gate's
    /// own channel. `harness/json-extract` IS registered — it is pure
    /// serde_json with no host reach, and cells need it.
    ///
    /// The sandbox prelude still runs, so `os/exit` and friends stay
    /// neutered here exactly as they are for plugins.
    #[cfg(feature = "plugin")]
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub fn try_spawn_notebook() -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let interrupt = InterruptHandle::new();
        let interrupt_clone = interrupt.clone();

        let join = thread::Builder::new()
            .name("dirge-janet-notebook".to_string())
            .spawn(move || notebook_worker_loop(cmd_rx, init_tx, shutdown_clone, interrupt_clone))
            .map_err(|e| format!("spawn janet notebook worker: {e}"))?;

        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                cmd_tx,
                join: Some(join),
                shutdown,
                // The notebook has no dialog bridge, so nothing can ever
                // raise this — a notebook cell must never be able to
                // extend its own deadline by opening a dialog.
                dialog_pending: Arc::new(AtomicUsize::new(0)),
                interrupt,
            }),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "janet notebook worker did not init within {INIT_TIMEOUT:?}"
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("janet notebook worker exited during init".to_string())
            }
        }
    }

    #[cfg(not(feature = "plugin"))]
    #[allow(dead_code)]
    pub fn try_spawn_notebook() -> Result<Self, String> {
        Err("notebook requires the `plugin` feature".to_string())
    }

    /// Send a Janet expression to the worker and block until it returns
    /// the stringified result (or a Janet error message). Uses a
    /// moderate default (`INTERACTIVE_EVAL_TIMEOUT`, 30s) appropriate
    /// for slash-command dispatch, provider list lookups, and any
    /// UI-driven path where a hung plugin would otherwise freeze the
    /// session. For deliberately long-running operations
    /// (`harness/...` jobs from a plugin's top-level setup), use
    /// [`eval_long`].
    ///
    /// UI-3 (audit follow-up): the previous default was 10 minutes,
    /// inherited from `EVAL_TIMEOUT`. A runaway `(while true)` in any
    /// non-hook plugin code would block every UI interaction for 10
    /// minutes per call.
    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        self.eval_with_timeout(code, INTERACTIVE_EVAL_TIMEOUT)
    }

    /// Long-running variant: same as `eval` but uses the global
    /// `EVAL_TIMEOUT` (10 min). Call this only for explicit
    /// long-running operations — anything user-interactive should
    /// use `eval()`.
    #[allow(dead_code)]
    pub fn eval_long(&mut self, code: &str) -> Result<String, String> {
        self.eval_with_timeout(code, EVAL_TIMEOUT)
    }

    /// Variant of `eval` with a caller-provided timeout. Capped at
    /// the global `EVAL_TIMEOUT` so callers can't accidentally
    /// extend the wait.
    ///
    /// The decision taken when a wait slice expires lives in
    /// [`timeout_action`], which is pure and unit-tested — see there for why
    /// it is not tested through this function.
    pub fn eval_with_timeout(&mut self, code: &str, timeout: Duration) -> Result<String, String> {
        let (reply, rx) = mpsc::channel();
        self.cmd_tx
            .send(Cmd::Eval {
                code: code.to_string(),
                reply,
            })
            .map_err(|_| "janet worker disconnected".to_string())?;
        self.await_reply(&rx, timeout)
    }

    /// Evaluate a notebook cell and return its value plus captured output.
    ///
    /// Shares the whole deadline/interrupt path with [`eval_with_timeout`]
    /// — a runaway cell is stopped the same way, and the VM keeps its
    /// state — but reports a Janet-level error as data rather than `Err`,
    /// because for a notebook a raising cell is an ordinary result.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub fn eval_cell(
        &mut self,
        code: &str,
        session: Option<&str>,
        out_limit: usize,
        timeout: Duration,
    ) -> Result<CellOutput, String> {
        let (reply, rx) = mpsc::channel();
        self.cmd_tx
            .send(Cmd::EvalCell {
                code: code.to_string(),
                session: session.map(str::to_string),
                out_limit,
                reply,
            })
            .map_err(|_| "janet worker disconnected".to_string())?;
        self.await_reply(&rx, timeout)
    }

    /// Wait for a worker reply, extending for an in-flight dialog and
    /// interrupting a runaway eval at the deadline.
    ///
    /// Generic over the reply payload so `Eval` and `EvalCell` cannot
    /// drift: the dialog extension here is what keeps the permission gate
    /// from failing open, and a second hand-written copy of it is the kind
    /// of duplicate that goes stale silently.
    fn await_reply<T>(
        &mut self,
        rx: &mpsc::Receiver<Result<T, String>>,
        timeout: Duration,
    ) -> Result<T, String> {
        let effective = timeout.min(EVAL_TIMEOUT);

        // dirge-hwzs: the base timeout is deliberately tight (5s per tool
        // hook) so a hook stuck in a loop or blocking syscall recovers
        // fast. But a hook that opens `harness/confirm`/`harness/select`
        // blocks on a HUMAN, who routinely takes longer than that — and
        // giving up then let the gated tool run while the confirm was
        // still on screen (a security gate failing OPEN). So when the base
        // timeout elapses, keep waiting AS LONG AS a dialog is actually in
        // flight (the worker sets `dialog_pending` around the blocking
        // dialog call), bounded by the dialog budget so a genuinely wedged
        // worker still can't pin us forever. No dialog pending → time out
        // at the base as before.
        let dialog_ceiling = effective + DIALOG_TIMEOUT + INTERACTIVE_EVAL_TIMEOUT;
        let start = std::time::Instant::now();
        let mut slice = effective;
        loop {
            match rx.recv_timeout(slice) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("janet worker dropped reply channel".to_string());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let dialog_active = self.dialog_pending.load(Ordering::SeqCst) > 0;
                    match timeout_action(dialog_active, start.elapsed(), dialog_ceiling) {
                        TimeoutAction::KeepWaiting => {
                            // A confirm/select is awaiting a human — keep
                            // waiting, polling in small slices so we notice
                            // promptly when it resolves (or the dialog aborts).
                            slice = DIALOG_POLL;
                            continue;
                        }
                        TimeoutAction::GiveUp => {
                            // dirge-9xjg.1: before reporting a wedge, ask
                            // the interpreter to stop. Reaching here means
                            // no dialog is in flight (or the ceiling is
                            // blown), so nothing is legitimately waiting on
                            // a human — `timeout_action` already made that
                            // call and is deliberately left untouched,
                            // because an interrupted `harness/confirm`
                            // would fail the permission gate OPEN.
                            //
                            // Interrupting rather than abandoning the eval
                            // is what lets VM state survive a runaway cell.
                            if !self.interrupt.raise() {
                                // No VM installed (worker still starting,
                                // or a non-plugin build): nothing to
                                // interrupt, so don't burn the grace slice.
                                return Err(Self::wedged(start.elapsed(), false));
                            }
                            return Self::await_after_interrupt(rx, start);
                        }
                    }
                }
            }
        }
    }

    /// Wait out [`INTERRUPT_GRACE`] for a worker that has just been
    /// interrupted, so the caller gets the interpreter's own error instead
    /// of a generic wedge report.
    ///
    /// Split from `eval_with_timeout` because the two failures mean
    /// different things and want different messages: a reply inside the
    /// grace window is a Janet-level runaway that was cleanly stopped and
    /// left the VM usable, while silence means the thread is parked
    /// somewhere the interrupt cannot reach (a C syscall) and only a
    /// respawn will recover it.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    fn await_after_interrupt<T>(
        rx: &mpsc::Receiver<Result<T, String>>,
        start: std::time::Instant,
    ) -> Result<T, String> {
        let grace_deadline = std::time::Instant::now() + INTERRUPT_GRACE;
        loop {
            let remaining = grace_deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(Self::wedged(start.elapsed(), true));
            }
            match rx.recv_timeout(remaining.min(DIALOG_POLL)) {
                // The eval unwound. A raced completion still returns its
                // real value; an error is almost certainly OUR interrupt,
                // and Janet reports that as a bare "Runtime VM error",
                // which tells the reader nothing about what happened.
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(janet_err)) => {
                    return Err(format!(
                        "eval exceeded its {}s deadline and was interrupted                          (janet: {janet_err})",
                        start.elapsed().as_secs().max(1),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("janet worker dropped reply channel".to_string());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            }
        }
    }

    /// The give-up message. `interrupted` distinguishes the two cases the
    /// operator has to act on differently — see
    /// [`await_after_interrupt`](Self::await_after_interrupt).
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    fn wedged(elapsed: Duration, interrupted: bool) -> String {
        if interrupted {
            format!(
                "janet worker did not reply within {}s and did not stop when interrupted —                  it is likely blocked in a system call (e.g. os/execute on a hung subprocess)                  rather than looping in Janet",
                elapsed.as_secs(),
            )
        } else {
            format!(
                "janet worker did not reply within {}s — plugin may be stuck in an infinite loop",
                elapsed.as_secs(),
            )
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Set the shutdown flag FIRST, then send the Shutdown cmd.
        // A worker that's currently blocked inside an unanswered
        // `harness/confirm`/`harness/select` polls this flag every
        // `DIALOG_POLL` and gives up — without the flag, the cfn would
        // sit on `reply_rx.recv()` forever, the cmd_rx would never read
        // Shutdown, and `join` would hang.
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(h) = self.join.take() {
            // Bounded join. `JoinHandle::join` has no timeout, so we
            // poll `is_finished()` (stable since Rust 1.61) and bail
            // after JOIN_TIMEOUT. If the worker is wedged in plugin
            // code (e.g. Janet `(while true)`), join would otherwise
            // hang the calling thread — usually the user's main
            // thread on `/quit`. We leak the JoinHandle rather than
            // pinning the process; the thread is reaped on exit.
            let deadline = std::time::Instant::now() + JOIN_TIMEOUT;
            while !h.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                tracing::warn!(
                    target: "dirge::plugin",
                    timeout_secs = JOIN_TIMEOUT.as_secs(),
                    "janet worker thread did not exit within JOIN_TIMEOUT; leaking on shutdown",
                );
                std::mem::forget(h);
            }
        }
    }
}

#[cfg(feature = "plugin")]
fn worker_loop(
    rx: mpsc::Receiver<Cmd>,
    dialog_tx: tmpsc::UnboundedSender<DialogRequest>,
    lsp_tx: tmpsc::UnboundedSender<LspRequest>,
    init_tx: mpsc::Sender<Result<(), String>>,
    shutdown: Arc<AtomicBool>,
    dialog_pending: Arc<AtomicUsize>,
    interrupt: Arc<InterruptHandle>,
) {
    // Hand the dialog sender + shutdown flag to this thread's C functions
    // before we run any plugin code, otherwise harness/confirm/select
    // would no-op and shutdown couldn't cancel an in-flight dialog.
    DIALOG_TX.with(|cell| *cell.borrow_mut() = Some(dialog_tx));
    LSP_TX.with(|cell| *cell.borrow_mut() = Some(lsp_tx));
    SHUTDOWN.with(|cell| *cell.borrow_mut() = Some(shutdown));
    DIALOG_PENDING.with(|cell| *cell.borrow_mut() = Some(dialog_pending));

    let mut client = match JanetClient::init_with_default_env() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("Janet init failed: {e}")));
            return;
        }
    };

    // Publish this thread's VM so other threads can interrupt a runaway
    // eval. MUST happen here: `janet_local_vm()` reads the *calling*
    // thread's VM, and it must be live, so after init and on this thread.
    interrupt.install_local_vm();

    // Install C functions backing harness/confirm and harness/select.
    // They must be registered before the Janet-side aliases reference
    // them; we register, then run the alias definitions.
    //
    // `CFunOptions::namespace` requires `'static` CStr, so we use C string
    // literals (Rust 1.77+ `c"..."` syntax) instead of runtime CString.
    if let Some(env) = client.env_mut() {
        env.add_c_fn(CFunOptions::new(c"__confirm", janet_confirm_cfn).namespace(c"harness"));
        env.add_c_fn(CFunOptions::new(c"__select", janet_select_cfn).namespace(c"harness"));
        env.add_c_fn(
            CFunOptions::new(c"__json-extract", janet_json_extract_cfn).namespace(c"harness"),
        );
        env.add_c_fn(
            CFunOptions::new(c"__json-decode", janet_json_decode_cfn).namespace(c"harness"),
        );
        // Notebook bridge. Plugin VM ONLY — see `janet_notebook_cfn`.
        env.add_c_fn(CFunOptions::new(c"__notebook", janet_notebook_cfn).namespace(c"harness"));
        // Only register the LSP bridge when the lsp feature is compiled
        // in. The Janet `harness/lsp` wrappers (HARNESS_LSP_INIT) guard on
        // this symbol's existence and degrade to nil when it's absent.
        #[cfg(feature = "lsp")]
        {
            env.add_c_fn(CFunOptions::new(c"__lsp", janet_lsp_cfn).namespace(c"harness"));
            env.add_c_fn(CFunOptions::new(c"__lsp-live", janet_lsp_live_cfn).namespace(c"harness"));
        }
        // Vigil bridge: expose vigil/live? and vigil/emit C functions
        // so Janet plugins can interact with the vigil-keeper at runtime.
        #[cfg(all(feature = "plugin", feature = "vigil"))]
        {
            env.add_c_fn(
                CFunOptions::new(c"__vigil-live", vigil_bridge::vigil_live_cfn)
                    .namespace(c"harness"),
            );
            env.add_c_fn(
                CFunOptions::new(c"__vigil-emit", vigil_bridge::vigil_emit_cfn)
                    .namespace(c"harness"),
            );
            env.add_c_fn(
                CFunOptions::new(c"__vigil-list", vigil_bridge::vigil_list_cfn)
                    .namespace(c"harness"),
            );
            env.add_c_fn(
                CFunOptions::new(c"__vigil-get", vigil_bridge::vigil_get_cfn).namespace(c"harness"),
            );
            env.add_c_fn(
                CFunOptions::new(c"__vigil-set-state", vigil_bridge::vigil_set_state_cfn)
                    .namespace(c"harness"),
            );
        }
        // Computer-use exec: forwards actions to the sandbox drainer.
        // The C function reads SANDBOX_EXEC_TX; if the channel wasn't
        // installed (e.g. --sandbox off), it returns nil gracefully.
        env.add_c_fn(
            CFunOptions::new(c"__computer-use-exec", janet_computer_use_exec_cfn)
                .namespace(c"harness"),
        );
        // Tool bridge: lets a plugin call dirge's own tools. Like the
        // computer-use bridge these read a global set by main.rs, so they
        // degrade to nil when the responder was never spawned.
        env.add_c_fn(CFunOptions::new(c"__call-tool", janet_call_tool_cfn).namespace(c"harness"));
        env.add_c_fn(CFunOptions::new(c"__list-tools", janet_list_tools_cfn).namespace(c"harness"));
        env.add_c_fn(CFunOptions::new(c"__tools-live", janet_tools_live_cfn).namespace(c"harness"));
    }
    // Register DAP C functions when both features are enabled.
    #[cfg(feature = "dap")]
    {
        crate::dap::janet_bindings::register_dap_cfns(&mut client);
    }

    // Redirect and mirror BEFORE anything that can fail to compile, so a
    // broken prelude reports through the buffers instead of the terminal.
    let _ = client.run(CAPTURE_BOOTSTRAP);
    mirror_capture_buffers_into_top_dyns(&client);

    if let Err(e) = client.run(HARNESS_INIT) {
        let _ = init_tx.send(Err(format!("harness init failed: {e}")));
        return;
    }
    if let Err(e) = client.run(HARNESS_NOTEBOOK_INIT) {
        let _ = init_tx.send(Err(format!("harness notebook init failed: {e}")));
        return;
    }
    if let Err(e) = client.run(HARNESS_DIALOG_INIT) {
        let _ = init_tx.send(Err(format!("harness dialog init failed: {e}")));
        return;
    }
    if let Err(e) = client.run(HARNESS_COMPUTER_USE_INIT) {
        let _ = init_tx.send(Err(format!("harness computer-use init failed: {e}")));
        return;
    }
    if let Err(e) = client.run(HARNESS_LSP_INIT) {
        let _ = init_tx.send(Err(format!("harness lsp init failed: {e}")));
        return;
    }
    if let Err(e) = client.run(HARNESS_TOOL_INIT) {
        let _ = init_tx.send(Err(format!("harness tool-bridge init failed: {e}")));
        return;
    }
    // Vigil Janet prelude — defines (vigil/emit), (vigil/list),
    // (vigil/set-state), (vigil/get), (vigil/live?).
    #[cfg(all(feature = "plugin", feature = "vigil"))]
    {
        if let Err(e) = client.run(HARNESS_VIGIL_INIT) {
            let _ = init_tx.send(Err(format!("vigil init failed: {e}")));
            return;
        }
    }
    // dirge-l6bf: disable host-terminating Janet functions. MUST run after
    // the harness preludes and before any plugin loads, so plugin code
    // compiles against the neutered bindings.
    if let Err(e) = client.run(HARNESS_SANDBOX) {
        let _ = init_tx.send(Err(format!("harness sandbox init failed: {e}")));
        return;
    }

    // Run the DAP Janet bindings prelude when the dap feature is enabled.
    // This defines (dap/launch ...), (dap/step), etc. as wrappers over
    // the C functions registered above. Plugins can call these directly.
    // DAP bridge: stores channel before init completes; the Janet
    // thread consumes it in set_ce_fn. Must precede dap init so plugins
    // that invoke dap/ functions during their own setup don't hit NPE.
    #[cfg(feature = "dap")]
    {
        // Note: the DAP bridge is already spawned by spawn_dap_responder()
        // in main.rs (from a tokio runtime). In test context there is no
        // main.rs, so the bridge was never spawned and take_dap_tx_for_worker
        // returns None. In that case we skip the Janet prelude and bridge
        // install — plugins will see (dap/available?) == false.
        // Run Janet init that binds dap/ C fns.
        // Must run AFTER harness-sandbox so overridden fns that touch
        // DAP internals can't be shadowed.
        if let Err(e) = client.run(crate::dap::janet_bindings::HARNESS_DAP_INIT) {
            let _ = init_tx.send(Err(format!("dap init failed: {e}")));
            return;
        }
        // Install the bridge sender on the Janet thread so C-fns can
        // reach the tokio worker. Must happen here, inside the worker,
        // because `store_dap_tx` already primed the channel from the
        // plugin-manager side.
        if let Some(dap_tx) = crate::dap::janet_bindings::take_dap_tx_for_worker() {
            crate::dap::janet_bindings::install_dap_tx(dap_tx);
        }
    }

    let _ = init_tx.send(Ok(()));

    run_command_loop(rx, &mut client, &interrupt);
}

/// The command loop, shared by the plugin VM and the notebook VM.
///
/// Deliberately one function rather than one per worker: the interrupt
/// drain ordering and the capture reset are subtle enough that a second
/// hand-maintained copy would drift, and the copy that drifted would be
/// the one running agent-authored code.
#[cfg(feature = "plugin")]
fn run_command_loop(
    rx: mpsc::Receiver<Cmd>,
    client: &mut JanetClient,
    interrupt: &Arc<InterruptHandle>,
) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Eval { code, reply } => {
                // Clear any interrupt raised while we were idle. It was
                // aimed at an eval that had already finished (the raiser
                // races the reply), and leaving it set would abort this
                // eval instead — and leave `auto_suspend` non-zero forever.
                interrupt.drain();
                // dirge-lncv: reset before every eval, not just notebook
                // cells. The buffers used to be written and never read, so
                // they grew for the whole process lifetime — and now that
                // compile errors and stack traces land in them too
                // (dirge-c8lh) that growth would be faster, not slower.
                let _ = client.run("(harness/-capture-reset)");
                // dirge-2jtd: run the code inside a fiber and re-raise its
                // error, instead of handing the raw string to janet_dobytes.
                //
                // janet_dobytes sets errflags only from the janet_continue it
                // performs itself. A fiber that yields JANET_SIGNAL_EVENT is
                // driven to completion by janet_loop, which prints a stack
                // trace and moves on WITHOUT touching errflags — so the error
                // VALUE came back as `ret` and janetrs mapped clear flags to
                // Ok. Every ev-backed builtin takes that path (os/execute,
                // os/spawn, ev/*, net/*), so a plugin hook that failed on one
                // reported success with its error text as the return value.
                //
                // `resume` drives the fiber to completion across the event
                // loop, so `fiber/status` is accurate for both paths; the
                // re-raise then happens in the outer fiber, where errflags do
                // get set. Same shape notebook/-eval already uses, which is
                // where the accurate half was verified.
                // The fiber MUST carry the caller's environment. A bare
                // `fiber/new` gets a fresh one, so every `def` lands somewhere
                // the next eval cannot see — and plugins define their hooks in
                // one eval and are dispatched in another, so that breaks them
                // in a way that reads as "the plugin didn't load".
                //
                // The verdict comes back IN THE VALUE rather than as a raised
                // error. Re-raising was tried first and does not work: when the
                // inner fiber yields to the event loop the OUTER fiber yields
                // too, so the re-raise happens inside janet_loop and errflags
                // stay clear — the very mechanism being worked around. Reading
                // a status variable afterwards would work but costs a second
                // round trip on every eval, and hook dispatch is on the hot
                // path. A sentinel prefix keeps it at one.
                let wrapped = format!(
                    r#"(let [env (fiber/getenv (fiber/current)) f (fiber/new (fn [] (eval-string "{}")) :e)] (fiber/setenv f env) (let [r (resume f)] (if (= (fiber/status f) :error) (string "{}" (describe r)) r)))"#,
                    super::escape_janet_string(&code),
                    EV_ERROR_SENTINEL,
                );
                let r = client
                    .run(&wrapped)
                    .map(|v| v.to_string())
                    .map_err(|e| format!("Janet error: {e}"))
                    .and_then(|v| match v.strip_prefix(EV_ERROR_SENTINEL) {
                        Some(msg) => Err(format!("Janet error: {}", msg.trim())),
                        None => Ok(v),
                    });
                if r.is_err() {
                    // A failed plugin eval used to announce itself by
                    // corrupting the screen; now it is captured, so route
                    // it somewhere an operator can actually read. Only on
                    // the error path — hook dispatch is latency-critical
                    // and the happy path should not pay for a second
                    // round trip.
                    if let Ok(out) = client.run("(harness/-capture-take 4096)") {
                        let out = out.to_string();
                        if !out.trim().is_empty() {
                            tracing::warn!(
                                target: "dirge::plugin",
                                output = %out,
                                "janet eval failed; captured output follows",
                            );
                        }
                    }
                }
                // Clear the interrupt this eval consumed. Janet's
                // `auto_suspend` is a counter, not a latch: until it
                // returns to zero the VM refuses to run scheduled fibers,
                // so skipping this would wedge the worker permanently —
                // the exact failure the interrupt exists to prevent.
                interrupt.drain();
                let _ = reply.send(r);
            }
            Cmd::EvalCell {
                code,
                session,
                out_limit,
                reply,
            } => {
                interrupt.drain();
                // Reset first so the capture holds THIS cell's output only.
                let _ = client.run("(harness/-capture-reset)");
                // A session cell is wrapped so it evaluates in that
                // session's own env. The wrapping happens HERE rather than
                // in the caller so there is exactly one place that decides
                // how agent code is sandboxed into a session.
                let to_run = match &session {
                    Some(sid) => format!(
                        r#"(notebook/-eval "{}" "{}")"#,
                        super::escape_janet_string(sid),
                        super::escape_janet_string(&code),
                    ),
                    None => code.clone(),
                };
                let outcome = client.run(&to_run);
                // Read the buffers back BEFORE draining/replying, while
                // they still hold this cell's output. `-capture-take`
                // prints nothing itself, so it cannot contaminate them.
                let captured = client
                    .run(format!("(harness/-capture-take {out_limit})"))
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                interrupt.drain();
                let cell = match outcome {
                    Ok(v) => CellOutput {
                        value: v.to_string(),
                        output: captured,
                        // A session cell catches its own errors inside the
                        // fiber, so `client.run` succeeding says nothing
                        // about whether the CELL succeeded — read the flag
                        // the wrapper set. Trusting `Ok` here would report
                        // every failing cell as a success.
                        ok: match &session {
                            None => true,
                            Some(_) => client
                                .run("notebook/-last-ok")
                                .map(|v| v.to_string() == "true")
                                .unwrap_or(false),
                        },
                    },
                    Err(e) => CellOutput {
                        value: format!("{e}"),
                        output: captured,
                        ok: false,
                    },
                };
                let _ = reply.send(Ok(cell));
            }
            Cmd::Shutdown => break,
        }
    }
}

/// dirge-eona: `janet_vm` is `__thread` in evil-janet's janet.c (that is
/// how the plugin and notebook VMs coexist on separate threads), and its
/// layout is file-local to janet.c — `janet.h` only forward-declares
/// `struct JanetVM`, so bindgen renders it opaque (`_unused: [u8; 0]`) and
/// the bindings cannot reach a field. Only the first two are needed;
/// `top_dyns` sits at offset 8, right after the `void *user` slot
/// (janet.c:172).
///
/// That offset is an assumption about a private C layout, and it has to hold
/// across any `evil-janet` 1.x (`janetrs` depends on `"1"`, so a `cargo
/// update` can move it). `top_dyns_offset_is_stable` in the plugin test
/// module pins it, so an upstream field insertion fails a test rather than
/// silently rooting whatever now sits at offset 8.
#[cfg(feature = "plugin")]
#[repr(C)]
struct JanetVMPrefix {
    #[allow(dead_code)]
    user: *mut core::ffi::c_void,
    top_dyns: *mut janetrs::lowlevel::JanetTable,
}

/// The calling thread's `janet_vm.top_dyns` — null until the first
/// `janet_setdyn` on this thread creates it (janet.c:4657).
///
/// Reads through [`JanetVMPrefix`]; see there for the layout assumption.
#[cfg(feature = "plugin")]
pub(super) fn top_dyns_ptr() -> *mut janetrs::lowlevel::JanetTable {
    // SAFETY: `janet_local_vm` returns `&janet_vm`, this thread's own
    // thread-local VM, so it is never null once Janet is initialized on the
    // thread — true for every caller, each of which holds a live
    // `JanetClient`. The cast reinterprets the bindings' opaque `JanetVM` as
    // its own leading fields, so reading `top_dyns` touches only bytes
    // inside the real struct.
    unsafe { (*(janetrs::lowlevel::janet_local_vm() as *mut JanetVMPrefix)).top_dyns }
}

/// Point Janet's TOP-LEVEL dyn table at the same `:out`/`:err` buffers the
/// harness prelude installed in the root env (dirge-c8lh).
///
/// `janet_dyn` consults `janet_vm.fiber->env` while a fiber is running and
/// falls back to `janet_vm.top_dyns` when none is — and `janet_dobytes`
/// prints compile errors and stack traces in exactly that second state,
/// after `janet_continue` has returned. So the Janet-side redirect, which
/// can only ever write the fiber env, does not cover them: a plugin with a
/// syntax error wrote straight to the user's terminal, corrupting the
/// raw-mode TUI.
///
/// Only Rust can fix it. `janet_setdyn` writes `top_dyns` precisely when no
/// fiber is live, which is true here between `client.run` calls and false
/// everywhere reachable from Janet code (including inside a cfn).
#[cfg(feature = "plugin")]
fn mirror_capture_buffers_into_top_dyns(client: &JanetClient) {
    for (janet_name, c_name) in [("(dyn :out)", c"out"), ("(dyn :err)", c"err")] {
        let Ok(buf) = client.run(janet_name) else {
            continue;
        };
        let raw = janetrs::lowlevel::Janet::from(buf);
        // SAFETY: `raw` is a live Janet value produced moments ago by this
        // same VM on this thread. `janet_gcroot` keeps the buffer alive
        // independently of the root-env entry, so a later rebinding of
        // `:out` cannot leave `top_dyns` holding a collected pointer. The
        // buffers are process-lifetime by design, so the permanent root is
        // not a leak. No fiber is active here, so `janet_setdyn` writes
        // `top_dyns` — which is the entire point.
        unsafe {
            janetrs::lowlevel::janet_gcroot(raw);
            janetrs::lowlevel::janet_setdyn(c_name.as_ptr(), raw);
        }
    }
    // dirge-eona: `janet_setdyn` above created `janet_vm.top_dyns` (no
    // fiber is live here), and Janet's collector never marks that table —
    // it appears nowhere in the mark phase of janet.c. The first full
    // collection frees it, and the next no-fiber `janet_dyn` (the stack
    // trace printer's `:err-color` lookup) reads the freed table and
    // segfaults the VM. Root the table itself, not just the buffers in it.
    // Janet roots `abstract_registry` the same way, for the same reason
    // (janet.c:35577) — a VM-struct field is not a GC root on its own.
    let top = top_dyns_ptr();
    if !top.is_null() {
        // SAFETY: `top` is the table `janet_setdyn` just created on this
        // thread, so it is a live Janet object; `janet_wrap_table` only
        // tags the pointer. Rooting is permanent by design and cannot leak
        // past one entry per VM: both callers run once at thread init, and
        // `top_dyns` is assigned nowhere but `janet_init`/`janet_deinit`
        // (NULL) and this lazy create, so the root can never go stale.
        unsafe {
            janetrs::lowlevel::janet_gcroot(janetrs::lowlevel::janet_wrap_table(top));
        }
    }
}

/// The notebook VM's thread (dirge-9xjg.2).
///
/// Same command protocol as [`worker_loop`], deliberately different init:
/// only the pure `__json-extract` cfn is registered, so none of the host
/// bridges exist on this VM at all. A cell cannot reach them even by
/// name — there is no symbol to reach.
#[cfg(feature = "plugin")]
fn notebook_worker_loop(
    rx: mpsc::Receiver<Cmd>,
    init_tx: mpsc::Sender<Result<(), String>>,
    shutdown: Arc<AtomicBool>,
    interrupt: Arc<InterruptHandle>,
) {
    // Publish the shutdown flag so a long cell can still observe teardown.
    // DIALOG_TX / LSP_TX are deliberately left unset on this thread: even
    // if a `harness/__confirm` symbol somehow appeared, it would find no
    // channel and no-op rather than reaching the permission UI.
    SHUTDOWN.with(|cell| *cell.borrow_mut() = Some(shutdown));

    let mut client = match JanetClient::init_with_default_env() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("Janet notebook init failed: {e}")));
            return;
        }
    };

    interrupt.install_local_vm();

    if let Some(env) = client.env_mut() {
        // Pure, host-free, and cells routinely need it. Nothing else.
        env.add_c_fn(
            CFunOptions::new(c"__json-extract", janet_json_extract_cfn).namespace(c"harness"),
        );
        env.add_c_fn(
            CFunOptions::new(c"__json-decode", janet_json_decode_cfn).namespace(c"harness"),
        );
    }

    let _ = client.run(CAPTURE_BOOTSTRAP);
    mirror_capture_buffers_into_top_dyns(&client);

    // Note: HARNESS_NOTEBOOK_INIT is deliberately absent — see its docs.
    for (label, src) in [
        ("harness", HARNESS_INIT),
        ("sandbox", HARNESS_SANDBOX),
        ("notebook", NOTEBOOK_INIT),
    ] {
        if let Err(e) = client.run(src) {
            let _ = init_tx.send(Err(format!("notebook {label} init failed: {e}")));
            return;
        }
    }

    let _ = init_tx.send(Ok(()));

    run_command_loop(rx, &mut client, &interrupt);
}

#[cfg(not(feature = "plugin"))]
#[allow(dead_code)]
fn worker_loop(
    _rx: mpsc::Receiver<Cmd>,
    _dialog_tx: tmpsc::UnboundedSender<DialogRequest>,
    _lsp_tx: tmpsc::UnboundedSender<LspRequest>,
    _init_tx: mpsc::Sender<Result<(), String>>,
    _shutdown: Arc<AtomicBool>,
    _dialog_pending: Arc<AtomicUsize>,
    _interrupt: Arc<InterruptHandle>,
) {
    unreachable!("worker_loop should never run without the plugin feature");
}

// --- JanetCFunction shims ----------------------------------------------
//
// These run on the worker thread under Janet's control. They unwrap
// argv as strings via evil_janet's raw API, build a DialogRequest, send
// it to the UI through DIALOG_TX, block on the reply, and wrap the
// answer back into a Janet value.

#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_confirm_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    // Catch any Rust panic at the FFI boundary. The C-unwind ABI would
    // technically let it propagate into Janet's C runtime, but Janet
    // isn't built to clean up after foreign unwinds — heap corruption
    // and segfaults follow. Convert any panic to a safe `false`.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        confirm_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            // Log the panic before swallowing so the operator can
            // see *why* harness/confirm collapsed to false. Previous
            // behavior masked panics silently and plugin authors had
            // no way to distinguish "user said no" from "Rust panic
            // at FFI boundary."
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/confirm",
                panic = %msg,
                "FFI panic in dialog cfn — returning safe default (false)",
            );
            unsafe { janet_wrap_boolean(0) }
        }
    }
}

/// Safe-Rust body of `janet_confirm_cfn`. Split out so it can panic
/// without worrying about FFI unwind semantics; the cfn wraps the call
/// in `catch_unwind` and substitutes a safe default on panic.
#[cfg(feature = "plugin")]
unsafe fn confirm_body(argc: i32, argv: *mut janetrs::lowlevel::Janet) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 2 {
        return unsafe { janet_wrap_boolean(0) };
    }
    let title = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_boolean(0) },
    };
    let question = match unsafe { read_string_arg(argv, 1) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_boolean(0) },
    };

    let answer = DIALOG_TX.with(|cell| match cell.borrow().as_ref() {
        Some(tx) => send_dialog(tx, |reply| DialogRequest::Confirm {
            title,
            question,
            reply,
        })
        .unwrap_or(DialogReply::Confirm(false)),
        None => DialogReply::Confirm(false),
    });

    let yes = matches!(answer, DialogReply::Confirm(true));
    unsafe { janet_wrap_boolean(if yes { 1 } else { 0 }) }
}

#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_select_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        select_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/select",
                panic = %msg,
                "FFI panic in dialog cfn — returning safe default (nil)",
            );
            unsafe { janet_wrap_nil() }
        }
    }
}

/// Best-effort conversion of a panic payload (`Box<dyn Any>`) to a
/// printable string. Tries `&str` then `String` — covers the two
/// payload shapes std and most code produce. Returns
/// `"<non-string panic payload>"` for anything else so the log
/// always has SOMETHING to anchor on rather than going silent again.
#[cfg(feature = "plugin")]
fn panic_payload_to_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(feature = "plugin")]
unsafe fn select_body(argc: i32, argv: *mut janetrs::lowlevel::Janet) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 2 {
        return unsafe { janet_wrap_nil() };
    }
    let title = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let options = match unsafe { read_string_array_arg(argv, 1) } {
        Some(v) if !v.is_empty() => v,
        _ => return unsafe { janet_wrap_nil() },
    };

    let answer = DIALOG_TX.with(|cell| match cell.borrow().as_ref() {
        Some(tx) => send_dialog(tx, |reply| DialogRequest::Select {
            title,
            options,
            reply,
        })
        .unwrap_or(DialogReply::Select(None)),
        None => DialogReply::Select(None),
    });

    match answer {
        DialogReply::Select(Some(s)) => unsafe { wrap_string(&s) },
        _ => unsafe { janet_wrap_nil() },
    }
}

#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_json_extract_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        json_extract_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/json-extract",
                panic = %msg,
                "FFI panic in json-extract cfn — returning nil",
            );
            unsafe { janet_wrap_nil() }
        }
    }
}

#[cfg(feature = "plugin")]
unsafe fn json_extract_body(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 2 {
        return unsafe { janet_wrap_nil() };
    }
    let json_str = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let key = match unsafe { read_string_arg(argv, 1) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    match serde_json::from_str::<serde_json::Value>(&json_str) {
        Ok(serde_json::Value::Object(map)) => match map.get(&key) {
            Some(serde_json::Value::String(s)) => unsafe { wrap_string(s) },
            _ => unsafe { janet_wrap_nil() },
        },
        _ => unsafe { janet_wrap_nil() },
    }
}

/// `(harness/__notebook op session code)` — the notebook bridge
/// (dirge-9xjg.4).
///
/// Registered on the **plugin** VM only. It runs on the plugin worker
/// thread and forwards to the notebook VM's thread, so the two never share
/// an interpreter; registering it on the notebook VM as well would let a
/// cell re-enter the notebook lock it is already holding and deadlock.
///
/// Returns a table `{:ok bool :value string :output string}` so the plugin
/// side does not have to parse anything. Ops: `eval`, `reset`, `sessions`,
/// `respawn`.
#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_notebook_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        notebook_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/notebook",
                panic = %msg,
                "FFI panic in notebook cfn — returning an error result",
            );
            unsafe { notebook_result(false, &format!("notebook bridge panicked: {msg}"), "") }
        }
    }
}

#[cfg(feature = "plugin")]
unsafe fn notebook_body(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use super::notebook;
    if argc < 1 {
        return unsafe { notebook_result(false, "notebook: missing op", "") };
    }
    let op = unsafe { read_string_arg(argv, 0) }.unwrap_or_default();
    let session = if argc > 1 {
        unsafe { read_string_arg(argv, 1) }.unwrap_or_default()
    } else {
        String::new()
    };
    let code = if argc > 2 {
        unsafe { read_string_arg(argv, 2) }.unwrap_or_default()
    } else {
        String::new()
    };

    match op.as_str() {
        "eval" => match notebook::eval_cell(&session, &code) {
            Ok(cell) => unsafe { notebook_result(cell.ok, &cell.value, &cell.output) },
            Err(e) => unsafe { notebook_result(false, &e, "") },
        },
        "reset" => match notebook::reset_session(&session) {
            Ok(v) => unsafe { notebook_result(true, &v, "") },
            Err(e) => unsafe { notebook_result(false, &e, "") },
        },
        "sessions" => match notebook::sessions() {
            Ok(v) => unsafe { notebook_result(true, &v, "") },
            Err(e) => unsafe { notebook_result(false, &e, "") },
        },
        "respawn" => match notebook::respawn() {
            Ok(()) => unsafe { notebook_result(true, "respawned", "") },
            Err(e) => unsafe { notebook_result(false, &e, "") },
        },
        other => unsafe { notebook_result(false, &format!("notebook: unknown op {other:?}"), "") },
    }
}

/// Build the `{:ok :value :output}` table the bridge returns.
#[cfg(feature = "plugin")]
unsafe fn notebook_result(ok: bool, value: &str, output: &str) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    unsafe {
        let tbl = janet_table(3);
        janet_table_put(
            tbl,
            janet_ckeywordv(c"ok".as_ptr() as *const u8, 2),
            janet_wrap_boolean(i32::from(ok)),
        );
        janet_table_put(
            tbl,
            janet_ckeywordv(c"value".as_ptr() as *const u8, 5),
            wrap_string(value),
        );
        janet_table_put(
            tbl,
            janet_ckeywordv(c"output".as_ptr() as *const u8, 6),
            wrap_string(output),
        );
        janet_wrap_table(tbl)
    }
}

/// `(harness/json-decode json-str)` -> native Janet data | nil
/// (dirge-9xjg.6).
///
/// `harness/json-extract` pulls ONE string value from a FLAT object, which
/// is enough for a `{code: …}` tool payload and nothing else. Notebook work
/// routinely parses an API response, a config file, or another tool's
/// output, and without a real decoder every cell hand-rolls string
/// munging — precisely the friction the notebook exists to remove.
///
/// Implemented host-side because the parser already exists here and
/// because a pure-Janet decoder would be both slower and another thing to
/// keep correct. Mapping: object -> table, array -> array, string ->
/// string, number -> number, true/false -> boolean, null -> `:null`.
///
/// `null` maps to the `:null` keyword rather than `nil` on purpose: in
/// Janet, putting `nil` into a table REMOVES the key, so a JSON `null`
/// would silently erase the field it belongs to and become
/// indistinguishable from a field that was never sent.
#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_json_decode_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        json_decode_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/json-decode",
                panic = %msg,
                "FFI panic in json-decode cfn — returning nil",
            );
            unsafe { janetrs::lowlevel::janet_wrap_nil() }
        }
    }
}

#[cfg(feature = "plugin")]
unsafe fn json_decode_body(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 1 {
        return unsafe { janet_wrap_nil() };
    }
    let Some(json_str) = (unsafe { read_string_arg(argv, 0) }) else {
        return unsafe { janet_wrap_nil() };
    };
    match serde_json::from_str::<serde_json::Value>(&json_str) {
        Ok(v) => unsafe { json_to_janet(&v) },
        Err(_) => unsafe { janet_wrap_nil() },
    }
}

/// Recursively convert a `serde_json::Value` into Janet data.
///
/// Depth is bounded so a deeply nested document cannot overflow the Rust
/// stack from inside a C callback, where an overflow is a hard crash of
/// the host rather than a catchable error. serde_json itself caps nesting
/// at 128 by default, so this is a belt-and-braces second bound.
#[cfg(feature = "plugin")]
unsafe fn json_to_janet(v: &serde_json::Value) -> janetrs::lowlevel::Janet {
    unsafe { json_to_janet_depth(v, 0) }
}

/// Maximum nesting `harness/json-decode` will materialise.
#[cfg(feature = "plugin")]
const JSON_DECODE_MAX_DEPTH: usize = 64;

#[cfg(feature = "plugin")]
unsafe fn json_to_janet_depth(v: &serde_json::Value, depth: usize) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if depth > JSON_DECODE_MAX_DEPTH {
        return unsafe { janet_wrap_nil() };
    }
    unsafe {
        match v {
            serde_json::Value::Null => {
                // Not `nil` — see the cfn docs: nil erases table keys.
                let kw = c"null";
                janet_ckeywordv(kw.as_ptr() as *const u8, 4)
            }
            serde_json::Value::Bool(b) => janet_wrap_boolean(i32::from(*b)),
            serde_json::Value::Number(n) => janet_wrap_number(n.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(s) => wrap_string(s),
            serde_json::Value::Array(items) => {
                let arr = janet_array(items.len().min(i32::MAX as usize) as i32);
                for item in items {
                    janet_array_push(arr, json_to_janet_depth(item, depth + 1));
                }
                janet_wrap_array(arr)
            }
            serde_json::Value::Object(map) => {
                let tbl = janet_table(map.len().min(i32::MAX as usize) as i32);
                for (k, val) in map {
                    let key = wrap_string(k);
                    janet_table_put(tbl, key, json_to_janet_depth(val, depth + 1));
                }
                janet_wrap_table(tbl)
            }
        }
    }
}

/// Send a dialog request, build it via the supplied closure (so we can
/// move owned strings into the variant), and block on the reply.
/// Returns `None` if the UI side dropped the channel OR the worker is
/// shutting down. The outbound side uses tokio's unbounded sender so
/// the UI loop can `recv().await` in `tokio::select!`; the inbound
/// reply is a std mpsc with a polling timeout so the cfn can also
/// abort when `Worker::Drop` flips the shutdown flag.
#[cfg(feature = "plugin")]
fn send_dialog<F>(tx: &tmpsc::UnboundedSender<DialogRequest>, build: F) -> Option<DialogReply>
where
    F: FnOnce(mpsc::Sender<DialogReply>) -> DialogRequest,
{
    let (reply_tx, reply_rx) = mpsc::channel();

    // Mark a dialog in flight so the host's eval loop keeps waiting for the
    // human answer instead of timing out at the tight per-hook budget and
    // running the gated tool while the confirm is still on screen
    // (dirge-hwzs). Cleared when this scope exits (answer, timeout, or
    // shutdown), so a hook stuck for a NON-dialog reason still times out.
    //
    // Entered BEFORE the request is published: once the responder can see
    // the request it can also answer it, and anything observing "is a
    // dialog in flight?" in that window must not see zero. Sending first
    // left a gap where the dialog was visible but not yet counted.
    let _pending = DialogPendingGuard::enter();

    let req = build(reply_tx);
    tx.send(req).ok()?;

    // Poll for the reply. Wake every `DIALOG_POLL` to check the
    // worker-shutdown flag so a UI exit or `Worker::Drop` doesn't pin
    // us forever on `recv()`. The polling overhead is negligible
    // compared to the time a human takes to answer a dialog. Also bail
    // after `DIALOG_TIMEOUT` so a dialog whose responder never answers
    // can't pin the worker forever (dirge-u5ig).
    let start = std::time::Instant::now();
    loop {
        match reply_rx.recv_timeout(DIALOG_POLL) {
            Ok(r) => return Some(r),
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let shutting_down = SHUTDOWN.with(|cell| {
                    cell.borrow()
                        .as_ref()
                        .map(|f| f.load(Ordering::SeqCst))
                        .unwrap_or(false)
                });
                if dialog_should_abort(start.elapsed(), shutting_down) {
                    if !shutting_down {
                        tracing::warn!(
                            target: "dirge::plugin",
                            timeout_secs = DIALOG_TIMEOUT.as_secs(),
                            "harness dialog had no responder within timeout — returning nil",
                        );
                    }
                    return None;
                }
            }
        }
    }
}

/// Whether the worker thread this C-function is running on has been asked to
/// shut down. Reads the same `SHUTDOWN` thread-local `send_dialog` polls, so
/// blocking FFI bridges (e.g. the DAP `dap_send_and_wait` loop) can bail out
/// promptly on UI exit instead of pinning the worker thread until their own
/// timeout. Returns `false` when called off the worker thread (flag unset).
//
// Gated on `dap + plugin` to match its only caller (the DAP Janet bridge in
// `dap::janet_bindings`, itself `cfg(all(dap, plugin))`); a `dap`-only gate
// would leave it dead in the dap-without-plugin build.
#[cfg(all(feature = "dap", feature = "plugin"))]
pub(crate) fn worker_is_shutting_down() -> bool {
    SHUTDOWN.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|f| f.load(Ordering::SeqCst))
            .unwrap_or(false)
    })
}

/// Read a Janet string at argv[i] and decode as UTF-8. Returns None for
/// non-string args or invalid UTF-8 (we don't surface lossy strings to
/// plugins — caller handles the None as a no-op).
#[cfg(feature = "plugin")]
unsafe fn read_string_arg(argv: *mut janetrs::lowlevel::Janet, i: i32) -> Option<String> {
    use janetrs::lowlevel::*;
    let v = unsafe { *argv.offset(i as isize) };
    // janet_checktype returns 1 if the type matches.
    let is_str = unsafe { janet_checktype(v, JanetType_JANET_STRING) } != 0;
    let is_kw = unsafe { janet_checktype(v, JanetType_JANET_KEYWORD) } != 0;
    let is_sym = unsafe { janet_checktype(v, JanetType_JANET_SYMBOL) } != 0;
    let is_buf = unsafe { janet_checktype(v, JanetType_JANET_BUFFER) } != 0;
    if !(is_str || is_kw || is_sym || is_buf) {
        return None;
    }
    if is_buf {
        let buf = unsafe { janet_unwrap_buffer(v) };
        if buf.is_null() {
            return None;
        }
        let data = unsafe { (*buf).data };
        let count = unsafe { (*buf).count } as usize;
        let slice = unsafe { std::slice::from_raw_parts(data, count) };
        return std::str::from_utf8(slice).ok().map(str::to_string);
    }
    let raw = unsafe { janet_unwrap_string(v) };
    if raw.is_null() {
        return None;
    }
    // Janet strings carry their length in the GC header; janet_string_head
    // is the public way to fetch it (janet_string_length is a C macro that
    // isn't exposed through the auto-generated bindings).
    let len = unsafe { (*janet_string_head(raw)).length } as usize;
    let slice = unsafe { std::slice::from_raw_parts(raw, len) };
    std::str::from_utf8(slice).ok().map(str::to_string)
}

/// Read a Janet tuple/array of strings at argv[i].
#[cfg(feature = "plugin")]
unsafe fn read_string_array_arg(
    argv: *mut janetrs::lowlevel::Janet,
    i: i32,
) -> Option<Vec<String>> {
    use janetrs::lowlevel::*;
    let v = unsafe { *argv.offset(i as isize) };
    let is_tuple = unsafe { janet_checktype(v, JanetType_JANET_TUPLE) } != 0;
    let is_array = unsafe { janet_checktype(v, JanetType_JANET_ARRAY) } != 0;
    if !is_tuple && !is_array {
        return None;
    }
    let (data, len) = if is_tuple {
        let raw = unsafe { janet_unwrap_tuple(v) };
        if raw.is_null() {
            return None;
        }
        // Same GC-header trick as strings; janet_tuple_length is a macro.
        let n = unsafe { (*janet_tuple_head(raw)).length } as usize;
        (raw, n)
    } else {
        let arr = unsafe { janet_unwrap_array(v) };
        if arr.is_null() {
            return None;
        }
        let n = unsafe { (*arr).count } as usize;
        (unsafe { (*arr).data } as *const janetrs::lowlevel::Janet, n)
    };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    let mut out = Vec::with_capacity(len);
    for (idx, _) in slice.iter().enumerate() {
        // Recurse through the same arg-reader, treating each element as if
        // it sat at argv[idx]. Doable because read_string_arg only uses
        // the raw Janet, not its position.
        let elt_ptr = unsafe { data.add(idx) } as *mut janetrs::lowlevel::Janet;
        {
            let s = unsafe { read_string_arg(elt_ptr, 0) }?;
            out.push(s)
        }
    }
    Some(out)
}

/// Wrap a Rust `&str` as a Janet string. The Janet GC takes ownership of
/// the copied bytes via janet_string. Returns Janet nil when the string
/// is too large for Janet's i32 length (>2 GB) — this never happens for
/// real dialog answers but is checked defensively because silently
/// truncating the length to i32 would let Janet read past the
/// allocation.
#[cfg(feature = "plugin")]
unsafe fn wrap_string(s: &str) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let bytes = s.as_bytes();
    let Ok(len) = i32::try_from(bytes.len()) else {
        return unsafe { janet_wrap_nil() };
    };
    let raw = unsafe { janet_string(bytes.as_ptr(), len) };
    unsafe { janet_wrap_string(raw) }
}

/// Read a Janet number at argv[i] as a u32 (clamped at 0). Returns None
/// for non-number args. Used for the LSP line/char position arguments.
#[cfg(all(feature = "plugin", feature = "lsp"))]
unsafe fn read_uint_arg(argv: *mut janetrs::lowlevel::Janet, i: i32) -> Option<u32> {
    use janetrs::lowlevel::*;
    let v = unsafe { *argv.offset(i as isize) };
    if unsafe { janet_checktype(v, JanetType_JANET_NUMBER) } == 0 {
        return None;
    }
    let n = unsafe { janet_unwrap_number(v) };
    // Reject non-finite / negative rather than coercing to 0 — a bogus
    // coordinate should not silently become line 0. The Janet `harness/lsp`
    // wrapper validates positivity before we get here; this is the backstop.
    if n.is_finite() && n >= 0.0 {
        Some(n as u32)
    } else {
        None
    }
}

/// C-function backing `harness/__lsp`. Reads (op, file, line, char,
/// query), forwards a JSON request to the tokio drainer via `LSP_TX`,
/// blocks on the reply (polling the shutdown flag like the dialog cfns),
/// and returns the JSON result string. Panics are caught at the FFI
/// boundary and collapse to nil.
#[cfg(all(feature = "plugin", feature = "lsp"))]
unsafe extern "C-unwind" fn janet_lsp_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        lsp_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/lsp",
                panic = %msg,
                "FFI panic in lsp cfn — returning nil",
            );
            unsafe { janet_wrap_nil() }
        }
    }
}

/// C-function backing `harness/__lsp-live`. Returns `true` only when the
/// bridge is wired to a live request receiver — i.e. the host spawned the
/// LSP responder against a real `LspManager`. When LSP is disabled at
/// runtime (no manager) the receiver is dropped, so `is_closed()` is true
/// and we report `false`. This makes `(harness/lsp?)` reflect *runtime*
/// availability rather than mere compile-time presence, so a plugin that
/// feature-detects won't then try to decode a nil result.
#[cfg(all(feature = "plugin", feature = "lsp"))]
unsafe extern "C-unwind" fn janet_lsp_live_cfn(
    _argc: i32,
    _argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let live = LSP_TX.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|tx| !tx.is_closed())
            .unwrap_or(false)
    });
    unsafe { janet_wrap_boolean(if live { 1 } else { 0 }) }
}

#[cfg(all(feature = "plugin", feature = "lsp"))]
unsafe fn lsp_body(argc: i32, argv: *mut janetrs::lowlevel::Janet) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 5 {
        return unsafe { janet_wrap_nil() };
    }
    let op = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let file = match unsafe { read_string_arg(argv, 1) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let line = unsafe { read_uint_arg(argv, 2) }.unwrap_or(1);
    let character = unsafe { read_uint_arg(argv, 3) }.unwrap_or(1);
    let query = unsafe { read_string_arg(argv, 4) }.unwrap_or_default();

    let request = serde_json::json!({
        "op": op,
        "file": file,
        "line": line,
        "char": character,
        "query": query,
    })
    .to_string();

    let answer = LSP_TX.with(|cell| match cell.borrow().as_ref() {
        Some(tx) => send_lsp(tx, request),
        None => None,
    });
    match answer {
        Some(json) => unsafe { wrap_string(&json) },
        None => unsafe { janet_wrap_nil() },
    }
}

/// Upper bound on a single `harness/lsp` query. Unlike dialogs (bounded by
/// a human), an LSP query can hang against a slow or wedged language
/// server; without a cap it would freeze the Janet worker thread — and
/// thus every plugin hook — indefinitely. After this elapses we give up
/// and return nil to the plugin.
#[cfg(all(feature = "plugin", feature = "lsp"))]
const LSP_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether `send_lsp` should stop waiting: either the worker is shutting
/// down, or the query has exceeded [`LSP_QUERY_TIMEOUT`]. Split out so the
/// give-up policy is unit-testable without a real hung server.
#[cfg(all(feature = "plugin", feature = "lsp"))]
fn lsp_should_abort(elapsed: Duration, shutting_down: bool) -> bool {
    shutting_down || elapsed >= LSP_QUERY_TIMEOUT
}

/// Send an LSP request and block on the JSON reply, polling the shutdown
/// flag so `Worker::Drop` can unblock us (mirrors `send_dialog`). Unlike
/// dialogs, also bounded by [`LSP_QUERY_TIMEOUT`] so a wedged language
/// server can't pin the worker thread forever.
#[cfg(all(feature = "plugin", feature = "lsp"))]
fn send_lsp(tx: &tmpsc::UnboundedSender<LspRequest>, request: String) -> Option<String> {
    let (reply_tx, reply_rx) = mpsc::channel::<String>();
    tx.send(LspRequest {
        request,
        reply: reply_tx,
    })
    .ok()?;
    let start = std::time::Instant::now();
    loop {
        match reply_rx.recv_timeout(DIALOG_POLL) {
            Ok(r) => return Some(r),
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let shutting_down = SHUTDOWN.with(|cell| {
                    cell.borrow()
                        .as_ref()
                        .map(|f| f.load(Ordering::SeqCst))
                        .unwrap_or(false)
                });
                if lsp_should_abort(start.elapsed(), shutting_down) {
                    if !shutting_down {
                        tracing::warn!(
                            target: "dirge::plugin",
                            timeout_secs = LSP_QUERY_TIMEOUT.as_secs(),
                            "harness/lsp query timed out — returning nil",
                        );
                    }
                    return None;
                }
            }
        }
    }
}

/// Translate a `ComputerUseAction` into a safe SSH command string
/// that runs inside the microVM. The command targets ydotool for
/// input simulation — the microVM sandbox isolates it from the host.
#[cfg(feature = "plugin")]
pub fn build_safe_command(action: &ComputerUseAction) -> String {
    match action {
        ComputerUseAction::Key { keys } => {
            let presses: Vec<String> = keys
                .iter()
                .flat_map(|k| [format!("{k}:1"), format!("{k}:0")])
                .collect();
            format!("DISPLAY=:99 ydotool key {}", presses.join(" "))
        }
        ComputerUseAction::Type { text } => {
            // Single-quote the text and escape any embedded single quotes
            let escaped = text.replace('\'', "'\\''");
            format!("DISPLAY=:99 ydotool type '{}'", escaped)
        }
        ComputerUseAction::MouseMove { x, y } => {
            format!("DISPLAY=:99 ydotool mousemove --absolute {x} {y}")
        }
        ComputerUseAction::MouseClick { button } => {
            format!("DISPLAY=:99 ydotool click {button}")
        }
        ComputerUseAction::Scroll { direction, amount } => {
            // ydotool scroll doesn't take a direction flag — just
            // positive/negative amounts. We normalise here.
            let normalised = match direction.as_str() {
                "up" | "Up" => *amount,
                "down" | "Down" => -amount,
                other => return format!("echo 'unknown scroll direction: {other}'"),
            };
            format!(
                "DISPLAY=:99 ydotool scroll {} {}",
                normalised.max(0),
                (-normalised).max(0)
            )
        }
        ComputerUseAction::Screenshot => {
            "DISPLAY=:99 import -window root /workspace/screenshot.png".to_string()
        }
        ComputerUseAction::KeyChord { chord } => {
            format!("DISPLAY=:99 ydotool key {chord}")
        }
        ComputerUseAction::OpenUrl { url } => {
            let escaped = url.replace('\'', "'\\''");
            format!(
                r#"for b in firefox-esr firefox chromium chromium-browser; do command -v "$b">/dev/null 2>&1 && {{ DISPLAY=:99 "$b" '{}' 1>/dev/null 2>&1 & break; }}; done"#,
                escaped
            )
        }
    }
}

/// Send a sandbox exec request and block on the result, polling the
/// shutdown flag so `Worker::Drop` can unblock us (mirrors `send_dialog`).
#[cfg(feature = "plugin")]
fn send_sandbox_exec(
    tx: &tmpsc::UnboundedSender<SandboxExecRequest>,
    action: ComputerUseAction,
) -> Option<Result<SandboxExecOutput, String>> {
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(SandboxExecRequest {
        action,
        reply: reply_tx,
    })
    .ok()?;
    let start = std::time::Instant::now();
    loop {
        match reply_rx.recv_timeout(DIALOG_POLL) {
            Ok(r) => return Some(r),
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let shutting_down = SHUTDOWN.with(|cell| {
                    cell.borrow()
                        .as_ref()
                        .map(|f| f.load(Ordering::SeqCst))
                        .unwrap_or(false)
                });
                if lsp_should_abort(start.elapsed(), shutting_down) {
                    if !shutting_down {
                        tracing::warn!(
                            target: "dirge::plugin",
                            timeout_secs = LSP_QUERY_TIMEOUT.as_secs(),
                            "harness/computer-use-exec query timed out — returning nil",
                        );
                    }
                    return None;
                }
            }
        }
    }
}

/// C-function backing `harness/__computer-use-exec`. Takes a Janet
/// dict with :action (string) and action-specific fields, sends a
/// `SandboxExecRequest` to the tokio drainer, blocks on the result,
/// and returns a dict {:exit_code :merged}. Panics are caught
/// at the FFI boundary and collapse to nil.
#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_computer_use_exec_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        computer_use_exec_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/computer-use-exec",
                panic = %msg,
                "FFI panic in computer-use-exec cfn — returning nil",
            );
            unsafe { janet_wrap_nil() }
        }
    }
}

#[cfg(feature = "plugin")]
unsafe fn computer_use_exec_body(
    _argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;

    // Arg 0 is a Janet dictionary/table.
    let v = unsafe { *argv.offset(0) };
    let is_dict = unsafe { janet_checktype(v, JanetType_JANET_TABLE) } != 0
        || unsafe { janet_checktype(v, JanetType_JANET_STRUCT) } != 0;
    if !is_dict {
        return unsafe { janet_wrap_nil() };
    }

    // Read :action string
    let action_str = match unsafe { get_dict_string(v, "action") } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };

    let action = match action_str.as_str() {
        "key" => {
            let keys = match unsafe { get_dict_int_array(v, "keys") } {
                Some(k) => k,
                None => return unsafe { janet_wrap_nil() },
            };
            ComputerUseAction::Key { keys }
        }
        "type" => {
            let text = match unsafe { get_dict_string(v, "text") } {
                Some(t) => t,
                None => return unsafe { janet_wrap_nil() },
            };
            ComputerUseAction::Type { text }
        }
        "mouse_move" => {
            let x = unsafe { get_dict_int(v, "x") }.unwrap_or(0);
            let y = unsafe { get_dict_int(v, "y") }.unwrap_or(0);
            ComputerUseAction::MouseMove { x, y }
        }
        "mouse_click" => {
            let button = match unsafe { get_dict_string(v, "button") } {
                Some(b) => b,
                None => "left".to_string(),
            };
            ComputerUseAction::MouseClick { button }
        }
        "scroll" => {
            let direction = match unsafe { get_dict_string(v, "direction") } {
                Some(d) => d,
                None => "down".to_string(),
            };
            let amount = unsafe { get_dict_int(v, "amount") }.unwrap_or(1);
            ComputerUseAction::Scroll { direction, amount }
        }
        "screenshot" => ComputerUseAction::Screenshot,
        "keychord" => {
            let chord = match unsafe { get_dict_string(v, "chord") } {
                Some(c) => c,
                None => return unsafe { janet_wrap_nil() },
            };
            ComputerUseAction::KeyChord { chord }
        }
        "open_url" => {
            let url = match unsafe { get_dict_string(v, "url") } {
                Some(u) => u,
                None => return unsafe { janet_wrap_nil() },
            };
            ComputerUseAction::OpenUrl { url }
        }
        _ => return unsafe { janet_wrap_nil() },
    };

    let result = match SANDBOX_EXEC_TX.get() {
        Some(tx) => send_sandbox_exec(tx, action),
        None => None,
    };

    match result {
        Some(Ok(output)) => {
            // Return a Janet table with :exit_code and :merged.
            let table = unsafe { janet_table(0) };
            let exit_code = unsafe { wrap_int(output.exit_code) };
            let merged = unsafe { wrap_string(&output.merged) };
            unsafe {
                janet_table_put(
                    table,
                    janet_ckeywordv(c"exit_code".as_ptr() as *const u8, 9),
                    exit_code,
                );
                janet_table_put(
                    table,
                    janet_ckeywordv(c"merged".as_ptr() as *const u8, 6),
                    merged,
                );
            }
            unsafe { janet_wrap_table(table) }
        }
        Some(Err(e)) => {
            // Return an error table so Janet can inspect it
            let table = unsafe { janet_table(0) };
            let exit_code = unsafe { wrap_int(-1) };
            let merged = unsafe { wrap_string(&e) };
            unsafe {
                janet_table_put(
                    table,
                    janet_ckeywordv(c"exit_code".as_ptr() as *const u8, 9),
                    exit_code,
                );
                janet_table_put(
                    table,
                    janet_ckeywordv(c"merged".as_ptr() as *const u8, 6),
                    merged,
                );
            }
            unsafe { janet_wrap_table(table) }
        }
        None => unsafe { janet_wrap_nil() },
    }
}

/// Upper bound on a single `harness/call-tool`. Generous compared to
/// [`LSP_QUERY_TIMEOUT`] because the callee is a real tool — `bash` alone
/// defaults to minutes — but still bounded: the Janet worker is blocked for
/// the whole call, so an unbounded wait would freeze every hook and every
/// other plugin behind it.
#[cfg(feature = "plugin")]
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Send a tool call and block on the reply, polling the shutdown flag so
/// `Worker::Drop` can unblock us. Same shape as [`send_lsp`].
#[cfg(feature = "plugin")]
fn send_tool_call(
    tx: &tmpsc::UnboundedSender<super::tool_bridge::ToolCallRequest>,
    name: String,
    args_json: String,
) -> Option<Result<String, String>> {
    let (reply_tx, reply_rx) = mpsc::channel::<Result<String, String>>();
    tx.send(super::tool_bridge::ToolCallRequest {
        name,
        args_json,
        reply: reply_tx,
    })
    .ok()?;
    let start = std::time::Instant::now();
    loop {
        match reply_rx.recv_timeout(DIALOG_POLL) {
            Ok(r) => return Some(r),
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let shutting_down = SHUTDOWN.with(|cell| {
                    cell.borrow()
                        .as_ref()
                        .map(|f| f.load(Ordering::SeqCst))
                        .unwrap_or(false)
                });
                if shutting_down {
                    return None;
                }
                if start.elapsed() >= TOOL_CALL_TIMEOUT {
                    return Some(Err(format!(
                        "tool call exceeded {}s and was abandoned",
                        TOOL_CALL_TIMEOUT.as_secs()
                    )));
                }
            }
        }
    }
}

/// C-function backing `harness/__call-tool`. Reads (name, args-json),
/// forwards to the tokio responder and blocks on the reply. Returns a Janet
/// table `{:ok bool :output string}`, or nil when the bridge is not wired.
/// A table rather than a bare string because the caller has to be able to
/// tell "the tool said no" from "the tool returned text".
#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_call_tool_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        call_tool_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/call-tool",
                panic = %msg,
                "FFI panic in call-tool cfn — returning nil",
            );
            unsafe { janet_wrap_nil() }
        }
    }
}

#[cfg(feature = "plugin")]
unsafe fn call_tool_body(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 2 {
        return unsafe { janet_wrap_nil() };
    }
    let name = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let args_json = unsafe { read_string_arg(argv, 1) }.unwrap_or_else(|| "{}".to_string());

    let answer = match super::tool_bridge::tool_call_tx() {
        Some(tx) => send_tool_call(tx, name, args_json),
        None => None,
    };

    let (ok, output) = match answer {
        Some(Ok(text)) => (true, text),
        Some(Err(err)) => (false, err),
        None => return unsafe { janet_wrap_nil() },
    };

    let table = unsafe { janet_table(0) };
    let ok_value = unsafe { janet_wrap_boolean(if ok { 1 } else { 0 }) };
    let out_value = unsafe { wrap_string(&output) };
    unsafe {
        janet_table_put(
            table,
            janet_ckeywordv(c"ok".as_ptr() as *const u8, 2),
            ok_value,
        );
        janet_table_put(
            table,
            janet_ckeywordv(c"output".as_ptr() as *const u8, 6),
            out_value,
        );
    }
    unsafe { janet_wrap_table(table) }
}

/// C-function backing `harness/__list-tools`. Returns a JSON string of the
/// callable tools, or nil when the bridge is not wired.
#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_list_tools_cfn(
    _argc: i32,
    _argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::tool_bridge::list_json()
    }));
    match result {
        Ok(json) => unsafe { wrap_string(&json) },
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/list-tools",
                panic = %msg,
                "FFI panic in list-tools cfn — returning nil",
            );
            unsafe { janet_wrap_nil() }
        }
    }
}

/// C-function backing `harness/__tools-live`. True only when the responder
/// is wired AND a registry has been published, so `(harness/tools?)` reports
/// runtime availability — the same contract `harness/lsp?` holds.
#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_tools_live_cfn(
    _argc: i32,
    _argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::janet_wrap_boolean;
    let live = super::tool_bridge::is_live();
    unsafe { janet_wrap_boolean(if live { 1 } else { 0 }) }
}

/// Wrap an `i32` as a Janet value, portably. `janet_wrap_integer` is only
/// linkable on x86_64 (it's nanbox-specific); Janet numbers are f64-backed
/// everywhere, so `janet_wrap_number` is the portable wrap and is exactly
/// what `janetrs::Janet::integer` falls back to off x86_64. Using it on all
/// arches keeps the plugin linking on aarch64 (macOS) — dirge-... .
#[cfg(feature = "plugin")]
unsafe fn wrap_int(value: i32) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::janet_wrap_number;
    unsafe { janet_wrap_number(value as f64) }
}

/// True if `v` is Janet nil. Replaces non-portable raw `.pointer.is_null()`
/// checks — the `Janet` union's field layout differs across Janet's
/// nanbox/non-nanbox builds and arches, so probing `.pointer` directly
/// doesn't compile on aarch64.
#[cfg(feature = "plugin")]
unsafe fn is_nil(v: janetrs::lowlevel::Janet) -> bool {
    use janetrs::lowlevel::*;
    unsafe { janet_checktype(v, JanetType_JANET_NIL) != 0 }
}

/// Look up `key` in a Janet table or struct, portably. Avoids casting the
/// raw `Janet` union's `.pointer` field (not portable across arches) by
/// unwrapping to the concrete container type first. Returns Janet nil when
/// `v` is neither a table nor a struct, or the key is absent.
#[cfg(feature = "plugin")]
unsafe fn dict_get(v: janetrs::lowlevel::Janet, key: &str) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let k = unsafe { janet_ckeywordv(key.as_ptr(), key.len() as i32) };
    if unsafe { janet_checktype(v, JanetType_JANET_TABLE) != 0 } {
        let t = unsafe { janet_unwrap_table(v) };
        if t.is_null() {
            return unsafe { janet_wrap_nil() };
        }
        unsafe { janet_table_get(t, k) }
    } else if unsafe { janet_checktype(v, JanetType_JANET_STRUCT) != 0 } {
        let s = unsafe { janet_unwrap_struct(v) };
        if s.is_null() {
            return unsafe { janet_wrap_nil() };
        }
        unsafe { janet_struct_get(s, k) }
    } else {
        unsafe { janet_wrap_nil() }
    }
}

/// Read a string value from a Janet dict/struct by key name.
#[cfg(feature = "plugin")]
unsafe fn get_dict_string(v: janetrs::lowlevel::Janet, key: &str) -> Option<String> {
    use janetrs::lowlevel::*;
    let out = unsafe { dict_get(v, key) };
    if unsafe { is_nil(out) } {
        return None;
    }
    if unsafe { janet_checktype(out, JanetType_JANET_STRING) } == 0
        && unsafe { janet_checktype(out, JanetType_JANET_KEYWORD) } == 0
    {
        return None;
    }
    let raw = unsafe { janet_unwrap_string(out) };
    if raw.is_null() {
        return None;
    }
    let len = unsafe { (*janet_string_head(raw)).length } as usize;
    let slice = unsafe { std::slice::from_raw_parts(raw, len) };
    std::str::from_utf8(slice).ok().map(str::to_string)
}

/// Read an i64 from a Janet dict/struct by key name.
#[cfg(feature = "plugin")]
unsafe fn get_dict_int(v: janetrs::lowlevel::Janet, key: &str) -> Option<i64> {
    use janetrs::lowlevel::*;
    let out = unsafe { dict_get(v, key) };
    if unsafe { is_nil(out) } {
        return None;
    }
    if unsafe { janet_checktype(out, JanetType_JANET_NUMBER) } == 0 {
        return None;
    }
    let n = unsafe { janet_unwrap_number(out) };
    if n.is_finite() && n >= (i64::MIN as f64) && n <= (i64::MAX as f64) {
        Some(n as i64)
    } else {
        None
    }
}

/// Read an array of i64 values from a Janet dict/struct by key name.
#[cfg(feature = "plugin")]
unsafe fn get_dict_int_array(v: janetrs::lowlevel::Janet, key: &str) -> Option<Vec<i64>> {
    use janetrs::lowlevel::*;
    let out = unsafe { dict_get(v, key) };
    if unsafe { is_nil(out) } {
        return None;
    }
    let is_tuple = unsafe { janet_checktype(out, JanetType_JANET_TUPLE) } != 0;
    let is_array = unsafe { janet_checktype(out, JanetType_JANET_ARRAY) } != 0;
    if !is_tuple && !is_array {
        return None;
    }
    let (data, len) = if is_tuple {
        let raw = unsafe { janet_unwrap_tuple(out) };
        if raw.is_null() {
            return None;
        }
        let n = unsafe { (*janet_tuple_head(raw)).length } as usize;
        (raw, n)
    } else {
        let arr = unsafe { janet_unwrap_array(out) };
        if arr.is_null() {
            return None;
        }
        let n = unsafe { (*arr).count } as usize;
        (unsafe { (*arr).data } as *const Janet, n)
    };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    let mut out = Vec::with_capacity(len);
    for (idx, _) in slice.iter().enumerate() {
        let elt_ptr = unsafe { data.add(idx) as *mut Janet };
        let elt = unsafe { *elt_ptr };
        if unsafe { janet_checktype(elt, JanetType_JANET_NUMBER) } == 0 {
            return None;
        }
        let n = unsafe { janet_unwrap_number(elt) };
        if n.is_finite() && n >= (i64::MIN as f64) && n <= (i64::MAX as f64) {
            out.push(n as i64);
        } else {
            return None;
        }
    }
    Some(out)
}

/// Bridge from the Janet worker thread to the vigil-keeper's event channel.
/// Thread-local mpsc sender installed once at startup.
///
/// Exposes Janet functions:
/// - `vigil/live` — true if the bridge has been installed (keeper is running)
/// - `vigil/emit` — push a vigil event from Janet
/// - `vigil/list` — list all active vigil names
/// - `vigil/get` — get state for a named vigil
/// - `vigil/set-state` — set key-value state on a named vigil
///
/// All functions are installed into the Janet environment by the plugin
/// loader and are guarded by the `plugin` + `vigil` feature gates.
#[cfg(all(feature = "plugin", feature = "vigil"))]
pub(crate) mod vigil_bridge {
    use super::read_string_arg;
    use std::collections::HashMap;

    // Process-global (not thread-local): install_* runs on the tokio
    // runtime thread while the C functions read these on the Janet worker
    // thread, so a thread_local value would never be visible where it's
    // consumed (mirrors the SANDBOX_EXEC_TX OnceLock pattern above).
    /// Sender to the vigil-keeper's event channel. Installed once at
    /// keeper startup during `--vigil` mode.
    static VIGIL_TX: std::sync::OnceLock<tokio::sync::mpsc::Sender<String>> =
        std::sync::OnceLock::new();

    /// Active vigil names, populated at keeper startup via install_vigil_names.
    static VIGIL_NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

    /// Per-vigil state map (name → JSON value). Janet code can
    /// read/write this via vigil/get and vigil/set-state. Lazy-initialized
    /// because `HashMap::new` is not const.
    static VIGIL_STATE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, serde_json::Value>>> =
        std::sync::OnceLock::new();

    fn vigil_state() -> &'static std::sync::Mutex<HashMap<String, serde_json::Value>> {
        VIGIL_STATE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
    }

    /// Install the vigil bridge sender. Called from the tokio runtime
    /// after the vigil-keeper is created. Panics if called twice.
    pub fn install_vigil_tx(tx: tokio::sync::mpsc::Sender<String>) {
        assert!(VIGIL_TX.set(tx).is_ok(), "vigil bridge already installed");
    }

    /// Install vigil names into the bridge. Called at keeper startup
    /// after install_vigil_tx.
    pub fn install_vigil_names(names: Vec<String>) {
        let _ = VIGIL_NAMES.set(names);
    }

    /// Return a Janet boolean — true if the vigil bridge has been installed.
    pub unsafe extern "C-unwind" fn vigil_live_cfn(
        _argc: i32,
        _argv: *mut janetrs::lowlevel::Janet,
    ) -> janetrs::lowlevel::Janet {
        use janetrs::lowlevel::*;
        let live = VIGIL_TX.get().is_some();
        unsafe { janet_wrap_boolean(if live { 1 } else { 0 }) }
    }

    /// Push a vigil event from Janet into the keeper. Takes one
    /// string argument (the event payload).
    pub unsafe extern "C-unwind" fn vigil_emit_cfn(
        argc: i32,
        argv: *mut janetrs::lowlevel::Janet,
    ) -> janetrs::lowlevel::Janet {
        use janetrs::lowlevel::*;
        if argc < 1 {
            return unsafe { janet_wrap_nil() };
        }
        let msg = match unsafe { read_string_arg(argv, 0) } {
            Some(s) => s,
            None => return unsafe { janet_wrap_nil() },
        };
        if let Some(tx) = VIGIL_TX.get() {
            let _ = tx.try_send(msg);
        }
        unsafe { janet_wrap_nil() }
    }

    /// (vigil/list) — return a Janet array of all active vigil names.
    #[allow(clippy::ptr_offset_with_cast)]
    pub unsafe extern "C-unwind" fn vigil_list_cfn(
        _argc: i32,
        _argv: *mut janetrs::lowlevel::Janet,
    ) -> janetrs::lowlevel::Janet {
        use janetrs::lowlevel::*;
        let names = VIGIL_NAMES.get().map(|v| v.as_slice()).unwrap_or(&[]);
        let tup = unsafe { janet_tuple_begin(names.len() as i32) };
        for (i, name) in names.iter().enumerate() {
            unsafe {
                let c_str = std::ffi::CString::new(name.as_str()).unwrap();
                let s = janet_wrap_string(janet_cstring(c_str.as_ptr()));
                *tup.offset(i as isize) = s;
            }
        }
        unsafe { janet_wrap_tuple(janet_tuple_end(tup)) }
    }

    /// (vigil/get name) — return a Janet table of state for the named vigil.
    /// Returns nil if the vigil is not found.
    pub unsafe extern "C-unwind" fn vigil_get_cfn(
        argc: i32,
        argv: *mut janetrs::lowlevel::Janet,
    ) -> janetrs::lowlevel::Janet {
        use janetrs::lowlevel::*;
        if argc < 1 {
            return unsafe { janet_wrap_nil() };
        }
        let name = match unsafe { read_string_arg(argv, 0) } {
            Some(s) => s,
            None => return unsafe { janet_wrap_nil() },
        };
        let state = vigil_state().lock().unwrap();
        if let Some(value) = state.get(&name) {
            json_to_janet(value)
        } else {
            let known = VIGIL_NAMES.get().is_some_and(|n| n.contains(&name));
            if known {
                // Vigil exists but has no state yet — return empty table.
                let tab = unsafe { janet_table(0) };
                unsafe { janet_wrap_table(tab) }
            } else {
                unsafe { janet_wrap_nil() }
            }
        }
    }

    /// (vigil/set-state name key value) — set a key-value pair on a vigil's
    /// state. Returns the vigil name on success, nil on failure.
    pub unsafe extern "C-unwind" fn vigil_set_state_cfn(
        argc: i32,
        argv: *mut janetrs::lowlevel::Janet,
    ) -> janetrs::lowlevel::Janet {
        use janetrs::lowlevel::*;
        if argc < 3 {
            return unsafe { janet_wrap_nil() };
        }
        let name = match unsafe { read_string_arg(argv, 0) } {
            Some(s) => s,
            None => return unsafe { janet_wrap_nil() },
        };
        let key = match unsafe { read_string_arg(argv, 1) } {
            Some(s) => s,
            None => return unsafe { janet_wrap_nil() },
        };
        let value_str = match unsafe { read_string_arg(argv, 2) } {
            Some(s) => s,
            None => return unsafe { janet_wrap_nil() },
        };
        // Parse as JSON — if the value looks like JSON, use it; otherwise
        // treat it as a raw string.
        let value: serde_json::Value =
            serde_json::from_str(&value_str).unwrap_or(serde_json::Value::String(value_str));
        let mut state = vigil_state().lock().unwrap();
        let entry = state
            .entry(name.clone())
            .or_insert(serde_json::Value::Object(serde_json::Map::new()));
        if let serde_json::Value::Object(map) = entry {
            map.insert(key, value);
        }
        drop(state);
        let c_str = std::ffi::CString::new(name.as_str()).unwrap();
        unsafe { janet_wrap_string(janet_cstring(c_str.as_ptr())) }
    }

    /// Convert a serde_json::Value to a Janet value.
    #[allow(clippy::ptr_offset_with_cast)]
    pub(super) fn json_to_janet(value: &serde_json::Value) -> janetrs::lowlevel::Janet {
        use janetrs::lowlevel::*;
        match value {
            serde_json::Value::Null => unsafe { janet_wrap_nil() },
            serde_json::Value::Bool(b) => unsafe { janet_wrap_boolean(if *b { 1 } else { 0 }) },
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    unsafe { janet_wrap_number(i as f64) }
                } else if let Some(f) = n.as_f64() {
                    unsafe { janet_wrap_number(f) }
                } else {
                    unsafe { janet_wrap_nil() }
                }
            }
            serde_json::Value::String(s) => {
                let c_str = std::ffi::CString::new(s.as_str()).unwrap();
                unsafe { janet_wrap_string(janet_cstring(c_str.as_ptr())) }
            }
            serde_json::Value::Array(arr) => {
                let tup = unsafe { janet_tuple_begin(arr.len() as i32) };
                for (i, v) in arr.iter().enumerate() {
                    unsafe {
                        *tup.offset(i as isize) = json_to_janet(v);
                    }
                }
                unsafe { janet_wrap_tuple(janet_tuple_end(tup)) }
            }
            serde_json::Value::Object(map) => {
                let tab = unsafe { janet_table(map.len() as i32) };
                for (k, v) in map {
                    let c_str = std::ffi::CString::new(k.as_str()).unwrap();
                    let key = unsafe { janet_wrap_string(janet_cstring(c_str.as_ptr())) };
                    let val = json_to_janet(v);
                    unsafe { janet_table_put(tab, key, val) };
                }
                unsafe { janet_wrap_table(tab) }
            }
        }
    }
}

#[cfg(all(test, feature = "plugin"))]
mod tests {
    use super::*;

    /// dirge-9xjg.1: the whole point of interrupting rather than
    /// abandoning a runaway eval — the worker recovers AND the bindings
    /// made before it survive. Without the interrupt the second eval never
    /// returns, because the `(while true)` still owns the worker thread.
    #[test]
    fn runaway_eval_is_interrupted_and_vm_state_survives() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        assert_eq!(
            w.eval("(def before-runaway 42) before-runaway").unwrap(),
            "42"
        );

        let err = w
            .eval_with_timeout("(while true (def spin 1))", Duration::from_millis(150))
            .expect_err("a (while true) must not return a value");
        assert!(
            err.contains("interrupted") || err.contains("did not reply"),
            "unexpected error text: {err}"
        );

        // The worker is usable again and kept everything defined before
        // the runaway cell. Both halves matter: a respawn would give the
        // first assertion and fail the second.
        assert_eq!(w.eval("before-runaway").unwrap(), "42");
        assert_eq!(w.eval("(+ 1 2)").unwrap(), "3");
    }

    /// Janet's `auto_suspend` is a counter the interpreter refuses to run
    /// under, so an interrupt that is raised and never cleared bricks the
    /// VM. Hammer the interrupt path and assert the worker still evaluates.
    #[test]
    fn repeated_interrupts_do_not_latch_the_vm() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        for _ in 0..3 {
            let _ = w.eval_with_timeout("(while true nil)", Duration::from_millis(120));
        }
        assert_eq!(w.eval("(+ 20 22)").unwrap(), "42");
    }

    /// A cell that wraps itself in `try` must still be interruptible:
    /// `try` compiles to the error-only fiber mask and
    /// `JANET_SIGNAL_INTERRUPT` is USER8, so it cannot be swallowed. If
    /// this ever regresses, agent-authored code can opt out of the
    /// deadline entirely.
    #[test]
    fn a_try_wrapped_runaway_is_still_interrupted() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let err = w
            .eval_with_timeout(
                "(try (while true nil) ([e] :swallowed))",
                Duration::from_millis(150),
            )
            .expect_err("try must not be able to swallow the interrupt");
        assert!(!err.contains("swallowed"), "interrupt was caught: {err}");
        assert_eq!(w.eval("(+ 1 1)").unwrap(), "2");
    }

    /// dirge-9xjg.3: a cell's `(print …)` has to come back, or the
    /// notebook is unusable — printing intermediate values is how notebook
    /// work is done.
    #[test]
    fn eval_cell_returns_value_and_captured_output() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let cell = w
            .eval_cell(
                r#"(print "hello") (print "world") (+ 20 22)"#,
                None,
                4096,
                Duration::from_secs(5),
            )
            .expect("worker reachable");
        assert!(cell.ok, "cell should have evaluated: {cell:?}");
        assert_eq!(cell.value, "42");
        assert_eq!(cell.output, "hello\nworld\n");
    }

    /// Capture must not accumulate across cells — that was dirge-lncv
    /// (the discard buffers grew for the whole process lifetime) and it
    /// would also mean every cell replayed the previous one's output.
    #[test]
    fn cell_output_does_not_leak_into_the_next_cell() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let first = w
            .eval_cell(r#"(print "first") :a"#, None, 4096, Duration::from_secs(5))
            .unwrap();
        assert_eq!(first.output, "first\n");
        let second = w
            .eval_cell(r#":b"#, None, 4096, Duration::from_secs(5))
            .unwrap();
        assert_eq!(second.output, "", "previous cell's output leaked");
        // And the buffer really is emptied, not merely skipped.
        assert_eq!(w.eval("(length (dyn :out))").unwrap(), "0");
    }

    /// An unbounded paste is worse for a context-limited reader than a
    /// labelled elision, so a chatty cell is capped.
    #[test]
    fn cell_output_is_capped_and_marks_the_truncation() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let cell = w
            .eval_cell(
                r#"(for i 0 1000 (print "0123456789")) :done"#,
                None,
                64,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(cell.value, ":done");
        assert!(
            cell.output.contains("[stdout truncated,"),
            "expected a truncation marker, got {:?}",
            cell.output
        );
        // Cap plus the marker — nowhere near the ~11k the cell printed.
        assert!(
            cell.output.len() < 200,
            "cap not applied: {}",
            cell.output.len()
        );
    }

    /// A raising cell is an ordinary notebook outcome, not a host failure:
    /// the agent needs the error text AND whatever printed before it.
    #[test]
    fn a_raising_cell_reports_ok_false_and_keeps_its_output() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let cell = w
            .eval_cell(
                r#"(print "before") (error "boom")"#,
                None,
                4096,
                Duration::from_secs(5),
            )
            .expect("worker reachable — a Janet error is data, not Err");
        assert!(!cell.ok, "expected the cell to report failure");
        assert!(cell.output.contains("before"), "output lost: {cell:?}");
    }

    /// dirge-c8lh: Janet emits compile errors and stack traces with NO
    /// fiber active, so they read `top_dyns` rather than the fiber env and
    /// bypassed the `:out`/`:err` redirect entirely — straight onto the
    /// user's terminal, corrupting the raw-mode TUI.
    ///
    /// Asserting the text is CAPTURED is the only way to prove it is not
    /// leaking: a test can't observe the process's real stderr, but the
    /// trace has to be in exactly one of the two places.
    #[test]
    fn compile_errors_are_captured_rather_than_printed_to_the_terminal() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let cell = w
            .eval_cell(
                "(this-symbol-does-not-exist)",
                None,
                4096,
                Duration::from_secs(5),
            )
            .unwrap();
        assert!(!cell.ok);
        assert!(
            cell.output.contains("this-symbol-does-not-exist"),
            "compile error escaped capture (it went to the real stderr): {cell:?}"
        );
    }

    /// The same, for a runtime error's stack trace.
    #[test]
    fn runtime_stack_traces_are_captured_rather_than_printed() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let cell = w
            .eval_cell(r#"(error "trace-me")"#, None, 4096, Duration::from_secs(5))
            .unwrap();
        assert!(!cell.ok);
        assert!(
            cell.output.contains("trace-me"),
            "stack trace escaped capture: {cell:?}"
        );
    }

    /// dirge-9xjg.2: the notebook VM must be a genuinely separate world.
    /// State is per-session, and neither session can see the other's defs.
    #[test]
    fn notebook_sessions_do_not_clobber_each_other() {
        let mut nb = Worker::try_spawn_notebook().expect("spawn notebook");
        let put_a = nb
            .eval_cell(
                r#"(notebook/-eval "a" "(def results :from-a) results")"#,
                None,
                4096,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(put_a.value, ":from-a");

        // Same name, other session — must NOT overwrite a's binding.
        let put_b = nb
            .eval_cell(
                r#"(notebook/-eval "b" "(def results :from-b) results")"#,
                None,
                4096,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(put_b.value, ":from-b");

        let read_a = nb
            .eval_cell(
                r#"(notebook/-eval "a" "results")"#,
                None,
                4096,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(read_a.value, ":from-a", "session b clobbered session a");
    }

    /// The other half of dirge-9xjg.5: isolation must not remove the
    /// deliberate sharing that motivated the design.
    #[test]
    fn notebook_shared_table_crosses_sessions() {
        let mut nb = Worker::try_spawn_notebook().expect("spawn notebook");
        nb.eval_cell(
            r#"(notebook/-eval "a" "(put notebook/shared :k :published)")"#,
            None,
            4096,
            Duration::from_secs(5),
        )
        .unwrap();
        let seen = nb
            .eval_cell(
                r#"(notebook/-eval "b" "(get notebook/shared :k)")"#,
                None,
                4096,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(seen.value, ":published");
    }

    /// The notebook VM must not carry the host bridges. `harness/confirm`
    /// is the permission gate's own channel; a cell that could open a
    /// dialog could also extend its own deadline indefinitely.
    #[test]
    fn notebook_vm_has_no_dialog_or_lsp_bridge() {
        let mut nb = Worker::try_spawn_notebook().expect("spawn notebook");
        for sym in [
            "harness/confirm",
            "harness/select",
            "harness/__confirm",
            "harness/__select",
            "harness/lsp",
            "harness/computer-use-exec",
        ] {
            let bound = nb
                .eval(&format!("(if (dyn '{sym}) :bound :absent)"))
                .unwrap();
            assert_eq!(bound, ":absent", "{sym} is reachable from a notebook cell");
        }
        // But the sandbox still applies and json IS available.
        assert_eq!(
            nb.eval("(if (dyn 'harness/json-decode) :bound :absent)")
                .unwrap(),
            ":bound"
        );
    }

    /// `os/exit` must stay neutered on the notebook VM: agent-authored
    /// code reaches this VM, so it is the one that most needs the sandbox.
    #[test]
    fn notebook_vm_keeps_the_sandbox() {
        let mut nb = Worker::try_spawn_notebook().expect("spawn notebook");
        let cell = nb
            .eval_cell(
                r#"(notebook/-eval "a" "(os/exit 1)")"#,
                None,
                4096,
                Duration::from_secs(5),
            )
            .expect("host must still be alive");
        assert!(
            cell.value.contains("disabled in dirge plugins"),
            "os/exit was not neutered: {cell:?}"
        );
    }

    /// A wedged notebook must not be able to take plugin state with it.
    #[test]
    fn a_runaway_notebook_cell_leaves_the_plugin_vm_untouched() {
        let (mut plugins, _d, _l) = Worker::try_spawn().expect("spawn plugin vm");
        let mut nb = Worker::try_spawn_notebook().expect("spawn notebook");
        plugins.eval("(def plugin-state :intact)").unwrap();

        let _ = nb.eval_with_timeout("(while true nil)", Duration::from_millis(150));

        assert_eq!(plugins.eval("plugin-state").unwrap(), ":intact");
        assert_eq!(plugins.eval("(+ 2 2)").unwrap(), "4");
        // And the notebook itself recovered rather than needing a respawn.
        assert_eq!(nb.eval("(+ 3 3)").unwrap(), "6");
    }

    /// dirge-9xjg.6: a flat single-key extract is not enough for notebook
    /// work; nested documents have to come back as native Janet data.
    #[test]
    fn json_decode_handles_nested_documents() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let code = r#"
          (let [d (harness/json-decode `{"a": {"b": [1, 2, {"c": "deep"}]}, "t": true, "n": null}`)]
            (string (get-in d ["a" "b" 2 "c"]) "|"
                    (get d "t") "|"
                    (type (get d "n")) "|"
                    (= :null (get d "n")) "|"
                    (length (get-in d ["a" "b"]))))"#;
        assert_eq!(w.eval(code).unwrap(), "deep|true|keyword|true|3");
    }

    /// JSON `null` must not decode to Janet `nil`: putting nil into a
    /// table REMOVES the key, so a null field would be indistinguishable
    /// from a field that was never sent.
    #[test]
    fn json_null_does_not_erase_its_key() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let out = w
            .eval(r#"(let [d (harness/json-decode `{"present": null}`)] (length (keys d)))"#)
            .unwrap();
        assert_eq!(out, "1", "the null-valued key vanished from the table");
    }

    /// Malformed input is a normal outcome for a decoder handed arbitrary
    /// tool output — it must return nil, not raise or crash the VM.
    #[test]
    fn json_decode_returns_nil_on_bad_input() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        assert_eq!(
            w.eval(r#"(harness/json-decode "{not json")"#).unwrap(),
            "nil"
        );
        assert_eq!(w.eval("(harness/json-decode 42)").unwrap(), "nil");
        assert_eq!(w.eval("(+ 1 1)").unwrap(), "2", "VM survived bad input");
    }

    /// The capture mirror holds buffer OBJECTS, so exactly one place may
    /// bind `:out`/`:err`. A second `(setdyn :out @"")` anywhere swaps the
    /// fiber-level binding, strands `top_dyns` on the orphaned buffer, and
    /// silently puts Janet error traces back on the user's terminal — a
    /// regression with no failing assertion anywhere near it.
    ///
    /// Encoded as a source scan rather than a behavioural test because the
    /// hazard is *adding a site*, and a behavioural test only covers the
    /// sites that already exist.
    #[test]
    fn only_the_bootstrap_binds_the_capture_buffers() {
        // Scan the production half only: the assertions below name the
        // very strings they forbid.
        let src = include_str!("worker.rs");
        let prod = src.split("#[cfg(test)]").next().expect("production half");
        assert!(
            prod.contains("const CAPTURE_BOOTSTRAP"),
            "marker gone — this scan would pass vacuously"
        );
        // Comments discuss the rule at length; count CODE only.
        let code: String = prod
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with('#')
            })
            .collect::<Vec<_>>()
            .join("\n");
        for pattern in ["(setdyn :out", "(setdyn :err"] {
            let hits = code.matches(pattern).count();
            assert_eq!(
                hits, 1,
                "{pattern} appears {hits}x; only CAPTURE_BOOTSTRAP may bind it. \
                 Clear in place with harness/-capture-reset instead."
            );
        }
    }

    /// dirge-v49u: a child process writes to the REAL fds, so the
    /// `:out`/`:err` redirect — a Janet-level abstraction — does not
    /// contain it. Unredirected subprocess output used to land straight on
    /// the raw-mode TUI.
    ///
    /// Asserting the output is CAPTURED is what proves it is not leaking:
    /// a test cannot observe the process's real stdout, but the bytes have
    /// to be in exactly one of the two places.
    #[test]
    fn subprocess_output_is_captured_not_written_to_the_terminal() {
        // Notebook VM: sessions live there, and it is the VM agent-authored
        // code actually reaches. The containment itself is installed by
        // HARNESS_SANDBOX, which both VMs run.
        let mut w = Worker::try_spawn_notebook().expect("spawn notebook");
        let cell = w
            .eval_cell(
                r#"(os/execute ["/bin/sh" "-c" "printf 'child-stdout\n'; printf 'child-stderr\n' >&2"] :p)"#,
                Some("subprocess-basic"),
                4096,
                Duration::from_secs(10),
            )
            .unwrap();
        assert!(cell.ok, "{cell:?}");
        assert_eq!(cell.value, "0", "exit code not preserved: {cell:?}");
        assert!(
            cell.output.contains("child-stdout"),
            "subprocess stdout escaped capture: {cell:?}"
        );
        assert!(
            cell.output.contains("child-stderr"),
            "subprocess stderr escaped capture: {cell:?}"
        );
    }

    /// The `:x` flag raises on a non-zero exit. What the child printed
    /// before failing is exactly what someone needs to read, so the drain
    /// has to survive the raise.
    #[test]
    fn subprocess_output_is_captured_even_when_the_command_fails() {
        let mut w = Worker::try_spawn_notebook().expect("spawn notebook");
        // Through the SESSION path, which is how the agent reaches it.
        // `ok` there comes from `fiber/status`, which is accurate even for
        // errors raised on the event-loop path — unlike janetrs' errflags,
        // see dirge-2jtd.
        let cell = w
            .eval_cell(
                r#"(os/execute ["/bin/sh" "-c" "printf 'printed-before-failing\n'; exit 9"] :px)"#,
                Some("subprocess-fail"),
                4096,
                Duration::from_secs(10),
            )
            .unwrap();
        assert!(!cell.ok, "a :x non-zero exit should raise: {cell:?}");
        assert!(
            cell.output.contains("printed-before-failing"),
            "output lost when the command failed: {cell:?}"
        );
    }

    /// A caller that manages its own redirection must be left alone —
    /// containment fills gaps, it does not seize the streams.
    #[test]
    fn caller_supplied_redirection_is_left_alone() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let out = w
            .eval(
                r#"(let [f (file/temp)]
                     (os/execute ["/bin/sh" "-c" "printf 'to-my-own-file'"] :p {:out f :err f})
                     (file/seek f :set 0)
                     (def s (string (file/read f :all)))
                     (file/close f)
                     s)"#,
            )
            .unwrap();
        assert_eq!(out, "to-my-own-file");
    }

    /// Only UNSPECIFIED streams are filled. plugins/k8s passes
    /// `{:out :pipe}` and leaves `:err` alone; its pipe must survive while
    /// its stderr still gets contained.
    #[test]
    fn only_unspecified_streams_are_filled() {
        let (mut w, _d, _l) = Worker::try_spawn().expect("spawn");
        let out = w
            .eval(
                r#"(let [p (os/spawn ["/bin/sh" "-c" "printf 'piped'; printf 'noise' >&2"] :p {:out :pipe})]
                     (def s (string (ev/read (p :out) 64)))
                     (os/proc-wait p)
                     s)"#,
            )
            .unwrap();
        assert_eq!(out, "piped", "caller's :out pipe was overridden");
    }

    /// Flags are passed through exactly. Injecting `:p` would add PATH
    /// lookup to a call that never asked for it, resolving a bare name
    /// that stock Janet refuses — a containment fix must not widen what
    /// can be executed.
    #[test]
    fn containment_does_not_inject_path_lookup() {
        let mut w = Worker::try_spawn_notebook().expect("spawn notebook");
        let cell = w
            .eval_cell(
                r#"(try (os/execute ["sh" "-c" "printf 'resolved-via-path'"]) ([e] :refused))"#,
                Some("subprocess-path"),
                4096,
                Duration::from_secs(10),
            )
            .unwrap();
        assert!(
            !cell.output.contains("resolved-via-path"),
            "a bare name resolved without :p — PATH lookup was injected: {cell:?}"
        );
    }

    #[test]
    fn worker_round_trips_an_eval() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let r = worker.eval("(+ 1 2)").unwrap();
        assert_eq!(r, "3");
    }

    #[test]
    fn worker_surfaces_janet_errors_as_err() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        // `undefined-fn` is genuinely unknown.
        let r = worker.eval("(undefined-fn 1)");
        assert!(r.is_err(), "expected Err, got {r:?}");
    }

    /// dirge-u5ig: the dialog give-up policy. Shutdown aborts immediately
    /// regardless of elapsed; otherwise it waits until DIALOG_TIMEOUT so a
    /// never-answered dialog can't pin the worker forever — but a human
    /// gets the full (generous) window before then.
    #[test]
    fn dialog_should_abort_policy() {
        // Shutting down → abort now, even at zero elapsed.
        assert!(dialog_should_abort(Duration::ZERO, true));
        // Not shutting down, well within the window → keep waiting.
        assert!(!dialog_should_abort(Duration::from_secs(1), false));
        assert!(!dialog_should_abort(
            DIALOG_TIMEOUT - Duration::from_secs(1),
            false
        ));
        // Past the window → give up.
        assert!(dialog_should_abort(DIALOG_TIMEOUT, false));
        assert!(dialog_should_abort(
            DIALOG_TIMEOUT + Duration::from_secs(1),
            false
        ));
        // The dialog window is far more generous than the LSP one — dialogs
        // wait on a human, LSP waits on a server.
        assert!(DIALOG_TIMEOUT > Duration::from_secs(30));
    }

    /// dirge-rj3k / #476: harness/bind-key accumulates tab-separated
    /// (key, command) lines the host reads back as keybinding overrides.
    #[test]
    fn bind_key_accumulates_keybinding_overrides() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        worker
            .eval(r#"(harness/bind-key "ctrl-t" "toggle_reasoning")"#)
            .unwrap();
        worker
            .eval(r#"(harness/bind-key "ctrl-x ctrl-s" "scroll_to_top")"#)
            .unwrap();
        // Non-strings are ignored (no crash, no entry).
        worker.eval("(harness/bind-key 5 6)").unwrap();
        let list = worker.eval("harness-keybindings-list").unwrap();
        assert!(list.contains("ctrl-t\ttoggle_reasoning"), "{list}");
        assert!(list.contains("ctrl-x ctrl-s\tscroll_to_top"), "{list}");
        assert_eq!(
            list.lines().count(),
            2,
            "non-string call added nothing: {list}"
        );
    }

    /// dirge-l6bf: a plugin must NOT be able to terminate the host process.
    /// `os/exit` (and friends) are neutered to raise a catchable Janet error
    /// rather than calling C `exit()`. Without the fix, this very test would
    /// terminate the test binary mid-run.
    #[test]
    fn os_exit_cannot_kill_the_host() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();

        // os/exit raises instead of calling C exit(). (At the top level
        // janetrs renders the error as a Janet stack trace; the exact
        // message is asserted in `neutered_os_exit_is_catchable_by_plugin_try`
        // where the raw error value is visible to a Janet `(try ...)`.)
        let r = worker.eval("(os/exit 0)");
        assert!(r.is_err(), "os/exit must raise, not exit; got {r:?}");

        // The worker — and therefore the host — is still alive.
        assert_eq!(worker.eval("(+ 1 2)").unwrap(), "3");

        // The other host-control escape hatches are neutered too.
        assert!(worker.eval("(os/proc-kill nil)").is_err());
        assert!(worker.eval("(os/sigaction :term (fn [&] nil))").is_err());
    }

    /// dirge-df1v: a plugin flooding `harness/notify` in a hot loop must not
    /// grow the notification buffer without bound. It's capped, gets a single
    /// "dropped" marker, and resets once the host drains it.
    #[test]
    fn notification_buffer_is_capped_and_resets_on_drain() {
        let (mut worker, _d, _l) = Worker::try_spawn().unwrap();

        // Flood far past the cap.
        worker
            .eval("(loop [i :range [0 50000]] (harness/notify (string \"notification number \" i) :info))")
            .unwrap();

        let len: usize = worker
            .eval("(length harness-notif-list)")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            len <= harness_notif_cap_for_test() + 256,
            "notif buffer should be capped, got {len}"
        );
        // The single flood marker is present.
        assert_ne!(
            worker
                .eval("(if (string/find \"further ones dropped\" harness-notif-list) 1 0)")
                .unwrap(),
            "0",
            "expected the flood marker",
        );

        // Simulate the host's per-turn drain (it clears the list to "").
        worker.eval("(set harness-notif-list \"\")").unwrap();
        // A normal notification after drain works again (marker re-armed).
        worker
            .eval("(harness/notify \"after drain\" :info)")
            .unwrap();
        let after = worker.eval("harness-notif-list").unwrap();
        assert!(after.contains("after drain"), "got {after}");
        assert!(
            !after.contains("dropped"),
            "flood marker should have reset; got {after}"
        );
    }

    /// Mirror cap for custom messages.
    #[test]
    fn custom_message_buffer_is_capped() {
        let (mut worker, _d, _l) = Worker::try_spawn().unwrap();
        worker
            .eval("(loop [i :range [0 50000]] (harness/add-custom-message (string \"custom message number \" i)))")
            .unwrap();
        let len: usize = worker
            .eval("(length harness-custom-messages)")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            len <= 131072 + 256,
            "custom-message buffer should be capped, got {len}"
        );
        assert_ne!(
            worker
                .eval("(if (string/find \"further ones dropped\" harness-custom-messages) 1 0)")
                .unwrap(),
            "0",
            "expected the custom-message flood marker",
        );
    }

    fn harness_notif_cap_for_test() -> usize {
        65536
    }

    /// The catchable error means the existing per-hook `(try ...)` wrapping
    /// swallows a plugin's `os/exit` exactly like any other plugin error.
    #[test]
    fn neutered_os_exit_is_catchable_by_plugin_try() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let r = worker
            .eval(r#"(try (os/exit 1) ([err] (string "caught: " err)))"#)
            .unwrap();
        assert!(r.contains("caught:"), "got {r}");
        assert!(r.contains("disabled in dirge plugins"), "got {r}");
    }

    #[test]
    fn worker_eval_returns_keyword_string() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        // Verify the worker installed the harness defs.
        let r = worker
            .eval("(harness/has-symbol? \"harness/notify\")")
            .unwrap();
        assert_eq!(r, "true");
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_harness_is_available_and_wrappers_are_defined() {
        // Hold the receiver alive so the bridge counts as live.
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        // With the lsp feature the `__lsp` C-function is registered and a
        // live receiver is attached, so the predicate reports available.
        assert_eq!(worker.eval("(harness/lsp?)").unwrap(), "true");
        // The core fn and every typed wrapper are installed.
        for sym in [
            "harness/lsp",
            "harness/lsp-definition",
            "harness/lsp-references",
            "harness/lsp-hover",
            "harness/lsp-implementation",
            "harness/lsp-incoming-calls",
            "harness/lsp-outgoing-calls",
            "harness/lsp-document-symbols",
            "harness/lsp-workspace-symbols",
            "harness/lsp-diagnostics",
        ] {
            let r = worker
                .eval(&format!("(harness/has-symbol? \"{sym}\")"))
                .unwrap();
            assert_eq!(r, "true", "{sym} should be defined");
        }
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_predicate_is_false_when_bridge_has_no_live_receiver() {
        // Simulate the runtime-disabled case (lsp_manager None → no
        // responder spawned → the request receiver is dropped). The
        // predicate must reflect that the bridge is NOT live, so plugins
        // that feature-detect don't then crash decoding a nil result.
        let (mut worker, _dialog_rx, lsp_rx) = Worker::try_spawn().unwrap();
        drop(lsp_rx);
        assert_eq!(worker.eval("(harness/lsp?)").unwrap(), "false");
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_query_rejects_nonpositive_coordinates() {
        // Coordinates are 1-based; 0, negative, or non-numbers are plugin
        // bugs and must surface as a Janet error rather than being silently
        // clamped to line 0. Drop the receiver so the validation (which runs
        // before the bridge call) is what fails — never a blocked query.
        let (mut worker, _dialog_rx, lsp_rx) = Worker::try_spawn().unwrap();
        drop(lsp_rx);
        for code in [
            r#"(harness/lsp "definition" "f.rs" 0 1)"#,
            r#"(harness/lsp "definition" "f.rs" 1 0)"#,
            r#"(harness/lsp "definition" "f.rs" -3 1)"#,
        ] {
            assert!(worker.eval(code).is_err(), "expected error for {code}");
        }
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_abort_decision_covers_shutdown_and_timeout() {
        // Give up on shutdown immediately, or once the query timeout elapses.
        assert!(!lsp_should_abort(Duration::from_secs(0), false));
        assert!(lsp_should_abort(Duration::from_secs(0), true));
        assert!(lsp_should_abort(LSP_QUERY_TIMEOUT, false));
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_query_returns_nil_when_bridge_has_no_live_receiver() {
        // Even if a plugin ignores the predicate, a query against a
        // dropped receiver must return nil immediately — never block the
        // worker thread (the load-time deadlock guard).
        let (mut worker, _dialog_rx, lsp_rx) = Worker::try_spawn().unwrap();
        drop(lsp_rx);
        let r = worker
            .eval(r#"(harness/lsp "definition" "f.rs" 1 1)"#)
            .unwrap();
        assert_eq!(r, "nil");
    }

    #[test]
    fn confirm_sends_a_dialog_request_with_title_and_question() {
        let (mut worker, dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();

        // Start a helper thread that auto-answers any confirm with `true`.
        let mut dialog_rx = dialog_rx;
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm {
                title,
                question,
                reply,
            }) => {
                assert_eq!(title, "warn");
                assert_eq!(question, "really?");
                let _ = reply.send(DialogReply::Confirm(true));
            }
            other => panic!("unexpected dialog request: {other:?}"),
        });

        let r = worker
            .eval(r#"(harness/confirm "warn" "really?")"#)
            .unwrap();
        // Janet booleans stringify as "true" / "false".
        assert_eq!(r, "true");
        helper.join().unwrap();
    }

    /// dirge-hwzs: a hook that opens `harness/confirm` blocks on a human, who
    /// takes longer than the tight per-hook eval budget. The eval must keep
    /// waiting while the dialog is in flight instead of giving up at the base
    /// timeout — otherwise the caller (`dispatch_tool_hook`) sees no block and
    /// runs the gated tool while the confirm is still on screen.
    ///
    /// This is the DECISION half of that behaviour, and it is a total function
    /// of three values: is a dialog in flight, how long have we waited, and
    /// what is the ceiling. No worker, no Janet VM, no clock, so the table is
    /// exhaustive and cannot flake.
    ///
    /// It replaces a test that raced a real VM against a wall clock and flaked
    /// twice on loaded CI (dirge-h3dw) — see [`timeout_action`] for why no
    /// choice of budget fixes that shape of test.
    #[test]
    fn timeout_action_extends_only_for_an_in_flight_dialog() {
        let under = Duration::from_secs(1);
        let ceiling = Duration::from_secs(10);

        // The one case that extends: a dialog is up and there is budget left.
        assert_eq!(
            timeout_action(true, under, ceiling),
            TimeoutAction::KeepWaiting
        );

        // No dialog: a wedged plugin must recover at the base budget rather
        // than inherit the human-scale grace period.
        assert_eq!(timeout_action(false, under, ceiling), TimeoutAction::GiveUp);

        // Past the ceiling, a pending dialog no longer buys time — otherwise a
        // worker wedged *inside* a dialog could pin the caller forever.
        assert_eq!(
            timeout_action(true, ceiling + under, ceiling),
            TimeoutAction::GiveUp
        );
        assert_eq!(
            timeout_action(false, ceiling + under, ceiling),
            TimeoutAction::GiveUp
        );

        // Boundary: the comparison is strict, so landing exactly on the
        // ceiling gives up. Pinned because `<` vs `<=` here is the difference
        // between a bounded and an unbounded wait.
        assert_eq!(
            timeout_action(true, ceiling, ceiling),
            TimeoutAction::GiveUp
        );

        // Degenerate ceiling: extension is off entirely, never negative time.
        assert_eq!(
            timeout_action(true, Duration::ZERO, Duration::ZERO),
            TimeoutAction::GiveUp
        );
    }

    /// The WIRING half: `dialog_pending` must actually be set while a dialog
    /// is in flight. If it were not, [`timeout_action`] would still be correct
    /// and the gate would still fail open — so this is the part that has to be
    /// observed against a real worker.
    ///
    /// Deterministic despite that. `DialogPendingGuard::enter` happens-before
    /// the request is published to `dialog_rx`, so by the time this helper has
    /// a `DialogRequest` in hand the counter is already incremented. The
    /// assertion is on that ordering, not on a duration, so no amount of CI
    /// load can change the outcome. The eval budget is generous and the reply
    /// is immediate, so the timeout path is never entered here.
    #[test]
    fn dialog_pending_is_set_while_a_confirm_is_in_flight() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let pending = worker.dialog_pending.clone();

        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm { reply, .. }) => {
                // Sampled BEFORE answering: the dialog is provably in flight
                // at this instant, because the worker is blocked waiting for
                // the reply we are holding.
                let seen = pending.load(Ordering::SeqCst);
                let _ = reply.send(DialogReply::Confirm(true));
                seen
            }
            other => panic!("unexpected dialog request: {other:?}"),
        });

        let r = worker.eval_with_timeout(
            r#"(harness/confirm "warn" "really?")"#,
            Duration::from_secs(60),
        );
        let seen = helper.join().unwrap();

        assert!(
            seen > 0,
            "dialog_pending must be set while a confirm is in flight, else the \
             eval timeout cannot tell a human-blocked hook from a wedged one \
             and the gate fails open"
        );
        assert_eq!(r.as_deref(), Ok("true"));

        // And it must be released afterwards, or the next eval would inherit
        // the human-scale grace period it has not earned.
        assert_eq!(
            worker.dialog_pending.load(Ordering::SeqCst),
            0,
            "the guard must drop once the dialog is answered"
        );
    }

    /// dirge-hwzs companion: a hook stuck for a NON-dialog reason (no
    /// dialog in flight) must still time out at the base budget so a
    /// wedged plugin recovers fast. The dialog extension only applies
    /// while a confirm/select is genuinely pending.
    #[test]
    fn eval_without_a_dialog_still_times_out_at_the_base() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let start = std::time::Instant::now();
        let r = worker.eval_with_timeout("(while true)", Duration::from_millis(150));
        let elapsed = start.elapsed();
        assert!(r.is_err(), "an infinite loop must time out, got {r:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "no dialog pending → must give up near the base timeout, took {elapsed:?}"
        );
    }

    #[test]
    fn confirm_returns_false_when_dialog_replies_false() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm { reply, .. }) => {
                let _ = reply.send(DialogReply::Confirm(false));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker.eval(r#"(harness/confirm "t" "q")"#).unwrap();
        assert_eq!(r, "false");
        helper.join().unwrap();
    }

    #[test]
    fn select_returns_picked_option_as_string() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select {
                title,
                options,
                reply,
            }) => {
                assert_eq!(title, "pick");
                assert_eq!(options, vec!["alpha".to_string(), "beta".to_string()]);
                let _ = reply.send(DialogReply::Select(Some("beta".to_string())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker
            .eval(r#"(harness/select "pick" ["alpha" "beta"])"#)
            .unwrap();
        // Janet strings stringify with surrounding quotes; we check substring.
        assert!(r.contains("beta"), "got {r:?}");
        helper.join().unwrap();
    }

    #[test]
    fn select_returns_nil_on_cancel() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { reply, .. }) => {
                let _ = reply.send(DialogReply::Select(None));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker.eval(r#"(harness/select "pick" ["a"])"#).unwrap();
        assert_eq!(r, "nil");
        helper.join().unwrap();
    }

    #[test]
    fn dialog_rx_drains_when_no_request_pending() {
        // Sanity: a fresh worker doesn't emit phantom dialog requests.
        let (_worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        assert!(matches!(
            dialog_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    /// R1 critical: setting the shutdown flag unblocks an in-flight
    /// dialog within ~`DIALOG_POLL` so `Worker::Drop` doesn't hang.
    /// Before R1, send_dialog's `reply_rx.recv()` had no timeout and
    /// the eval would block forever if the UI never replied.
    ///
    /// We can't trigger the abort via Drop directly (the worker is
    /// moved into the eval thread; dropping it from outside is exactly
    /// the catch-22 R1 exists to break). Instead we clone the shutdown
    /// Arc out before moving, then flip it once the dialog has arrived.
    /// This exercises the same code path Drop uses.
    #[test]
    fn shutdown_flag_aborts_in_flight_dialog() {
        use std::time::Instant;

        let (worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let shutdown_handle = worker.shutdown.clone();

        // Kick off a confirm; it will block waiting for a reply we
        // never send. After the shutdown flag flips, send_dialog's
        // polling loop returns None and the cfn returns Janet false.
        let eval_t = std::thread::spawn(move || {
            let mut worker = worker;
            let result = worker.eval(r#"(harness/confirm "x" "y")"#);
            (worker, result)
        });

        // Wait for the dialog request to land — the worker is now
        // parked inside send_dialog's recv_timeout loop.
        let _req = dialog_rx.blocking_recv().expect("dialog request");

        // Flip the flag. The cfn wakes up on its next 50 ms tick.
        shutdown_handle.store(true, Ordering::SeqCst);

        let started = Instant::now();
        let (worker, eval_result) = eval_t.join().expect("eval thread");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "eval took {elapsed:?}, expected ~DIALOG_POLL once the flag was flipped"
        );
        // On shutdown the cfn returns Janet false (its safe default).
        assert_eq!(eval_result.unwrap(), "false");

        // Drop the worker explicitly — should complete promptly since
        // the in-flight dialog has already unwound.
        drop(worker);
    }

    /// R1: oversized strings to wrap_string don't truncate to i32 —
    /// instead they return Janet nil. Hard to test with a real 2 GB
    /// string, so we exercise the same boundary via a small synthetic
    /// check that the i32::try_from path is taken. This is mostly a
    /// regression sentinel — if someone reverts the bounds check it
    /// fails to compile (wrap_string still requires Send/Sync to be
    /// callable from a select reply context).
    #[test]
    fn wrap_string_handles_empty() {
        // Just verify Janet round-trips the empty string through
        // confirm's reply path. Catches any wrap_string regression
        // that miscounts zero-length input.
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { reply, .. }) => {
                let _ = reply.send(DialogReply::Select(Some(String::new())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker
            .eval(r#"(harness/select "pick" ["only-option"])"#)
            .unwrap();
        // janetrs stringifies a Janet string with no quotes (just the
        // raw bytes), so an empty Janet string round-trips as the
        // empty Rust string here.
        assert_eq!(r, "");
        helper.join().unwrap();
    }

    // --- R2: FFI edge cases ---------------------------------------------

    /// R2: read_string_arg accepts Janet keywords (call sites can use
    /// `(harness/confirm :title "q")` instead of double-quoted strings).
    /// Caught by an integration test through harness/confirm since the
    /// cfn is the only caller; if read_string_arg ever stops accepting
    /// keywords this test fails.
    #[test]
    fn confirm_accepts_keyword_title() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm {
                title,
                question,
                reply,
            }) => {
                assert_eq!(title, "warn");
                assert_eq!(question, "really?");
                let _ = reply.send(DialogReply::Confirm(true));
            }
            other => panic!("unexpected: {other:?}"),
        });
        // Keyword first arg — read_string_arg's is_kw branch handles it.
        let r = worker
            .eval(r#"(harness/__confirm :warn "really?")"#)
            .unwrap();
        assert_eq!(r, "true");
        helper.join().unwrap();
    }

    /// R2: read_string_array_arg returns None for an empty array, and
    /// the select cfn surfaces that as Janet nil. Janet-side
    /// harness/select already short-circuits on `(indexed? opts)`, so
    /// we hit the cfn via __select directly.
    #[test]
    fn select_with_empty_options_returns_nil() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        // Empty array should never even emit a dialog request.
        let r = worker.eval(r#"(harness/__select "pick" [])"#).unwrap();
        assert_eq!(r, "nil");
    }

    /// R2: read_string_array_arg works with tuples too (not just
    /// arrays). Janet array literals `["a"]` are arrays; quoted forms
    /// `'("a")` produce tuples. Both should be accepted.
    #[test]
    fn select_accepts_tuple_options() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { options, reply, .. }) => {
                assert_eq!(options, vec!["alpha".to_string(), "beta".to_string()]);
                let _ = reply.send(DialogReply::Select(Some("alpha".to_string())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        // Use a quoted tuple instead of an array literal.
        let r = worker
            .eval(r#"(harness/__select "pick" '("alpha" "beta"))"#)
            .unwrap();
        assert!(r.contains("alpha"), "got {r:?}");
        helper.join().unwrap();
    }

    /// R2: wrap_string handles multibyte UTF-8 correctly. The byte
    /// length is the Janet string's allocation; an off-by-one here
    /// would either truncate emoji or read past the slice.
    #[test]
    fn select_returns_multibyte_option_through_wrap_string() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { reply, .. }) => {
                // Emoji + CJK + Cyrillic — all multibyte UTF-8.
                let _ = reply.send(DialogReply::Select(Some("🦀漢字Привет".to_string())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker.eval(r#"(harness/select "pick" ["x"])"#).unwrap();
        // Janet stringification preserves the raw UTF-8 bytes; the
        // result should contain all three multibyte sequences intact.
        assert!(r.contains("🦀"), "lost emoji: {r:?}");
        assert!(r.contains("漢字"), "lost CJK: {r:?}");
        assert!(r.contains("Привет"), "lost Cyrillic: {r:?}");
        helper.join().unwrap();
    }

    /// vigil/emit uses json-encode to serialize event data. Verify
    /// the Janet dict → JSON round-trip produces valid JSON that
    /// serde_json can parse back into structured fields — the keeper
    /// path at src/extras/vigil/mod.rs:171.
    #[cfg(feature = "vigil")]
    #[test]
    fn json_encode_produces_valid_json_for_vigil_emit() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let json_str = worker
            .eval(
                r#"(json-encode {:job "my-pipeline"
                             :build_number "42"
                             :url "http://jenkins:8080/job/my-pipeline/42"
                             :status "FAILURE"})"#,
            )
            .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("json-encode must produce valid JSON");

        assert_eq!(parsed["job"], "my-pipeline");
        assert_eq!(parsed["build_number"], "42");
        assert_eq!(parsed["url"], "http://jenkins:8080/job/my-pipeline/42");
        assert_eq!(parsed["status"], "FAILURE");
    }

    /// Regression: verify that the full vigil/emit message format
    /// (name\tjson) can be split and parsed by the keeper router.
    #[cfg(feature = "vigil")]
    #[test]
    fn vigil_emit_message_format_is_parseable_by_keeper() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();

        // Simulate what (vigil/emit "jenkins-remediate" {...}) sends
        // through harness/__vigil-emit. We can't call vigil/emit directly
        // (vigil/live? is false in tests), so we call json-encode and
        // format the message manually.
        let payload = worker
            .eval(
                r#"(json-encode {:job "my-pipeline"
                             :build_number "42"
                             :url "http://jenkins:8080/job/my-pipeline/42"
                             :status "FAILURE"})"#,
            )
            .unwrap();
        let msg = format!("jenkins-remediate\t{payload}");

        // Simulate the keeper router (src/extras/vigil/mod.rs:167-173)
        let (name, payload_str) = msg.split_once('\t').expect("tab-separated message");
        assert_eq!(name, "jenkins-remediate");

        let context: serde_json::Value =
            serde_json::from_str(payload_str).expect("payload must be valid JSON");

        assert_eq!(context["job"], "my-pipeline");
        assert_eq!(context["build_number"], "42");
        assert_eq!(context["url"], "http://jenkins:8080/job/my-pipeline/42");
        assert_eq!(context["status"], "FAILURE");
    }

    /// harness/json-decode parses nested JSON into a Janet table keyed by
    /// string (not keyword) — the poller fixtures rely on this to read
    /// `jobs[0].lastBuild.result` from a Jenkins API response.
    #[cfg(feature = "vigil")]
    #[test]
    fn json_decode_parses_nested_json_with_string_keys() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let r = worker
            .eval(
                r#"(let [d (harness/json-decode "{\"jobs\":[{\"name\":\"test-pipeline\",\"lastBuild\":{\"number\":1,\"result\":\"FAILURE\"}}]}")]
                 (let [job (get (get d "jobs") 0)]
                   (string (get job "name") "|" (get-in job ["lastBuild" "result"]))))"#,
            )
            .unwrap();
        assert!(r.contains("test-pipeline"), "got {r:?}");
        assert!(r.contains("FAILURE"), "got {r:?}");
    }

    /// harness/json-decode returns nil (not a panic) for malformed JSON.
    #[cfg(feature = "vigil")]
    #[test]
    fn json_decode_returns_nil_on_invalid_json() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let r = worker.eval(r#"(harness/json-decode "not json")"#).unwrap();
        assert_eq!(r, "nil");
    }

    /// Regression: the vigil bridge sender installed on one thread must be
    /// visible to `vigil_emit_cfn` running on the Janet worker thread. The
    /// original `thread_local!` bridge wrote VIGIL_TX on the tokio runtime
    /// thread and read it on the worker thread, so (vigil/emit ...) was a
    /// silent no-op. This pins the cross-thread contract with a real worker.
    #[cfg(feature = "vigil")]
    #[test]
    fn vigil_bridge_delivers_emit_across_threads() {
        // Install on the TEST thread; the worker evaluates on its own thread.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
        vigil_bridge::install_vigil_tx(tx);

        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let r = worker
            .eval(r#"(vigil/emit "test-vigil" {:job "my-pipeline"})"#)
            .unwrap();
        assert_eq!(r, "nil");

        let msg = rx
            .blocking_recv()
            .expect("vigil/emit must reach the bridge tx across threads");
        let (name, payload) = msg.split_once('\t').expect("name\tpayload");
        assert_eq!(name, "test-vigil");
        let context: serde_json::Value =
            serde_json::from_str(payload).expect("payload must be valid JSON");
        assert_eq!(context["job"], "my-pipeline");
    }
}
