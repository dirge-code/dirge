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
fn harbinger_entry_defaults_to_template_mode() {
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
            assert_eq!(socket_mode, SocketMode::Template);
            assert!(commands.is_empty());
        }
        other => panic!("expected harbinger, got {other:?}"),
    }
}

#[test]
fn toll_entry_rejects_zero_interval() {
    let err = crate::build_vigil_entry("poll", &VigilAddTrigger::Toll, &args(&["interval_secs=0"]))
        .unwrap_err();
    assert!(err.to_string().contains("interval_secs"), "{err}");
}

#[test]
fn entry_rejects_zero_reap_interval() {
    let err = crate::build_vigil_entry(
        "poll",
        &VigilAddTrigger::Toll,
        &args(&["reap_interval_secs=0"]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("reap_interval_secs"), "{err}");
}

#[test]
fn entry_rejects_keyless_arg() {
    let err =
        crate::build_vigil_entry("poll", &VigilAddTrigger::Toll, &args(&["bogus"])).unwrap_err();
    assert!(err.to_string().contains("key=value"), "{err}");
}

#[test]
fn entry_rejects_unknown_arg() {
    let err = crate::build_vigil_entry("poll", &VigilAddTrigger::Toll, &args(&["interval_sec=60"]))
        .unwrap_err();
    assert!(err.to_string().contains("unknown vigil arg"), "{err}");
}

#[test]
fn harbinger_rejects_commands_socket_mode() {
    let err = crate::build_vigil_entry(
        "h",
        &VigilAddTrigger::Harbinger,
        &args(&["socket_mode=commands"]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("commands"), "{err}");
}

#[test]
fn list_merges_config_and_store_with_status() {
    use crate::extras::dirge_paths::ProjectPaths;
    use crate::extras::vigil_db::{VigilStatus, VigilStore};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let tmp = std::env::temp_dir().join(format!("dirge-vigil-list-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let paths = ProjectPaths::at(&tmp);

    // Store-only vigil, laid to rest: must appear with its resting status.
    let store = VigilStore::open(&paths).unwrap();
    let store_entry = VigilEntry {
        name: "from-store".to_string(),
        trigger: VigilTrigger::Toll { interval_secs: 30 },
        ..Default::default()
    };
    store
        .upsert("from-store", &serde_json::to_string(&store_entry).unwrap())
        .unwrap();
    store
        .set_status("from-store", VigilStatus::Resting)
        .unwrap();

    // Config vigil: must appear and default to Active.
    let config_entry = VigilEntry {
        name: "from-config".to_string(),
        trigger: VigilTrigger::Watcher {
            path: "/tmp/x".to_string(),
        },
        ..Default::default()
    };

    let vigils = crate::collect_vigils_for_list(&paths, vec![config_entry]);
    let names: Vec<&str> = vigils.iter().map(|(e, _)| e.name.as_str()).collect();
    assert_eq!(names, vec!["from-config", "from-store"]);

    for (entry, status) in &vigils {
        match entry.name.as_str() {
            "from-store" => assert_eq!(*status, VigilStatus::Resting),
            "from-config" => assert_eq!(*status, VigilStatus::Active),
            other => panic!("unexpected vigil {other}"),
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
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
