//! End-to-end: a tiny local event-stream server, the real client
//! loop, a recording sink. Covers discovery, token admission, op
//! routing, stale-panel cleanup on disconnect, reconnect and replies.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::client::FeedOptions;
use super::discovery::{DISCOVERY_FILE, Source};
use super::ops::tests::RecordingSink;
use super::ops::{FeedEffect, NotifyLevel};
use super::{ReplyAction, reply_to, spawn};
use crate::ui::panels_ext::PanelOp;

const TOKEN: &str = "t0k&en";
const TOKEN_QUERY: &str = "token=t0k%26en";

#[test]
fn reply_bodies_match_the_wire() {
    assert_eq!(
        ReplyAction::Focus("w-1".into()).to_json(),
        r#"{"action":"focus","target":"w-1"}"#
    );
    assert_eq!(ReplyAction::Unfocus.to_json(), r#"{"action":"unfocus"}"#);
    assert_eq!(ReplyAction::NextTab.to_json(), r#"{"action":"next-tab"}"#);
    assert_eq!(ReplyAction::PrevTab.to_json(), r#"{"action":"prev-tab"}"#);
    assert_eq!(ReplyAction::Refresh.to_json(), r#"{"action":"refresh"}"#);
    assert_eq!(
        ReplyAction::Ack("s-1".into()).to_json(),
        r#"{"action":"ack","target":"s-1"}"#
    );
}

/// One request as the test server saw it.
#[derive(Debug)]
struct Seen {
    head: String,
    body: String,
}

impl Seen {
    fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or_default()
    }

    fn has_header(&self, name: &str) -> bool {
        self.head
            .lines()
            .skip(1)
            .any(|l| l.to_ascii_lowercase().starts_with(&format!("{name}:")))
    }
}

async fn read_request(sock: &mut TcpStream) -> Option<Seen> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if sock.read(&mut byte).await.ok()? == 0 {
            return None;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).await.ok()?;
    Some(Seen {
        head,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                        Cache-Control: no-cache\r\nConnection: close\r\n\r\n";

/// Serves: 1st events connection -> retry + a panel + heartbeat,
/// then hangs up; 2nd events connection -> a notify, then stays
/// open; any reply -> 204; a wrong token -> 401.
async fn serve(listener: TcpListener, seen: mpsc::UnboundedSender<Seen>) {
    let mut events_served = 0usize;
    let mut open = Vec::new();
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let Some(req) = read_request(&mut sock).await else {
            continue;
        };
        let line = req.request_line().to_string();
        let _ = seen.send(req);
        if !line.contains(TOKEN_QUERY) {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        if line.starts_with("POST ") && line.contains("/feed/reply?") {
            let _ = sock
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await;
            continue;
        }
        events_served += 1;
        let _ = sock.write_all(SSE_HEAD.as_bytes()).await;
        if events_served == 1 {
            // Split one frame across writes to exercise the parser at
            // the socket boundary.
            let frames = [
                "retry: 100\n\n",
                "id: 1\nevent: feed\ndata: {\"op\":\"ui/show-panel\",\"panel/id\":\"p\",",
                "\"title\":\"P\",\"lines\":[{\"text\":\"row\",\"face\":\"success\"}]}\n\n",
                ": ping\n\n",
            ];
            for f in frames {
                let _ = sock.write_all(f.as_bytes()).await;
                let _ = sock.flush().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            drop(sock);
        } else {
            let _ = sock
                .write_all(b"id: 2\nevent: feed\ndata: {\"op\":\"ui/notify\",\"message\":\"back\",\"level\":\"warn\"}\n\n")
                .await;
            open.push(sock);
        }
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dirge-panel-feed-e2e-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(unix)]
fn write_private(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

async fn wait_for(sink: &RecordingSink, what: &str, pred: impl Fn(&[FeedEffect]) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        {
            let got = sink.0.lock().unwrap();
            if pred(&got) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; got {got:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn feed_routes_ops_cleans_up_reconnects_and_replies() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(serve(listener, seen_tx));

    let dir = temp_dir("run");
    let discovery = dir.join(DISCOVERY_FILE);
    write_private(
        &discovery,
        &serde_json::json!({"url": format!("http://127.0.0.1:{port}/feed/"), "token": TOKEN})
            .to_string(),
    );
    let source = Source::DiscoveryFile(discovery);

    let sink = Arc::new(RecordingSink::default());
    let opts = FeedOptions {
        initial_retry: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        idle_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(2),
        features: Vec::new(),
    };
    let handle = spawn(source.clone(), sink.clone(), opts);

    wait_for(&sink, "notify after reconnect", |got| {
        got.iter()
            .any(|e| matches!(e, FeedEffect::Notify { message, .. } if message == "back"))
    })
    .await;
    let got = sink.0.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![
            FeedEffect::Panel(PanelOp::Show {
                id: "p".into(),
                title: "P".into(),
                lines: vec![crate::ui::panels_ext::PanelLine::new(
                    "row",
                    crate::ui::panels_ext::PanelFace::Success
                )],
            }),
            // The first connection dropped: its panel must not linger.
            FeedEffect::Panel(PanelOp::Close { id: "p".into() }),
            FeedEffect::Notify {
                level: NotifyLevel::Warn,
                message: "back".into()
            },
        ]
    );

    reply_to(&source, &ReplyAction::Focus("w-1".into()))
        .await
        .expect("reply accepted");

    handle.shutdown().await;
    server.abort();

    let mut requests = Vec::new();
    while let Ok(r) = seen_rx.try_recv() {
        requests.push(r);
    }
    let events: Vec<_> = requests
        .iter()
        .filter(|r| r.request_line().starts_with("GET "))
        .collect();
    assert!(events.len() >= 2, "reconnected: {requests:?}");
    for r in &requests {
        assert!(!r.has_header("origin"), "no Origin header: {}", r.head);
    }
    for r in &events {
        assert!(
            r.request_line()
                .starts_with(&format!("GET /feed/events?{TOKEN_QUERY} ")),
            "{}",
            r.request_line()
        );
        assert!(r.has_header("accept"));
    }
    let reply = requests
        .iter()
        .find(|r| r.request_line().starts_with("POST "))
        .expect("reply seen");
    assert!(
        reply
            .request_line()
            .starts_with(&format!("POST /feed/reply?{TOKEN_QUERY} "))
    );
    assert_eq!(reply.body, r#"{"action":"focus","target":"w-1"}"#);
    std::fs::remove_dir_all(dir).ok();
}

#[cfg(unix)]
#[tokio::test]
async fn wrong_token_is_retried_and_reply_reports_status() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(serve(listener, seen_tx));

    let dir = temp_dir("bad");
    let token_file = dir.join("token");
    write_private(&token_file, "wrong\n");
    let source = Source::Explicit {
        url: format!("http://127.0.0.1:{port}/feed"),
        token_file: Some(token_file),
    };
    let sink = Arc::new(RecordingSink::default());
    let opts = FeedOptions {
        initial_retry: Duration::from_millis(20),
        max_backoff: Duration::from_millis(40),
        idle_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(2),
        features: Vec::new(),
    };
    let handle = spawn(source.clone(), sink.clone(), opts);
    // Two refused attempts prove the loop keeps retrying.
    for _ in 0..2 {
        let r = tokio::time::timeout(Duration::from_secs(5), seen_rx.recv())
            .await
            .expect("attempt")
            .expect("request");
        assert!(
            r.request_line()
                .starts_with("GET /feed/events?token=wrong ")
        );
    }
    let err = reply_to(&source, &ReplyAction::Refresh).await.unwrap_err();
    assert!(
        matches!(err, super::client::ReplyError::Status(401)),
        "{err}"
    );
    assert!(!err.to_string().contains("wrong"), "{err}");
    handle.shutdown().await;
    server.abort();
    assert!(sink.0.lock().unwrap().is_empty());
    std::fs::remove_dir_all(dir).ok();
}

mod reply_command {
    use std::sync::Mutex;

    use super::super::client::ReplyError;
    use super::super::{ReplyAction, ReplyTransport, failure_notice, send_reply};
    use crate::ui::notifications::Notification;

    #[test]
    fn parses_every_verb() {
        let ok = |args: &[&str]| ReplyAction::parse(args).expect("valid");
        assert_eq!(ok(&["next"]), ReplyAction::NextTab);
        assert_eq!(ok(&["next-tab"]), ReplyAction::NextTab);
        assert_eq!(ok(&["prev"]), ReplyAction::PrevTab);
        assert_eq!(ok(&["refresh"]), ReplyAction::Refresh);
        assert_eq!(ok(&["unfocus"]), ReplyAction::Unfocus);
        assert_eq!(ok(&["focus", "w-1"]), ReplyAction::Focus("w-1".into()));
    }

    #[test]
    fn rejects_bad_input_with_usage() {
        for args in [
            &[][..],
            &["sideways"][..],
            &["focus"][..],
            &["focus", "a", "b"][..],
            &["next", "x"][..],
        ] {
            let err = ReplyAction::parse(args).expect_err("invalid");
            assert!(err.contains("usage: /panel"), "{args:?} -> {err}");
        }
        let missing = ReplyAction::parse(&["focus"]).unwrap_err();
        assert!(missing.contains("needs an item id"), "{missing}");
    }

    /// Records every action and answers with a canned result.
    struct Recording {
        sent: Mutex<Vec<ReplyAction>>,
        answer: fn() -> Result<(), ReplyError>,
    }

    impl ReplyTransport for Recording {
        async fn send(&self, action: &ReplyAction) -> Result<(), ReplyError> {
            self.sent.lock().unwrap().push(action.clone());
            (self.answer)()
        }
    }

    #[tokio::test]
    async fn dispatch_goes_through_the_transport() {
        let t = Recording {
            sent: Mutex::new(Vec::new()),
            answer: || Ok(()),
        };
        assert!(send_reply(&t, ReplyAction::NextTab).await.is_none());
        assert!(
            send_reply(&t, ReplyAction::Focus("x".into()))
                .await
                .is_none()
        );
        assert_eq!(
            *t.sent.lock().unwrap(),
            [ReplyAction::NextTab, ReplyAction::Focus("x".into())]
        );
    }

    #[tokio::test]
    async fn failures_become_notifications() {
        let not_running = Recording {
            sent: Mutex::new(Vec::new()),
            answer: || Err(ReplyError::NotRunning),
        };
        match send_reply(&not_running, ReplyAction::Refresh).await {
            Some(Notification::Warn(m)) => {
                assert!(m.contains("refresh") && m.contains("no panel feed"), "{m}")
            }
            other => panic!("expected a warning, got {other:?}"),
        }
        let refused = Recording {
            sent: Mutex::new(Vec::new()),
            answer: || Err(ReplyError::Status(503)),
        };
        match send_reply(&refused, ReplyAction::PrevTab).await {
            Some(Notification::Error(m)) => {
                assert!(m.contains("prev-tab") && m.contains("503"), "{m}")
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn transport_errors_are_errors_and_escape_free() {
        let n = failure_notice(
            &ReplyAction::NextTab,
            &ReplyError::Http("boom \u{1b}[31mred".into()),
        );
        match n {
            Notification::Error(m) => assert!(!m.contains('\u{1b}'), "{m:?}"),
            other => panic!("{other:?}"),
        }
    }
}
