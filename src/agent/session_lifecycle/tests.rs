//! [`Lifecycle`] against a recording port: what is announced, in which
//! order, and that a slow listener never holds the session.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::collect::McpServersFn;
use super::domain::StartFacts;
use super::*;
use crate::agent::agent_loop::hooks::RunOpening;

/// Writes every event it hears to `log`, shared with whatever else the
/// test records, so their order shows.
#[derive(Default)]
struct RecordingPort {
    log: Arc<Mutex<Vec<String>>>,
    answer: Vec<String>,
    /// How long each call takes.
    delay: Duration,
    deaf: bool,
}

impl SessionLifecycle for RecordingPort {
    fn listens(&self, _hook: LifecycleHook) -> bool {
        !self.deaf
    }

    fn session_start(&self, event: &SessionStart) -> Vec<String> {
        std::thread::sleep(self.delay);
        self.log.lock().unwrap().push(format!(
            "start {} first={} servers={}",
            event.session_id.as_deref().unwrap_or("-"),
            event.first_prompt,
            event.mcp_servers.join(",")
        ));
        self.answer.clone()
    }

    fn session_end(&self, event: &SessionEnd) {
        std::thread::sleep(self.delay);
        self.log.lock().unwrap().push(format!(
            "end {} {}",
            event.session_id.as_deref().unwrap_or("-"),
            event.reason.key()
        ));
    }
}

/// The servers `hive` and how many times they were asked for.
fn hive_server() -> (McpServersFn, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = asked.clone();
    let servers: McpServersFn = Arc::new(move |_wait| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { vec!["hive".to_string()] })
    });
    (servers, asked)
}

fn lifecycle(port: RecordingPort, budgets: Budgets) -> (Lifecycle, Arc<Mutex<Vec<String>>>) {
    let log = port.log.clone();
    let (servers, _) = hive_server();
    let lifecycle = Lifecycle::new(Arc::new(port), Arc::default(), servers, budgets);
    (lifecycle, log)
}

fn answering(text: &str) -> RecordingPort {
    RecordingPort {
        answer: vec![text.to_string()],
        ..RecordingPort::default()
    }
}

fn facts(id: &str, first_prompt: bool) -> StartFacts {
    StartFacts {
        session_id: Some(id.to_string()),
        cwd: "/w".into(),
        first_prompt,
    }
}

fn logged(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().unwrap().clone()
}

#[tokio::test]
async fn a_session_starts_once_per_id_and_again_after_a_fold() {
    let (lifecycle, log) = lifecycle(answering("ctx"), Budgets::default());

    let first = lifecycle.start(facts("s1", true)).await;
    assert!(first.expect("answered").contains("ctx"));
    assert_eq!(
        lifecycle.start(facts("s1", false)).await,
        None,
        "a later prompt"
    );
    assert!(
        lifecycle.start(facts("s2", false)).await.is_some(),
        "a fold gives the session a new id"
    );

    assert_eq!(
        logged(&log),
        vec![
            "start s1 first=true servers=hive",
            "start s2 first=false servers=hive"
        ]
    );
}

#[tokio::test]
async fn the_end_carries_the_id_the_runs_started_under() {
    let (lifecycle, log) = lifecycle(answering("ctx"), Budgets::default());

    lifecycle.start(facts("s1", true)).await;
    lifecycle.start(facts("s1-folded", false)).await;
    lifecycle.end(EndCause::Quit).await;

    assert_eq!(logged(&log)[2..], ["end s1-folded exit"]);
}

#[tokio::test]
async fn clear_and_switch_end_with_swap_then_the_session_starts_again() {
    let (lifecycle, log) = lifecycle(answering("ctx"), Budgets::default());

    lifecycle.start(facts("s1", true)).await;
    lifecycle.end(EndCause::Clear).await;
    lifecycle.start(facts("s1", true)).await;
    lifecycle.end(EndCause::Switch).await;
    lifecycle.end(EndCause::Switch).await;
    lifecycle.end(EndCause::Quit).await;

    assert_eq!(
        logged(&log),
        vec![
            "start s1 first=true servers=hive",
            "end s1 swap",
            "start s1 first=true servers=hive",
            "end s1 swap"
        ]
    );
}

#[tokio::test]
async fn the_session_ends_before_the_teardown_closes_the_mcp_servers() {
    let port = RecordingPort {
        delay: Duration::from_millis(50),
        ..answering("ctx")
    };
    let (lifecycle, log) = lifecycle(port, Budgets::default());
    lifecycle.start(facts("s1", true)).await;

    let teardown_log = log.clone();
    let closed = lifecycle
        .end_then(EndCause::Quit, async move {
            teardown_log.lock().unwrap().push("mcp closed".into());
            "closed"
        })
        .await;

    assert_eq!(closed, "closed");
    assert_eq!(logged(&log)[1..], ["end s1 exit", "mcp closed"]);
}

#[tokio::test]
async fn listeners_that_outlast_their_budget_do_not_hold_the_session() {
    let port = RecordingPort {
        delay: Duration::from_secs(2),
        ..answering("too late")
    };
    let budgets = Budgets {
        mcp_wait: Duration::ZERO,
        start: Duration::from_millis(50),
        end: Duration::from_millis(50),
    };
    let (lifecycle, log) = lifecycle(port, budgets);
    let started = Instant::now();

    assert_eq!(lifecycle.start(facts("s1", true)).await, None);
    let teardown_log = log.clone();
    lifecycle
        .end_then(EndCause::Quit, async move {
            teardown_log.lock().unwrap().push("mcp closed".into());
        })
        .await;

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(logged(&log), vec!["mcp closed"]);
}

#[tokio::test]
async fn without_listeners_neither_the_servers_nor_the_port_are_asked() {
    let (servers, asked) = hive_server();
    let port = RecordingPort {
        deaf: true,
        ..answering("ctx")
    };
    let log = port.log.clone();
    let lifecycle = Lifecycle::new(Arc::new(port), Arc::default(), servers, Budgets::default());

    assert_eq!(lifecycle.start(facts("s1", true)).await, None);
    lifecycle.end(EndCause::Quit).await;

    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert!(logged(&log).is_empty());
}

#[tokio::test]
async fn the_start_reminder_leads_the_first_turn_and_leaves_the_system_prompt() {
    let (lifecycle, _) = lifecycle(answering("open work: 3 cards"), Budgets::default());
    let open = lifecycle.open_run(facts("s1", true));
    let opening = RunOpening {
        system_prompt: "sys".into(),
        prompt: "hi".into(),
        reminders: Vec::new(),
        refusal: None,
    };

    let first = open(opening.clone()).await;
    assert_eq!(first.system_prompt, "sys");
    assert_eq!(first.prompt, "hi");
    assert_eq!(first.reminders.len(), 1);
    assert!(first.reminders[0].contains("open work: 3 cards"));

    assert_eq!(open(opening.clone()).await, opening, "once per session id");
}
