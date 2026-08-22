//! Tests for the `dirge vigil` CLI management layer: entry parsing and the
//! vigil config serde shape. Gated on the `vigil` feature because every type
//! under test (VigilEntry, VigilTrigger, VigilAddTrigger) is cfg-gated too.

use crate::cli::VigilAddTrigger;
use crate::config::{SocketMode, VigilEntry, VigilTrigger};

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

#[test]
fn toll_entry_uses_defaults() {
    let entry = crate::build_vigil_entry("poll", &VigilAddTrigger::Toll, &[]).unwrap();
    assert_eq!(entry.name, "poll");
    assert!(matches!(
        entry.trigger,
        VigilTrigger::Toll { interval_secs: 30 }
    ));
    assert_eq!(entry.reap_interval_secs, 30);
    assert!(entry.prompt.is_empty());
    assert!(entry.rite.is_some());
}

#[test]
fn toll_entry_parses_interval_and_reap() {
    let entry = crate::build_vigil_entry(
        "poll",
        &VigilAddTrigger::Toll,
        &args(&["interval_secs=60", "reap_interval_secs=10", "prompt=hi"]),
    )
    .unwrap();
    assert!(matches!(
        entry.trigger,
        VigilTrigger::Toll { interval_secs: 60 }
    ));
    assert_eq!(entry.reap_interval_secs, 10);
    assert_eq!(entry.prompt, "hi");
}

#[test]
fn watcher_entry_parses_path() {
    let entry =
        crate::build_vigil_entry("w", &VigilAddTrigger::Watcher, &args(&["path=/tmp/watch"]))
            .unwrap();
    assert!(matches!(entry.trigger, VigilTrigger::Watcher { path } if path == "/tmp/watch"));
}

#[test]
fn watcher_entry_defaults_path_to_dot() {
    let entry = crate::build_vigil_entry("w", &VigilAddTrigger::Watcher, &[]).unwrap();
    assert!(matches!(entry.trigger, VigilTrigger::Watcher { path } if path == "."));
}

#[test]
fn harbinger_entry_defaults_address_and_commands_mode() {
    let entry = crate::build_vigil_entry("h", &VigilAddTrigger::Harbinger, &[]).unwrap();
    match entry.trigger {
        VigilTrigger::Harbinger {
            address,
            protocol,
            socket_mode,
            commands,
        } => {
            assert_eq!(address, "127.0.0.1:9000");
            assert!(protocol.is_empty());
            assert_eq!(socket_mode, SocketMode::Commands);
            assert!(commands.is_empty());
        }
        other => panic!("expected harbinger, got {other:?}"),
    }
}

#[test]
fn config_deserializes_toll_with_defaults() {
    let entry: VigilEntry =
        serde_json::from_str(r#"{"name":"poll","trigger":{"type":"toll","interval_secs":45}}"#)
            .unwrap();
    assert!(matches!(
        entry.trigger,
        VigilTrigger::Toll { interval_secs: 45 }
    ));
    assert_eq!(entry.reap_interval_secs, 30);
    assert!(entry.prompt.is_empty());
}

#[test]
fn config_deserializes_harbinger_kebab_case() {
    let entry: VigilEntry = serde_json::from_str(
        r#"{"name":"jh","trigger":{"type":"harbinger","address":"127.0.0.1:9001","socket_mode":"commands"}}"#,
    )
    .unwrap();
    match entry.trigger {
        VigilTrigger::Harbinger {
            address,
            protocol,
            socket_mode,
            ..
        } => {
            assert_eq!(address, "127.0.0.1:9001");
            assert!(protocol.is_empty());
            assert_eq!(socket_mode, SocketMode::Commands);
        }
        other => panic!("expected harbinger, got {other:?}"),
    }
}
