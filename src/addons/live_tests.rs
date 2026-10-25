//! Acceptance tests for changing addons while they run: `refresh!`, the
//! REPL, open hook keys and the `:dirge/event` stream, against the `live`
//! fixture addon on a real isolate.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::cljrs::isolate::IsolateOptions;
use super::discovery;
use super::domain::{AddonPlan, ReloadReport};
use super::host::AddonHost;
use super::port::{Harness, HarnessSink, Level};

#[derive(Default)]
struct QuietSink(Mutex<Vec<String>>);

impl HarnessSink for QuietSink {
    fn notify(&self, _level: Level, message: &str) {
        self.0.lock().unwrap().push(message.to_string());
    }
}

const PROTOCOL: &str = "fixture.addon-protocol";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/addons")
}

fn live_plan(addons: &Path) -> AddonPlan {
    discovery::plan(&[addons.to_path_buf()], &[fixtures().join("protocol/src")])
}

fn live_host(options: IsolateOptions) -> AddonHost {
    let host = super::start_with(
        live_plan(&fixtures().join("live")),
        Harness::with_sink(Arc::new(QuietSink::default())),
        PROTOCOL,
        options,
    )
    .expect("host starts");
    assert!(host.failures().is_empty(), "{:?}", host.failures());
    host
}

fn tool_text(host: &AddonHost, name: &str, args: Value) -> String {
    let tool = host
        .tools()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no tool {name}"));
    let (content, _) = host.call_tool(&tool, &args).expect("tool runs");
    content[0]["text"].as_str().unwrap_or_default().to_string()
}

fn tool_names(host: &AddonHost) -> Vec<String> {
    host.tools().into_iter().map(|t| t.name).collect()
}

/// The next in-place change the host takes in, waiting up to five seconds:
/// the isolate re-reads the addons just after the call that asked returns.
fn next_sync(host: &AddonHost) -> ReloadReport {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(report) = host.sync() {
            return report;
        }
        assert!(Instant::now() < deadline, "no refresh within 5s");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_tool_that_asks_for_a_refresh_offers_its_new_tool_without_a_reload() {
    let host = live_host(IsolateOptions::default());
    assert_eq!(tool_names(&host), vec!["grow", "heard"]);
    assert!(host.sync().is_none(), "nothing re-read yet");

    assert_eq!(
        tool_text(&host, "grow", json!({"name": "wave"})),
        "grew wave"
    );
    let report = next_sync(&host);

    assert_eq!(report.tools_added, vec!["wave".to_string()]);
    assert!(report.tools_removed.is_empty());
    assert_eq!(tool_names(&host), vec!["grow", "heard", "wave"]);
    assert_eq!(tool_text(&host, "wave", json!({})), "hello from wave");
}

#[test]
fn open_hook_keys_reach_addons_by_name() {
    let host = live_host(IsolateOptions::default());
    let keys: Vec<String> = host
        .hook_keys()
        .into_iter()
        .find(|(id, _)| id == "live")
        .map(|(_, keys)| keys)
        .unwrap_or_default();
    assert!(keys.contains(&"acme/ping".to_string()), "{keys:?}");
    assert!(keys.contains(&"dirge/event".to_string()), "{keys:?}");

    assert!(host.listens_key("acme/ping"));
    assert!(!host.listens_key("acme/other"));
    let replies = host.emit("acme/ping", &json!({"n": 3}));
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].addon_id, "live");
    assert_eq!(replies[0].result, Ok(json!("pong 3")));
    assert!(host.emit("acme/other", &json!({})).is_empty());
}

#[test]
fn posted_events_reach_the_event_hook_in_order() {
    use crate::event::AgentEvent;

    let host = live_host(IsolateOptions::default());
    for event in [
        AgentEvent::TurnStart { index: 0 },
        AgentEvent::Token("not heard".into()),
        AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: json!({}),
        },
        AgentEvent::Done {
            response: "ok".into(),
            tokens: 1,
            cost: 0.0,
        },
    ] {
        if let Some(ctx) = super::events::project(&event) {
            host.post(super::events::EVENT_KEY, &ctx);
        }
    }

    // Commands run in the order they were queued, so the posts ran first.
    assert_eq!(
        tool_text(&host, "heard", json!({})),
        "turn-start,tool-call,done"
    );
}

#[cfg(feature = "addons-nrepl")]
mod repl {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use super::*;
    use crate::addons::cljrs::isolate::ReplOptions;

    /// One bencoded nREPL `eval` of `code`; answers every byte read back
    /// until the server says the request is done.
    fn eval(endpoint: &str, code: &str) -> String {
        let mut stream = TcpStream::connect(endpoint).expect("connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let request = format!("d4:code{}:{}2:id1:12:op4:evale", code.len(), code);
        stream.write_all(request.as_bytes()).unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(&seen).contains("4:done") {
            let n = stream.read(&mut buf).expect("the server answers");
            assert!(
                n > 0,
                "connection closed: {}",
                String::from_utf8_lossy(&seen)
            );
            seen.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[test]
    fn a_repl_evaluation_changes_the_running_addon_and_dirge_takes_it_in() {
        let host = live_host(IsolateOptions {
            repl: Some(ReplOptions {
                addr: ([127, 0, 0, 1], 0).into(),
                port_file: None,
            }),
            refresh_after_eval: true,
        });
        let endpoint = host.repl_endpoint().expect("the REPL listens");

        let answer = eval(&endpoint, "(swap! live.addon/!extra conj \"from-repl\")");
        assert!(answer.contains("from-repl"), "{answer}");
        let report = next_sync(&host);

        assert_eq!(report.tools_added, vec!["from-repl".to_string()]);
        assert_eq!(
            tool_text(&host, "from-repl", json!({})),
            "hello from from-repl"
        );
        let version = eval(&endpoint, "(dirge.harness/version)");
        assert!(version.contains(env!("CARGO_PKG_VERSION")), "{version}");
    }

    #[test]
    fn without_live_refresh_an_evaluation_changes_nothing_until_asked() {
        let host = live_host(IsolateOptions {
            repl: Some(ReplOptions {
                addr: ([127, 0, 0, 1], 0).into(),
                port_file: None,
            }),
            refresh_after_eval: false,
        });
        let endpoint = host.repl_endpoint().expect("the REPL listens");

        eval(&endpoint, "(swap! live.addon/!extra conj \"quiet\")");
        std::thread::sleep(Duration::from_millis(200));
        assert!(host.sync().is_none(), "no refresh without being asked");

        eval(&endpoint, "(dirge.harness/refresh!)");
        assert_eq!(next_sync(&host).tools_added, vec!["quiet".to_string()]);
    }

    /// Bytes read from `stream` until `needle` shows up, within ten seconds.
    fn read_until(stream: &mut TcpStream, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(&seen).contains(needle) {
            assert!(
                Instant::now() < deadline,
                "no {needle} within 10s: {}",
                String::from_utf8_lossy(&seen)
            );
            let n = stream.read(&mut buf).expect("the server answers");
            assert!(
                n > 0,
                "connection closed: {}",
                String::from_utf8_lossy(&seen)
            );
            seen.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[test]
    fn an_interrupt_frees_the_isolate_from_a_runaway_repl_form() {
        let host = live_host(IsolateOptions {
            repl: Some(ReplOptions {
                addr: ([127, 0, 0, 1], 0).into(),
                port_file: None,
            }),
            refresh_after_eval: false,
        });
        let endpoint = host.repl_endpoint().expect("the REPL listens");

        // The runaway form holds the isolate thread, and with it every hook.
        let code = "(loop [] (recur))";
        let mut runaway = TcpStream::connect(&endpoint).expect("connects");
        runaway
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let request = format!(
            "d4:code{}:{}2:id3:run2:op4:eval7:session7:defaulte",
            code.len(),
            code
        );
        runaway.write_all(request.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let mut control = TcpStream::connect(&endpoint).expect("connects");
        control
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        control
            .write_all(b"d2:id4:stop12:interrupt-id3:run2:op9:interrupt7:session7:defaulte")
            .unwrap();
        read_until(&mut control, "4:done");

        let answer = read_until(&mut runaway, "4:done");
        assert!(answer.contains("11:interrupted"), "{answer}");

        // The isolate is free again: hooks and the REPL both answer.
        let replies = host.emit("acme/ping", &json!({"n": 7}));
        assert_eq!(replies[0].result, Ok(json!("pong 7")));
        assert!(eval(&endpoint, "(+ 1 2)").contains("1:3"));
    }
}
