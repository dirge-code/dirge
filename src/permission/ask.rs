use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub type AskSender = mpsc::Sender<AskRequest>;
pub type AskReceiver = mpsc::Receiver<AskRequest>;

#[derive(Debug)]
pub struct AskRequest {
    pub tool: String,
    pub input: String,
    /// Why an `approval_provider` flagged this call, when the prompt is an
    /// escalated evaluator denial (dirge-r16x). `None` for an ordinary
    /// permission prompt. Shown to the user so they know what the evaluator
    /// objected to before they decide.
    pub reason: Option<String>,
    pub reply: oneshot::Sender<UserDecision>,
}

#[derive(Debug, Clone)]
pub enum UserDecision {
    AllowOnce,
    AllowAlways(String),
    Deny,
}

/// Drain `ask_rx` in headless modes (`--print`, `--loop`) by denying
/// every tool-permission ask. These modes have no UI loop and no human
/// at a keyboard, so a tool that routes to a confirmation prompt would
/// otherwise send an `AskRequest` and block on `reply_rx.await` forever
/// — the receiver is held but never serviced, suspending the agent loop
/// and hanging the whole run with no output and no `result` (issue
/// #523). Auto-denying fails fast: the model sees the denial and can
/// re-plan, exactly as `extras::acp::spawn_acp_ask_drain` does for ACP.
///
/// `--yolo` allows every tool unconditionally and never reaches the ask
/// path, so a fully-unattended run that must not be blocked should use
/// `--yolo` (or configure explicit allow rules). `--accept-all` still
/// withholds the operations it deems dangerous; those now surface as a
/// clean deny instead of a silent hang.
pub fn spawn_headless_ask_responder(mut ask_rx: AskReceiver) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = ask_rx.recv().await {
            eprintln!(
                "[headless] tool '{}' requires confirmation but no interactive \
                 prompt is available; denying. Use --yolo or add an allow rule \
                 to permit it.",
                req.tool,
            );
            // Caller is awaiting `req.reply`; Deny is a clearer signal
            // than dropping the sender (which would surface as
            // "Permission system unavailable").
            let _ = req.reply.send(UserDecision::Deny);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headless responder must answer a pending ask so the awaiting
    /// tool call resolves instead of hanging forever (issue #523).
    #[tokio::test]
    async fn headless_responder_denies_pending_ask() {
        let (tx, rx) = mpsc::channel(4);
        let _handle = spawn_headless_ask_responder(rx);

        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(AskRequest {
            tool: "bash".to_string(),
            input: "which allium".to_string(),
            reason: None,
            reply: reply_tx,
        })
        .await
        .unwrap();

        let decision = reply_rx.await.expect("responder should answer the ask");
        assert!(matches!(decision, UserDecision::Deny));
    }

    /// Closing the sender ends the drain task cleanly (no leak/panic).
    #[tokio::test]
    async fn headless_responder_exits_when_channel_closes() {
        let (tx, rx) = mpsc::channel::<AskRequest>(1);
        let handle = spawn_headless_ask_responder(rx);
        drop(tx);
        handle.await.expect("drain task should finish");
    }
}
