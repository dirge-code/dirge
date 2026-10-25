//! External loop directives: messages a producer outside the agent pushes
//! INTO the running agent loop, each with a mode that says how the loop
//! must take it.
//!
//! This is the push counterpart of an MCP result piggyback. A piggyback
//! reaches the model only inside the result of the next tool call it
//! happens to make, as text the model may ignore; a directive changes the
//! loop's control flow:
//!
//! - [`LoopMode::Steer`] joins the steering poll, so it is injected before
//!   the next model call of the running turn (between tool rounds);
//! - [`LoopMode::Interject`] ends the running turn at its next boundary
//!   (the loop's graceful interjection) and opens the next run;
//! - [`LoopMode::FollowUp`] joins the finalization follow-ups, so a run
//!   that was about to finish continues with it.
//!
//! Whatever is still queued when no run is active opens a new run: the TUI
//! awaits [`LoopInbox::ready`] while idle and starts a turn on
//! [`LoopInbox::take_for_new_run`]. Every directive handed to the loop is
//! reported once through the [`InjectionAck`] port, so the producer can
//! stop holding it (the panel feed answers `{"action":"ack"}`).
//!
//! The first producer is the panel feed's `loop/*` ops (hive senses: a
//! worker asks, is blocked, failed, completed); nothing here knows about
//! hive. Only the main session's runs attach an inbox — subagents never
//! see these directives.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Notify;

use super::hooks::{GetFollowupMessagesFn, GetSteeringMessagesFn};
use super::message::{LoopMessage, UserMessage};
use super::tool::AbortSignal;
use crate::sync_util::LockExt;

/// How the loop takes a directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    Steer,
    Interject,
    FollowUp,
}

impl LoopMode {
    /// The mode named by a wire op suffix (`steer`, `interject`,
    /// `followup` / `follow-up`).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "steer" => Some(Self::Steer),
            "interject" => Some(Self::Interject),
            "followup" | "follow-up" | "follow_up" => Some(Self::FollowUp),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Interject => "interject",
            Self::FollowUp => "followup",
        }
    }
}

/// One directive: `prompt` is exactly what the model reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopDirective {
    /// Producer id, echoed in the acknowledgement.
    pub id: String,
    pub mode: LoopMode,
    pub prompt: String,
}

/// Preamble of a directive injected mid-turn, so the model weighs it
/// against the task in hand instead of dropping that task.
pub const EXTERNAL_STEER_WRAPPER: &str = "[External event delivered mid-turn. It did not come from the user. Weigh it against the current task: act on it now if it is urgent or blocks others, otherwise finish the current step first.]";

/// Largest number of directives held; past it the oldest is dropped (and
/// never acknowledged, so the producer sends it again later).
pub const MAX_QUEUED: usize = 128;

/// Told which directives reached the loop. Production: the panel feed,
/// which acknowledges them to the producer.
pub trait InjectionAck: Send + Sync {
    fn injected(&self, ids: &[String]);
}

/// The pure queue: directives in arrival order, taken by mode.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Queue {
    items: VecDeque<LoopDirective>,
}

impl Queue {
    /// Add `d`. A directive whose id is already queued replaces it in
    /// place (a producer resends what it has not seen acknowledged).
    /// Returns the directive dropped to stay within `cap`, if any.
    pub fn push(&mut self, d: LoopDirective, cap: usize) -> Option<LoopDirective> {
        if let Some(slot) = self.items.iter_mut().find(|q| q.id == d.id) {
            *slot = d;
            return None;
        }
        self.items.push_back(d);
        if self.items.len() > cap.max(1) {
            self.items.pop_front()
        } else {
            None
        }
    }

    /// Remove and return every directive of `mode`, in order.
    pub fn take_mode(&mut self, mode: LoopMode) -> Vec<LoopDirective> {
        let (taken, kept): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.items)
            .into_iter()
            .partition(|d| d.mode == mode);
        self.items = kept;
        taken.into()
    }

    /// Remove and return everything.
    pub fn take_all(&mut self) -> Vec<LoopDirective> {
        std::mem::take(&mut self.items).into()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// A directive as the mid-turn user message the loop injects.
pub fn steer_message(d: &LoopDirective) -> LoopMessage {
    LoopMessage::User(UserMessage::text(format!(
        "{EXTERNAL_STEER_WRAPPER}\n{}",
        d.prompt
    )))
}

/// A directive as a finalization follow-up.
pub fn followup_message(d: &LoopDirective) -> LoopMessage {
    LoopMessage::User(UserMessage::text(d.prompt.clone()))
}

/// The opening prompt of a run started for `ds` (none: `None`).
pub fn new_run_prompt(ds: &[LoopDirective]) -> Option<String> {
    if ds.is_empty() {
        return None;
    }
    Some(
        ds.iter()
            .map(|d| d.prompt.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

struct Attached {
    generation: u64,
    signal: AbortSignal,
}

/// The inbox: the queue plus the run it may interrupt, the idle wake-up
/// and the acknowledgement port.
pub struct LoopInbox {
    queue: Mutex<Queue>,
    run: Mutex<Option<Attached>>,
    generation: AtomicU64,
    notify: Notify,
    ack: Mutex<Option<Arc<dyn InjectionAck>>>,
}

impl Default for LoopInbox {
    fn default() -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            run: Mutex::new(None),
            generation: AtomicU64::new(0),
            notify: Notify::new(),
            ack: Mutex::new(None),
        }
    }
}

impl std::fmt::Debug for LoopInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopInbox")
            .field("queued", &self.queue.lock_ignore_poison().len())
            .finish()
    }
}

/// Detaches its run from the inbox when dropped (the run ended or its task
/// was aborted), so an interjection never reaches a finished run's signal.
pub struct RunGuard {
    inbox: Arc<LoopInbox>,
    generation: u64,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let mut run = self.inbox.run.lock_ignore_poison();
        if run
            .as_ref()
            .is_some_and(|a| a.generation == self.generation)
        {
            *run = None;
        }
        drop(run);
        // Whatever the run left queued may now open the next one.
        if !self.inbox.queue.lock_ignore_poison().is_empty() {
            self.inbox.notify.notify_one();
        }
    }
}

static GLOBAL: OnceLock<Arc<LoopInbox>> = OnceLock::new();

/// The process inbox, armed by a producer (see [`arm`]). `None` until then,
/// so a dirge without a producer composes nothing into its loop.
pub fn installed() -> Option<Arc<LoopInbox>> {
    GLOBAL.get().cloned()
}

/// The process inbox, created on first use. A producer calls this.
pub fn arm() -> Arc<LoopInbox> {
    GLOBAL
        .get_or_init(|| Arc::new(LoopInbox::default()))
        .clone()
}

impl LoopInbox {
    /// Set where acknowledgements go.
    pub fn set_ack(&self, ack: Arc<dyn InjectionAck>) {
        *self.ack.lock_ignore_poison() = Some(ack);
    }

    fn acknowledge(&self, ds: &[LoopDirective]) {
        if ds.is_empty() {
            return;
        }
        let ack = self.ack.lock_ignore_poison().clone();
        if let Some(ack) = ack {
            let ids: Vec<String> = ds.iter().map(|d| d.id.clone()).collect();
            ack.injected(&ids);
        }
    }

    /// Accept `d`. An interjection gracefully interrupts the attached run.
    /// Always wakes an idle waiter.
    pub fn push(&self, d: LoopDirective) {
        let interject = d.mode == LoopMode::Interject;
        let dropped = self.queue.lock_ignore_poison().push(d, MAX_QUEUED);
        if let Some(old) = dropped {
            tracing::warn!(target: "dirge::loop_inbox", id = %old.id, "loop inbox full; dropped the oldest directive");
        }
        if interject && let Some(run) = self.run.lock_ignore_poison().as_ref() {
            run.signal.interject();
        }
        self.notify.notify_one();
    }

    /// Attach the run behind `signal`; the guard detaches it.
    pub fn attach_run(self: &Arc<Self>, signal: AbortSignal) -> RunGuard {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let pending_interject = self
            .queue
            .lock_ignore_poison()
            .items
            .iter()
            .any(|d| d.mode == LoopMode::Interject);
        *self.run.lock_ignore_poison() = Some(Attached {
            generation,
            signal: signal.clone(),
        });
        // An interjection that arrived between two runs still ends the
        // next one at its first boundary; the run opened for it (if any)
        // took it already, so this only fires for leftovers.
        if pending_interject {
            signal.interject();
        }
        RunGuard {
            inbox: self.clone(),
            generation,
        }
    }

    /// True while a run is attached.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn run_attached(&self) -> bool {
        self.run.lock_ignore_poison().is_some()
    }

    /// Take the steering directives (acknowledged).
    pub fn take_steering(&self) -> Vec<LoopDirective> {
        let ds = self.queue.lock_ignore_poison().take_mode(LoopMode::Steer);
        self.acknowledge(&ds);
        ds
    }

    /// Take the follow-up directives (acknowledged).
    pub fn take_followups(&self) -> Vec<LoopDirective> {
        let ds = self
            .queue
            .lock_ignore_poison()
            .take_mode(LoopMode::FollowUp);
        self.acknowledge(&ds);
        ds
    }

    /// Take everything queued as the opening prompt of a new run
    /// (acknowledged), or `None` when empty.
    pub fn take_for_new_run(&self) -> Option<String> {
        let ds = self.queue.lock_ignore_poison().take_all();
        self.acknowledge(&ds);
        new_run_prompt(&ds)
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock_ignore_poison().is_empty()
    }

    /// Resolve once something is queued (at once if it already is).
    pub async fn ready(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_empty() {
                return;
            }
            notified.await;
        }
    }

    /// The loop's steering hook over this inbox.
    pub fn steering_hook(self: &Arc<Self>) -> GetSteeringMessagesFn {
        let inbox = self.clone();
        Arc::new(move || {
            let msgs: Vec<LoopMessage> = inbox.take_steering().iter().map(steer_message).collect();
            Box::pin(async move { msgs })
        })
    }

    /// The loop's follow-up hook over this inbox.
    pub fn followup_hook(self: &Arc<Self>) -> GetFollowupMessagesFn {
        let inbox = self.clone();
        Arc::new(move || {
            let msgs: Vec<LoopMessage> = inbox
                .take_followups()
                .iter()
                .map(followup_message)
                .collect();
            Box::pin(async move { msgs })
        })
    }
}

/// `a` then `b`, both polled at every boundary, results concatenated.
pub fn chain_steering(
    a: Option<GetSteeringMessagesFn>,
    b: GetSteeringMessagesFn,
) -> GetSteeringMessagesFn {
    match a {
        None => b,
        Some(a) => Arc::new(move || {
            let (a, b) = (a.clone(), b.clone());
            Box::pin(async move {
                let mut out = a().await;
                out.extend(b().await);
                out
            })
        }),
    }
}

/// `a` then `b` for follow-ups (same contract as [`chain_steering`]).
pub fn chain_followups(
    a: Option<GetFollowupMessagesFn>,
    b: GetFollowupMessagesFn,
) -> GetFollowupMessagesFn {
    match a {
        None => b,
        Some(a) => Arc::new(move || {
            let (a, b) = (a.clone(), b.clone());
            Box::pin(async move {
                let mut out = a().await;
                out.extend(b().await);
                out
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: &str, mode: LoopMode) -> LoopDirective {
        LoopDirective {
            id: id.into(),
            mode,
            prompt: format!("p-{id}"),
        }
    }

    fn text(m: &LoopMessage) -> String {
        match m {
            LoopMessage::User(u) => u.text_joined(),
            _ => panic!("expected a user message"),
        }
    }

    #[derive(Default)]
    struct RecordingAck(Mutex<Vec<String>>);

    impl InjectionAck for RecordingAck {
        fn injected(&self, ids: &[String]) {
            self.0.lock().unwrap().extend(ids.iter().cloned());
        }
    }

    #[test]
    fn mode_names_round_trip() {
        for m in [LoopMode::Steer, LoopMode::Interject, LoopMode::FollowUp] {
            assert_eq!(LoopMode::from_name(m.name()), Some(m));
        }
        assert_eq!(LoopMode::from_name("follow-up"), Some(LoopMode::FollowUp));
        assert_eq!(LoopMode::from_name("nope"), None);
    }

    #[test]
    fn queue_takes_by_mode_dedups_and_is_bounded() {
        let mut q = Queue::default();
        q.push(d("a", LoopMode::Steer), 8);
        q.push(d("b", LoopMode::FollowUp), 8);
        q.push(d("c", LoopMode::Steer), 8);
        q.push(
            LoopDirective {
                prompt: "resent".into(),
                ..d("a", LoopMode::Steer)
            },
            8,
        );
        assert_eq!(q.len(), 3, "a resent id replaces, never duplicates");
        let steer = q.take_mode(LoopMode::Steer);
        assert_eq!(
            steer.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(steer[0].prompt, "resent");
        assert_eq!(q.take_all(), vec![d("b", LoopMode::FollowUp)]);
        let mut small = Queue::default();
        assert_eq!(small.push(d("1", LoopMode::Steer), 2), None);
        assert_eq!(small.push(d("2", LoopMode::Steer), 2), None);
        assert_eq!(
            small.push(d("3", LoopMode::Steer), 2),
            Some(d("1", LoopMode::Steer))
        );
    }

    #[tokio::test]
    async fn hooks_inject_by_mode_and_acknowledge() {
        let inbox = Arc::new(LoopInbox::default());
        let ack = Arc::new(RecordingAck::default());
        inbox.set_ack(ack.clone());
        inbox.push(d("s", LoopMode::Steer));
        inbox.push(d("f", LoopMode::FollowUp));
        let steer = inbox.steering_hook()().await;
        assert_eq!(steer.len(), 1);
        assert!(text(&steer[0]).starts_with(EXTERNAL_STEER_WRAPPER));
        assert!(text(&steer[0]).ends_with("p-s"));
        assert!(inbox.steering_hook()().await.is_empty(), "consumed once");
        let follow = inbox.followup_hook()().await;
        assert_eq!(text(&follow[0]), "p-f");
        assert_eq!(*ack.0.lock().unwrap(), ["s", "f"]);
    }

    #[tokio::test]
    async fn interject_reaches_only_the_attached_run() {
        let inbox = Arc::new(LoopInbox::default());
        let signal = AbortSignal::new();
        let guard = inbox.attach_run(signal.clone());
        assert!(inbox.run_attached());
        inbox.push(d("s", LoopMode::Steer));
        assert!(!signal.is_interjected(), "a steer never interrupts");
        inbox.push(d("i", LoopMode::Interject));
        assert!(signal.is_interjected());
        drop(guard);
        assert!(!inbox.run_attached());
        let later = AbortSignal::new();
        let _g = inbox.attach_run(later.clone());
        assert!(
            later.is_interjected(),
            "a queued interjection still ends the next run"
        );
        assert_eq!(inbox.take_for_new_run().as_deref(), Some("p-s\n\np-i"));
        assert!(inbox.is_empty());
    }

    #[tokio::test]
    async fn ready_wakes_on_push_and_when_a_run_leaves_work() {
        let inbox = Arc::new(LoopInbox::default());
        let waiter = {
            let inbox = inbox.clone();
            tokio::spawn(async move { inbox.ready().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        inbox.push(d("f", LoopMode::FollowUp));
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("woken")
            .unwrap();
        // Already non-empty: resolves at once.
        tokio::time::timeout(std::time::Duration::from_millis(200), inbox.ready())
            .await
            .expect("immediate");
    }

    #[tokio::test]
    async fn chains_poll_both_in_order() {
        let a: GetSteeringMessagesFn = Arc::new(|| {
            Box::pin(async { vec![LoopMessage::User(UserMessage::text("a".to_string()))] })
        });
        let b: GetSteeringMessagesFn = Arc::new(|| {
            Box::pin(async { vec![LoopMessage::User(UserMessage::text("b".to_string()))] })
        });
        let both = chain_steering(Some(a), b.clone());
        let got: Vec<String> = both().await.iter().map(text).collect();
        assert_eq!(got, ["a", "b"]);
        assert_eq!(chain_steering(None, b)().await.len(), 1);
    }
}
