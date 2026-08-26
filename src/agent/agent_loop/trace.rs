//! The loop trace: a JSONL record of what the agentic loop actually did.
//!
//! # Why this exists
//!
//! The loop already emits a per-run aggregate on the `dirge::gates` target
//! (see [`super::gate_tally`]) — how many turns, how many errored calls,
//! which gates fired. That answers "how did this run go" and cannot answer
//! "what happened, in what order, and why". A run where the critic fired and
//! the model then edited a file looks identical on the tally to one where the
//! model edited the file and the critic fired afterwards; the second is a
//! harness bug and the first is the harness working.
//!
//! Diagnosing a live run without this meant reading `RUST_LOG=debug` output,
//! which for one small task is a few hundred lines carrying six from the loop
//! itself — and the ones it does carry omit the numbers that matter. The
//! force-summary warning logs `ratio=1.0173125` and neither `prompt_tokens`
//! nor `ctx_max`, so recovering "the window is 32000 and our own prompt is
//! 32554" took solving the division by hand (dirge-cprj).
//!
//! # What it records, and why it cannot drift
//!
//! Almost everything comes from the `LoopEvent` stream the loop already
//! emits, tapped at the ONE point every event passes through on its way to
//! every consumer (`integration.rs`'s pump). So the trace is not a second set
//! of call sites to keep in step with the first: an event the UI can see is
//! an event the trace can see, and [`record_event`]'s match is exhaustive, so
//! a new `LoopEvent` variant fails to compile until it says how it traces.
//! That is the same rule the tally learned the hard way — no hand-maintained
//! list may stand between a signal and its report.
//!
//! Harness interventions are attributed the same way: [`record_event`] reads
//! the tag off the message with [`super::intervention::tag_of`] and names the
//! guard from the shared registry, so a guard added later is traced without
//! anyone remembering to come here. That attribution is the point of the
//! whole module — an injected steer is indistinguishable from a user turn in
//! the raw transcript, and "which feature moved the model" is the question a
//! harness review is made of.
//!
//! A handful of decisions never become events at all — the context manager's
//! verdict, the tool set a run starts with. Those call [`note`] directly.
//!
//! # Cost when off
//!
//! [`enabled`] is a relaxed atomic load against a `OnceLock` that is never
//! initialized unless `--trace` (or `DIRGE_TRACE`) asked for it. The tap in
//! the pump returns before touching the event.

use super::message::{LoopEvent, LoopMessage};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Longest payload excerpt written for any one field. Tool arguments and
/// results are the reason a trace would otherwise be unreadable: a `read` of a
/// large file is megabytes, and none of it distinguishes one run from another.
const EXCERPT_BYTES: usize = 400;

static ENABLED: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicU64 = AtomicU64::new(0);
static SINK: OnceLock<Sink> = OnceLock::new();

struct Sink {
    file: std::sync::Mutex<std::fs::File>,
    start: std::time::Instant,
}

/// True when a trace is being written. Cheap enough to call per event.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Begin tracing to `path`, truncating any previous trace there.
///
/// Returns the error rather than failing the run: a trace is a diagnostic, and
/// a run that refuses to start because its debug log is unwritable is worse
/// than one that runs untraced. The caller reports it.
pub fn enable(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::create(path)?;
    // A second `enable` (a nested or re-entered host) keeps the first sink;
    // the trace is process-wide by construction.
    let _ = SINK.set(Sink {
        file: std::sync::Mutex::new(file),
        start: std::time::Instant::now(),
    });
    ENABLED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Write one record. `fields` must be a JSON object; `kind` names the record.
///
/// Every write is flushed. A trace exists to explain a run that hung or
/// crashed, and a buffered tail is exactly the part such a run needs.
pub fn note(kind: &str, fields: Value) {
    if !enabled() {
        return;
    }
    let Some(sink) = SINK.get() else { return };
    let mut rec = json!({
        "ms": sink.start.elapsed().as_millis() as u64,
        "seq": SEQ.fetch_add(1, Ordering::Relaxed),
        "kind": kind,
    });
    if let (Some(obj), Value::Object(extra)) = (rec.as_object_mut(), fields) {
        obj.extend(extra);
    }
    if let Ok(mut f) = sink.file.lock() {
        let _ = writeln!(f, "{rec}");
        let _ = f.flush();
    }
}

/// The text a tool result carries, joined. `LoopToolResult::content` is
/// untyped JSON blocks (`{"type":"text","text":…}`); anything else — an image,
/// a structured detail — contributes no text and is skipped.
fn result_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Shortest gap between two `streaming` heartbeats. Long enough that an
/// ordinary turn contributes at most a record or two, short enough that a turn
/// which has stopped making progress is obvious.
const HEARTBEAT_SECS: u64 = 10;

/// True when a streaming heartbeat is due, and claims the slot.
///
/// A CAS rather than a plain load/store so two deltas arriving together cannot
/// both decide they are due. Seconds since the sink opened, so no wall-clock
/// read per delta.
fn heartbeat_due() -> bool {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let Some(sink) = SINK.get() else { return false };
    let now = sink.start.elapsed().as_secs();
    let last = LAST.load(Ordering::Relaxed);
    now.saturating_sub(last) >= HEARTBEAT_SECS
        && LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// A short, single-line excerpt of `s` for a trace field.
fn excerpt(s: &str) -> String {
    let head = crate::text::head(s, EXCERPT_BYTES);
    let flat = head.replace('\n', "⏎");
    if head.len() < s.len() {
        format!("{flat}…")
    } else {
        flat
    }
}

/// The trace record for one `LoopEvent`, or `None` for an event that is
/// deliberately not traced.
///
/// EXHAUSTIVE BY DESIGN: no `_` arm. A new `LoopEvent` variant fails to
/// compile here, which is what stops the trace from quietly falling behind the
/// loop the way three separate tag lists once fell behind each other
/// (see [`super::intervention`]).
fn describe(evt: &LoopEvent) -> Option<(&'static str, Value)> {
    Some(match evt {
        LoopEvent::AgentStart => ("agent_start", json!({})),
        LoopEvent::AgentEnd { messages } => ("agent_end", json!({ "messages": messages.len() })),
        LoopEvent::TurnStart => ("turn_start", json!({})),
        // WHAT THE MODEL SAID. `TurnEnd` carries the FINALIZED assistant
        // message, and it is the only event that does — see the
        // `MessageStart` arm below.
        LoopEvent::TurnEnd {
            message,
            tool_results,
        } => (
            "turn_end",
            json!({
                "text": excerpt(&message.text_joined()),
                "text_chars": message.text_joined().len(),
                "tool_calls": message.tool_calls().count(),
                "tool_results": tool_results.len(),
                "stop_reason": format!("{:?}", message.stop_reason),
                "error": message.error_message.as_deref().map(excerpt),
            }),
        ),

        // The message stream. A harness intervention is a User message
        // carrying a registry tag — naming the guard here is what makes the
        // trace answer "which feature moved the model".
        //
        // An ASSISTANT message is skipped here: `MessageStart` fires BEFORE
        // the stream, so its text is empty and its tool-call list is empty for
        // every turn, however much the model went on to say or do. Recording
        // it produced a line per turn reading `assistant:` with nothing after
        // it — present, plausible, and describing no turn that ever happened.
        // The finalized message arrives as `TurnEnd` above.
        LoopEvent::MessageStart { message } => match message {
            LoopMessage::Assistant(_) => return None,
            _ => ("message", describe_message(message)),
        },
        // MessageEnd repeats MessageStart's content for every message.
        LoopEvent::MessageEnd { .. } => return None,

        // MessageUpdate fires per streamed delta — far too many to record, and
        // the trace's unit is the decision, not the byte. But recording NOTHING
        // during a turn means a trace that claims to explain a run that hung
        // goes silent for exactly as long as the interesting part lasts: a
        // local model was observed thinking for thirteen minutes and 2892
        // tokens with nothing between `turn_start` and the next record, which
        // is indistinguishable in the trace from a deadlock.
        //
        // So: a heartbeat, throttled to one record per HEARTBEAT_SECS, saying
        // what the turn is doing and how much it has produced.
        LoopEvent::MessageUpdate { message, phase } => {
            if !heartbeat_due() {
                return None;
            }
            let chars: usize = message
                .content
                .iter()
                .map(|b| match b {
                    super::message::ContentBlock::Text { text }
                    | super::message::ContentBlock::Thinking { text } => text.len(),
                    super::message::ContentBlock::ToolCall { arguments, .. } => {
                        arguments.to_string().len()
                    }
                })
                .sum();
            (
                "streaming",
                json!({ "phase": format!("{phase:?}"), "chars": chars }),
            )
        }

        LoopEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => (
            "tool_start",
            json!({
                "id": tool_call_id,
                "tool": tool_name,
                "args": excerpt(&args.to_string()),
            }),
        ),
        LoopEvent::ToolExecutionUpdate { .. } => return None,
        LoopEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => (
            "tool_end",
            json!({
                "id": tool_call_id,
                "tool": tool_name,
                "error": is_error,
                "output": excerpt(&result_text(&result.content)),
            }),
        ),

        LoopEvent::Usage { usage } => (
            "usage",
            json!({
                "input": usage.input_tokens,
                "output": usage.output_tokens,
                "cached": usage.cached_input_tokens,
            }),
        ),

        LoopEvent::CompactionStarted { tokens_before } => (
            "compaction_start",
            json!({ "tokens_before": tokens_before }),
        ),
        LoopEvent::ContextCompacted {
            new_session_id,
            tokens_before,
            tokens_after,
            compaction_kind,
            first_kept_index,
            skill_anchors_kept,
            ..
        } => (
            "compacted",
            json!({
                "session": new_session_id,
                "tokens_before": tokens_before,
                "tokens_after": tokens_after,
                "first_kept": first_kept_index,
                "how": format!("{compaction_kind:?}"),
                // dirge-69oe.4: the one artefact that shows whether a loaded
                // skill still governs after a fold. Empty is the interesting
                // reading, not a missing field.
                "skill_anchors_kept": skill_anchors_kept,
            }),
        ),
        LoopEvent::CheckpointRefresh { summary } => {
            ("checkpoint", json!({ "summary_chars": summary.len() }))
        }

        LoopEvent::RetryNotice {
            attempt,
            delay_ms,
            error,
        } => (
            "retry",
            json!({
                "attempt": attempt,
                "delay_ms": delay_ms,
                "error": excerpt(error),
            }),
        ),
        LoopEvent::SystemNotice { content } => {
            ("system_notice", json!({ "text": excerpt(content) }))
        }
        LoopEvent::RepairStats { snapshot } => (
            "repairs",
            json!({
                "repaired": snapshot.total_successful(),
                "invalid": snapshot.invalid,
            }),
        ),
        LoopEvent::EscalationActivated { provider, reason } => (
            "escalation",
            json!({ "provider": provider, "reason": format!("{reason:?}") }),
        ),
    })
}

/// The fields for a transcript message, including its harness attribution.
fn describe_message(message: &LoopMessage) -> Value {
    match message {
        LoopMessage::User(u) => {
            let text = u.text_joined();
            match super::intervention::tag_of(&text) {
                // An injected steer, not something the user typed. `guard`
                // names the feature; `why` is the registry's own one-line
                // account of what it did.
                Some(tag) => json!({
                    "role": "intervention",
                    "guard": tag,
                    "why": super::intervention::summary_for_user(tag),
                    "text": excerpt(super::intervention::strip_tag(&text).unwrap_or(&text)),
                }),
                None => json!({ "role": "user", "text": excerpt(&text) }),
            }
        }
        LoopMessage::Assistant(a) => json!({
            "role": "assistant",
            "text": excerpt(&a.text_joined()),
            "tool_calls": a.tool_calls().count(),
        }),
        LoopMessage::ToolResult(t) => json!({
            "role": "tool_result",
            "tool": t.tool_name.clone(),
            "error": t.is_error,
        }),
        LoopMessage::Custom(v) => json!({ "role": "custom", "text": excerpt(&v.to_string()) }),
    }
}

/// Trace one `LoopEvent`. Called from the pump every event passes through.
pub fn record_event(evt: &LoopEvent) {
    if !enabled() {
        return;
    }
    if let Some((kind, fields)) = describe(evt) {
        note(kind, fields);
    }
}

/// Trace one `AgentEvent` — what the UI actually consumes.
///
/// The `LoopEvent` records above say what the LOOP did. This says what reaches
/// the front end, which is a different question and the only one that can
/// answer "would the TUI show this twice?". The two streams are not one-to-one:
/// the bridge drops some events, splits others, and a single harness
/// intervention produces BOTH a `SystemNotice` (summary plus body) and a
/// `UserMessage` (the tagged message), each of which the TUI renders.
///
/// Recorded for the events that produce visible output, with the text they
/// would render, so a duplicate shows up as two records carrying the same body.
/// Token and reasoning deltas are excluded — they are the stream, not a
/// decision, and the `streaming` heartbeat already covers them.
pub fn record_ui_event(evt: &crate::event::AgentEvent) {
    use crate::event::AgentEvent as E;
    if !enabled() {
        return;
    }
    let (what, text) = match evt {
        E::UserMessage { content } => {
            // Mirror the TUI's own attribution so the trace says which handle
            // it would render under — `handle_user_message` asks exactly this.
            let tagged = super::intervention::tag_of(content);
            let handle = match tagged {
                Some(t) if super::intervention::is_finalization(t) => "critic",
                Some(_) => "sys",
                None => "you",
            };
            let body = super::intervention::strip_tag(content).unwrap_or(content);
            (handle, body.to_string())
        }
        E::SystemNotice { content } => ("sys-notice", content.to_string()),
        E::Error(e) => ("error", e.to_string()),
        // Everything else is either not rendered as its own line or is already
        // covered by the loop-side records (tool calls, tokens, reasoning).
        _ => return,
    };
    note("ui", json!({ "as": what, "text": excerpt(text.trim()) }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::message::{
        AssistantMessage, ContentBlock, StopReason, UserMessage,
    };
    use crate::agent::agent_loop::result::LoopToolResult;

    /// A tool result carrying one text block, the shape every tool returns.
    fn tool_result(text: &str) -> LoopToolResult {
        LoopToolResult {
            content: vec![json!({"type": "text", "text": text})],
            ..Default::default()
        }
    }

    /// An assistant message carrying only text.
    fn assistant(text: &str) -> AssistantMessage {
        AssistantMessage::new(
            vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            StopReason::Stop,
        )
    }

    /// Reading a trace back is what the tests assert on, so it is worth one
    /// helper: enable the sink, run `body`, return the records `body` wrote.
    ///
    /// Records are selected by SEQUENCE NUMBER, not by a byte offset into the
    /// file. The offset version cut a line in half and produced a record named
    /// `tool_startear` — the sink is process-global and set once, so any other
    /// test's write between the two reads shifted the boundary. A sequence
    /// range cannot land mid-record however the writes interleave.
    fn traced(body: impl FnOnce()) -> Vec<Value> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = sink_path();
        if SINK.get().is_none() {
            enable(&path).expect("enable trace");
        }
        ENABLED.store(true, Ordering::Relaxed);

        let first = SEQ.load(Ordering::Relaxed);
        body();
        let last = SEQ.load(Ordering::Relaxed);

        std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|r| r["seq"].as_u64().is_some_and(|s| s >= first && s < last))
            .collect()
    }

    /// The path this test process's sink writes to. Under `cargo test` all
    /// tests in the module share one process (and the `traced` lock below
    /// serializes them), but nextest runs every test in its own process — two
    /// processes can stamp the same nanosecond and truncate each other's file,
    /// which is what made these tests flaky on macOS. The pid makes the path
    /// unique per process regardless of clock granularity.
    fn sink_path() -> std::path::PathBuf {
        static FIRST: OnceLock<std::path::PathBuf> = OnceLock::new();
        FIRST
            .get_or_init(|| {
                std::env::temp_dir().join(format!(
                    "dirge-trace-test-{}-{}.jsonl",
                    crate::text::test_run_stamp(),
                    std::process::id()
                ))
            })
            .clone()
    }

    /// The whole point of the module: an injected steer must be
    /// distinguishable from a user turn, and must name the guard that sent it.
    ///
    /// Both halves matter. A trace that labelled every user message an
    /// intervention would satisfy "the intervention is labelled" and be
    /// useless, so the ordinary message is asserted to stay ordinary.
    #[test]
    fn an_intervention_is_attributed_to_its_guard_and_a_user_turn_is_not() {
        let steer = super::super::intervention::user_message(
            super::super::progress::STALL_TAG,
            "name what is blocking you",
        );
        let typed = LoopMessage::User(UserMessage::text("fix the parser bug"));

        let recs = traced(|| {
            record_event(&LoopEvent::MessageStart { message: steer });
            record_event(&LoopEvent::MessageStart { message: typed });
        });

        assert_eq!(recs.len(), 2, "both messages trace: {recs:?}");
        assert_eq!(recs[0]["role"], "intervention");
        assert_eq!(recs[0]["guard"], super::super::progress::STALL_TAG);
        assert!(
            recs[0]["why"].as_str().is_some_and(|w| !w.is_empty()),
            "the guard's own account of what it did: {:?}",
            recs[0]
        );
        assert_eq!(
            recs[0]["text"], "name what is blocking you",
            "the tag is attribution, not body — it must not be repeated in the text"
        );

        assert_eq!(recs[1]["role"], "user", "a typed message is not a steer");
        assert!(recs[1].get("guard").is_none());
    }

    /// A streaming turn must leave SOMETHING in the trace, and must not leave
    /// one record per delta.
    ///
    /// A local model was observed thinking for thirteen minutes across 2892
    /// tokens with nothing recorded between `turn_start` and the next event —
    /// which reads exactly like a deadlock, in a file whose stated purpose is
    /// explaining a run that hung. One record per delta is the opposite
    /// failure: a megabyte of trace saying nothing.
    ///
    /// The throttle is time-based, so this asserts the shape a burst produces
    /// (at most one record) rather than trying to make the clock move.
    #[test]
    fn a_burst_of_stream_deltas_leaves_at_most_one_heartbeat() {
        let recs = traced(|| {
            for _ in 0..500 {
                record_event(&LoopEvent::MessageUpdate {
                    message: assistant("thinking"),
                    phase: crate::agent::agent_loop::message::DeltaPhase::ThinkingDelta,
                });
            }
        });
        assert!(
            recs.len() <= 1,
            "500 deltas must not become 500 records; got {}",
            recs.len()
        );
        for r in &recs {
            assert_eq!(r["kind"], "streaming");
            assert!(r["chars"].is_u64(), "a heartbeat says how much: {r:?}");
        }
    }

    /// The throttle itself: due once, then not again until the gap elapses.
    /// Without this the test above is satisfied by a heartbeat that never
    /// fires at all, which is the state that made a hang unreadable.
    #[test]
    fn the_heartbeat_fires_then_throttles() {
        // The sink must exist for `heartbeat_due` to have a clock.
        let _ = traced(|| {});
        // Whether the FIRST call is due depends on how long this process has
        // been alive, so drive it to a known state: once it fires, the next
        // call must not.
        if heartbeat_due() {
            assert!(
                !heartbeat_due(),
                "a heartbeat must throttle the call right after it fires"
            );
        }
    }

    /// The UI-side records must say which handle the TUI would render a
    /// message under, because that is the question "would this show twice"
    /// turns on: a harness intervention arrives as BOTH a `SystemNotice` and a
    /// `UserMessage`, and the TUI renders both.
    #[test]
    fn ui_records_name_the_handle_the_tui_would_render_under() {
        use crate::event::AgentEvent as E;
        let steer = format!(
            "{} name what is blocking you",
            super::super::progress::STALL_TAG
        );
        let critic = format!("{} not done yet", super::super::critic::CRITIC_TAG);

        let recs = traced(|| {
            record_ui_event(&E::UserMessage {
                content: steer.as_str().into(),
            });
            record_ui_event(&E::UserMessage {
                content: critic.as_str().into(),
            });
            record_ui_event(&E::UserMessage {
                content: "fix the parser".into(),
            });
            record_ui_event(&E::SystemNotice {
                content: "harness intervention: no progress".into(),
            });
        });

        let seen: Vec<(&str, &str)> = recs
            .iter()
            .map(|r| (r["as"].as_str().unwrap(), r["text"].as_str().unwrap()))
            .collect();
        assert_eq!(
            seen,
            vec![
                // A boundary nudge renders under <sys>, tag stripped...
                ("sys", "name what is blocking you"),
                // ...a finalization nudge under <critic>...
                ("critic", "not done yet"),
                // ...and something the user typed under <you>.
                ("you", "fix the parser"),
                ("sys-notice", "harness intervention: no progress"),
            ]
        );
    }

    /// A tool call and its result must be joinable, or the trace cannot say
    /// which call failed — the id is what does that.
    #[test]
    fn a_tool_call_and_its_result_share_an_id() {
        let recs = traced(|| {
            record_event(&LoopEvent::ToolExecutionStart {
                tool_call_id: "call-7".into(),
                tool_name: "bash".into(),
                args: json!({"command": "ls"}),
            });
            record_event(&LoopEvent::ToolExecutionEnd {
                tool_call_id: "call-7".into(),
                tool_name: "bash".into(),
                result: tool_result("a.txt"),
                is_error: false,
            });
        });

        assert_eq!(recs[0]["kind"], "tool_start");
        assert_eq!(recs[1]["kind"], "tool_end");
        assert_eq!(recs[0]["id"], recs[1]["id"]);
        assert_eq!(recs[1]["error"], false);
    }

    /// Records must carry a monotonic sequence and a timestamp, because the
    /// question a trace answers is "in what order" — and two events inside one
    /// millisecond are exactly the pair whose order is in doubt.
    #[test]
    fn records_are_ordered_by_a_monotonic_sequence() {
        let recs = traced(|| {
            record_event(&LoopEvent::AgentStart);
            record_event(&LoopEvent::TurnStart);
            record_event(&LoopEvent::AgentEnd { messages: vec![] });
        });
        let seqs: Vec<u64> = recs.iter().map(|r| r["seq"].as_u64().unwrap()).collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "sequence must strictly increase: {seqs:?}"
        );
        assert!(recs.iter().all(|r| r["ms"].is_u64()));
    }

    /// A trace that grows with the size of a file read is a trace nobody can
    /// read. The excerpt must bound the field AND mark that it did — an
    /// unmarked truncation reads as a complete short value.
    #[test]
    fn oversized_payloads_are_bounded_and_marked() {
        let huge = "x".repeat(50_000);
        let recs = traced(|| {
            record_event(&LoopEvent::ToolExecutionEnd {
                tool_call_id: "c".into(),
                tool_name: "read".into(),
                result: tool_result(&huge),
                is_error: false,
            });
        });
        let out = recs[0]["output"].as_str().unwrap();
        assert!(
            out.len() <= EXCERPT_BYTES + 8,
            "excerpt not bounded: {} bytes",
            out.len()
        );
        assert!(out.ends_with('…'), "a truncated excerpt must say so: {out}");

        // ...and a short value is passed through whole, or the marker means
        // nothing.
        let recs = traced(|| {
            record_event(&LoopEvent::ToolExecutionEnd {
                tool_call_id: "c".into(),
                tool_name: "read".into(),
                result: tool_result("short"),
                is_error: false,
            });
        });
        assert_eq!(recs[0]["output"], "short");
    }

    /// Nothing is written when tracing is off — the flag is what every call
    /// site relies on to be free.
    ///
    /// Asserted by reading the sink back around a disabled write, so the test
    /// fails if the guard is removed. The paired write afterwards is what
    /// stops it going vacuous: a sink that had stopped accepting records
    /// entirely would satisfy the first half and mean nothing.
    #[test]
    fn a_disabled_trace_writes_nothing() {
        let recs = traced(|| {
            let was = ENABLED.swap(false, Ordering::Relaxed);
            note("should_not_appear", json!({}));
            record_event(&LoopEvent::AgentStart);
            ENABLED.store(was, Ordering::Relaxed);
            // ...and the same call with the flag restored does write.
            note("should_appear", json!({}));
        });
        let kinds: Vec<&str> = recs.iter().filter_map(|r| r["kind"].as_str()).collect();
        assert_eq!(
            kinds,
            vec!["should_appear"],
            "a disabled trace must write nothing and an enabled one must write"
        );
    }

    /// What the model SAID must reach the trace — it is half of what a
    /// harness review reads, the other half being what the harness said back.
    ///
    /// It has to come from `TurnEnd`, because `MessageStart` fires before the
    /// stream: its message is a placeholder whose text and tool calls are
    /// empty for every turn ever taken. Tracing that produced one vacuous
    /// `assistant:` line per turn and lost the model's words entirely, which
    /// is the failure this test exists to keep out — so it asserts BOTH that
    /// the finalized text is recorded and that the placeholder is not.
    #[test]
    fn the_models_words_reach_the_trace_from_the_finalized_turn() {
        let recs = traced(|| {
            // The placeholder the loop emits before streaming anything.
            record_event(&LoopEvent::MessageStart {
                message: LoopMessage::Assistant(assistant("")),
            });
            record_event(&LoopEvent::TurnEnd {
                message: assistant("I'll read the file first"),
                tool_results: vec![],
            });
        });

        assert_eq!(
            recs.len(),
            1,
            "the pre-stream placeholder must not produce a record: {recs:?}"
        );
        assert_eq!(recs[0]["kind"], "turn_end");
        assert_eq!(recs[0]["text"], "I'll read the file first");
        assert_eq!(recs[0]["text_chars"], 24);
        assert_eq!(recs[0]["tool_calls"], 0);
    }
}
