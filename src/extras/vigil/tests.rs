//! End-to-end tests for the vigil runtime, exercised without the TUI or agent.
//!
//! Each test builds a real `VigilKeeper`, drives a trigger (toll, watcher, or
//! harbinger), and asserts that an observance flows through the reaper with the
//! expected prompt substitution, rite gating, coalescing, procession chaining,
//! and pause/resume behavior. This is the scripted replacement for the manual
//! two-terminal workflow.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use crate::config::{SocketMode, VigilCommand, VigilEntry, VigilRite, VigilTrigger};

use super::VigilKeeper;
use super::reaper::Observance;
use super::types::{HookDispatchRequest, TriggerKind, VigilCtl, VigilEvent};

fn toll_entry(
    name: &str,
    interval_secs: u64,
    reap_interval_secs: u64,
    prompt: &str,
    rite: Option<VigilRite>,
) -> VigilEntry {
    VigilEntry {
        name: name.to_string(),
        trigger: VigilTrigger::Toll { interval_secs },
        reap_interval_secs,
        prompt: prompt.to_string(),
        rite,
        ..Default::default()
    }
}

fn ok_rite() -> Option<VigilRite> {
    Some(VigilRite {
        cmd: Some("echo ok".to_string()),
        ..Default::default()
    })
}

fn spawn_keeper(entries: Vec<VigilEntry>) -> VigilKeeper {
    VigilKeeper::from_entries(entries, std::collections::HashSet::new())
        .expect("vigil keeper should build")
}

async fn recv_observance_from(
    keeper: &mut VigilKeeper,
    name: &str,
    timeout: Duration,
) -> Option<Observance> {
    let rx = keeper.observance_rx.as_mut().expect("observance receiver");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        match tokio::time::timeout(deadline - now, rx.recv()).await {
            Ok(Some(obs)) if obs.vigil_name == name => return Some(obs),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

async fn recv_hook(
    keeper: &mut VigilKeeper,
    hook_name: &str,
    timeout: Duration,
) -> Option<HookDispatchRequest> {
    let rx = keeper.hook_rx.as_mut().expect("hook receiver");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        match tokio::time::timeout(deadline - now, rx.recv()).await {
            Ok(Some(req)) if req.hook_name == hook_name => return Some(req),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

fn send_line(port: u16, line: &str) {
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to harbinger");
    stream
        .write_all(format!("{line}\n").as_bytes())
        .expect("write to harbinger");
    stream.flush().expect("flush harbinger");
}

struct CleanupDir(std::path::PathBuf);

impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn toll_trigger_fires_observance_with_rite_output() {
    let mut keeper = spawn_keeper(vec![toll_entry(
        "toll-a",
        1,
        1,
        "rite: {rite_output} count: {event_count}",
        ok_rite(),
    )]);

    let obs = recv_observance_from(&mut keeper, "toll-a", Duration::from_secs(8))
        .await
        .expect("toll observance");

    assert_eq!(obs.vigil_name, "toll-a");
    assert!(obs.event_count >= 1);
    assert!(obs.prompt.contains("rite: ok"), "prompt = {}", obs.prompt);
    assert!(
        obs.prompt.contains(&format!("count: {}", obs.event_count)),
        "prompt = {}",
        obs.prompt
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rite_failure_blocks_observance() {
    let failing = Some(VigilRite {
        cmd: Some("false".to_string()),
        ..Default::default()
    });
    let mut keeper = spawn_keeper(vec![toll_entry("rite-fail", 1, 1, "x", failing)]);

    assert!(
        recv_observance_from(&mut keeper, "rite-fail", Duration::from_secs(4))
            .await
            .is_none(),
        "a failing rite must block the observance"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reaper_coalesces_multiple_events_into_one_observance() {
    let mut keeper = spawn_keeper(vec![toll_entry("coalesce", 3600, 3, "{event_count}", None)]);

    let tx = keeper.vigils[0].tx.clone();
    for i in 0..3 {
        tx.send(VigilEvent {
            vigil_name: "coalesce".to_string(),
            trigger: TriggerKind::Toll,
            context: serde_json::json!({ "kind": "toll", "n": i }),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("send vigil event");
    }

    let obs = recv_observance_from(&mut keeper, "coalesce", Duration::from_secs(8))
        .await
        .expect("coalesced observance");

    assert_eq!(obs.event_count, 3);
    assert_eq!(obs.prompt, "3");
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_fires_on_file_create() {
    let dir = std::env::temp_dir().join(format!("dirge-vigil-watcher-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create watch dir");
    let _cleanup = CleanupDir(dir.clone());

    let mut keeper = spawn_keeper(vec![VigilEntry {
        name: "watch-a".to_string(),
        trigger: VigilTrigger::Watcher {
            path: dir.display().to_string(),
        },
        reap_interval_secs: 1,
        prompt: "files: {files}".to_string(),
        rite: None,
        ..Default::default()
    }]);

    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(dir.join("trigger.txt"), "x").expect("write trigger file");

    let obs = recv_observance_from(&mut keeper, "watch-a", Duration::from_secs(8))
        .await
        .expect("watcher observance");

    assert_eq!(obs.vigil_name, "watch-a");
    assert!(
        obs.prompt.contains("trigger.txt"),
        "prompt = {}",
        obs.prompt
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn harbinger_template_emits_raw_payload() {
    let mut keeper = spawn_keeper(vec![VigilEntry {
        name: "harb-template".to_string(),
        trigger: VigilTrigger::Harbinger {
            address: "127.0.0.1:19190".to_string(),
            protocol: "tcp".to_string(),
            socket_mode: SocketMode::Template,
            commands: HashMap::new(),
        },
        reap_interval_secs: 1,
        prompt: "data: {harbinger_data}".to_string(),
        rite: None,
        ..Default::default()
    }]);

    send_line(19190, r#"{"message":"hello-template"}"#);

    let obs = recv_observance_from(&mut keeper, "harb-template", Duration::from_secs(8))
        .await
        .expect("harbinger template observance");

    assert_eq!(obs.vigil_name, "harb-template");
    assert!(
        obs.prompt.contains("hello-template"),
        "prompt = {}",
        obs.prompt
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn harbinger_commands_resolves_registered_tool() {
    let mut commands = HashMap::new();
    let mut ping_args = serde_json::Map::new();
    ping_args.insert(
        "command".to_string(),
        serde_json::Value::String("echo {message}".to_string()),
    );
    commands.insert(
        "ping".to_string(),
        VigilCommand {
            tool: "bash".to_string(),
            args: ping_args,
        },
    );

    let mut keeper = spawn_keeper(vec![VigilEntry {
        name: "harb-cmd".to_string(),
        trigger: VigilTrigger::Harbinger {
            address: "127.0.0.1:19191".to_string(),
            protocol: "tcp".to_string(),
            socket_mode: SocketMode::Commands,
            commands,
        },
        reap_interval_secs: 1,
        prompt: String::new(),
        rite: None,
        ..Default::default()
    }]);

    send_line(19191, r#"{"command":"ping","args":{"message":"hello"}}"#);

    let obs = recv_observance_from(&mut keeper, "harb-cmd", Duration::from_secs(8))
        .await
        .expect("harbinger commands observance");

    assert_eq!(obs.vigil_name, "harb-cmd");
    assert_eq!(obs.context["_resolved_tool"], "bash");
    assert_eq!(obs.context["_resolved_args"]["command"], "echo hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn procession_chains_to_next_vigil() {
    let a = VigilEntry {
        name: "chain-a".to_string(),
        trigger: VigilTrigger::Toll { interval_secs: 1 },
        reap_interval_secs: 1,
        prompt: "a".to_string(),
        procession: Some("chain-b".to_string()),
        rite: None,
    };
    let b = VigilEntry {
        name: "chain-b".to_string(),
        trigger: VigilTrigger::Toll {
            interval_secs: 3600,
        },
        reap_interval_secs: 1,
        prompt: "b: {event_count}".to_string(),
        rite: None,
        ..Default::default()
    };

    let mut keeper = spawn_keeper(vec![a, b]);

    let first = recv_observance_from(&mut keeper, "chain-a", Duration::from_secs(8))
        .await
        .expect("chain-a observance");
    assert_eq!(first.vigil_name, "chain-a");

    let second = recv_observance_from(&mut keeper, "chain-b", Duration::from_secs(8))
        .await
        .expect("chain-b observance via procession");
    assert_eq!(second.vigil_name, "chain-b");
    assert_eq!(second.context["procession_from"], "chain-a");
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_blocks_and_resume_allows_observance() {
    let mut keeper = spawn_keeper(vec![toll_entry("pause-a", 1, 1, "x", None)]);

    keeper
        .ctl_tx
        .as_ref()
        .expect("ctl sender")
        .send(VigilCtl::Pause {
            name: "pause-a".to_string(),
        })
        .await
        .expect("send pause");
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        recv_observance_from(&mut keeper, "pause-a", Duration::from_secs(3))
            .await
            .is_none(),
        "paused vigil must not observe"
    );

    keeper
        .ctl_tx
        .as_ref()
        .expect("ctl sender")
        .send(VigilCtl::Resume {
            name: "pause-a".to_string(),
        })
        .await
        .expect("send resume");

    let obs = recv_observance_from(&mut keeper, "pause-a", Duration::from_secs(6))
        .await
        .expect("resumed observance");
    assert_eq!(obs.vigil_name, "pause-a");
}

#[tokio::test(flavor = "multi_thread")]
async fn unresolved_placeholder_skips_observance() {
    let mut keeper = spawn_keeper(vec![toll_entry("unresolved", 1, 1, "Job: {job}", None)]);

    assert!(
        recv_observance_from(&mut keeper, "unresolved", Duration::from_secs(4))
            .await
            .is_none(),
        "an unresolved template placeholder must skip the observance"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn toll_dispatches_event_hook() {
    let mut keeper = spawn_keeper(vec![toll_entry("hook-a", 1, 1, "x", None)]);

    let req = recv_hook(&mut keeper, "on-vigil-event", Duration::from_secs(4))
        .await
        .expect("on-vigil-event hook");

    assert_eq!(req.hook_name, "on-vigil-event");
    assert!(req.context.contains("hook-a"), "context = {}", req.context);
}

#[tokio::test(flavor = "multi_thread")]
async fn rite_empty_stdout_yields_empty_rite_output() {
    let silent = Some(VigilRite {
        cmd: Some("true".to_string()),
        ..Default::default()
    });
    let mut keeper = spawn_keeper(vec![toll_entry(
        "rite-empty",
        1,
        1,
        "out:[{rite_output}]",
        silent,
    )]);

    let obs = recv_observance_from(&mut keeper, "rite-empty", Duration::from_secs(8))
        .await
        .expect("rite-empty observance");

    assert_eq!(obs.prompt, "out:[]");
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_fires_on_file_modify() {
    let dir = std::env::temp_dir().join(format!("dirge-vigil-watchmod-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create watch dir");
    let _cleanup = CleanupDir(dir.clone());

    let mut keeper = spawn_keeper(vec![VigilEntry {
        name: "watch-mod".to_string(),
        trigger: VigilTrigger::Watcher {
            path: dir.display().to_string(),
        },
        reap_interval_secs: 1,
        prompt: "files: {files}".to_string(),
        rite: None,
        ..Default::default()
    }]);

    tokio::time::sleep(Duration::from_millis(300)).await;
    let file = dir.join("mod.txt");
    std::fs::write(&file, "a").expect("create file");
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&file, "b").expect("modify file");

    let obs = recv_observance_from(&mut keeper, "watch-mod", Duration::from_secs(8))
        .await
        .expect("watcher modify observance");

    assert!(obs.prompt.contains("mod.txt"), "prompt = {}", obs.prompt);
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_coalesces_multiple_files() {
    let dir = std::env::temp_dir().join(format!("dirge-vigil-watchmulti-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create watch dir");
    let _cleanup = CleanupDir(dir.clone());

    let mut keeper = spawn_keeper(vec![VigilEntry {
        name: "watch-multi".to_string(),
        trigger: VigilTrigger::Watcher {
            path: dir.display().to_string(),
        },
        reap_interval_secs: 1,
        prompt: "files: {files}".to_string(),
        rite: None,
        ..Default::default()
    }]);

    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(dir.join("a.txt"), "1").expect("write a");
    std::fs::write(dir.join("b.txt"), "2").expect("write b");

    let obs = recv_observance_from(&mut keeper, "watch-multi", Duration::from_secs(8))
        .await
        .expect("watcher multi-file observance");

    assert!(obs.prompt.contains("a.txt"), "prompt = {}", obs.prompt);
    assert!(obs.prompt.contains("b.txt"), "prompt = {}", obs.prompt);
}

#[tokio::test(flavor = "multi_thread")]
async fn reaper_dispatches_on_vigil_reap_hook() {
    let mut keeper = spawn_keeper(vec![toll_entry("reap-hook", 1, 1, "x", None)]);

    let req = recv_hook(&mut keeper, "on-vigil-reap", Duration::from_secs(8))
        .await
        .expect("on-vigil-reap hook");

    assert_eq!(req.hook_name, "on-vigil-reap");
    assert!(
        req.context.contains("reap-hook"),
        "context = {}",
        req.context
    );
    assert!(
        req.context.contains(":trigger :toll"),
        "context = {}",
        req.context
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn in_flight_observance_skips_overlapping_reap() {
    let mut keeper = spawn_keeper(vec![toll_entry("inflight", 1, 1, "x", None)]);

    let first = recv_observance_from(&mut keeper, "inflight", Duration::from_secs(8))
        .await
        .expect("first inflight observance");
    assert_eq!(first.vigil_name, "inflight");

    // No UI loop clears the running flag in this harness, so the in-flight guard
    // stays set and subsequent reaps are skipped.
    assert!(
        recv_observance_from(&mut keeper, "inflight", Duration::from_secs(3))
            .await
            .is_none(),
        "second reap must be skipped while observance is in flight"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn toll_prompt_substitutes_name_and_events() {
    let mut keeper = spawn_keeper(vec![toll_entry(
        "pv",
        3600,
        2,
        "name={name} events={events}",
        None,
    )]);

    let tx = keeper.vigils[0].tx.clone();
    tx.send(VigilEvent {
        vigil_name: "pv".to_string(),
        trigger: TriggerKind::Toll,
        context: serde_json::json!({ "kind": "toll" }),
        timestamp: chrono::Utc::now(),
    })
    .await
    .expect("send vigil event");

    let obs = recv_observance_from(&mut keeper, "pv", Duration::from_secs(8))
        .await
        .expect("prompt vars observance");

    assert_eq!(obs.prompt, "name=pv events=toll");
}
