use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub type AskSender = mpsc::Sender<AskRequest>;
pub type AskReceiver = mpsc::Receiver<AskRequest>;

#[derive(Debug)]
pub struct AskRequest {
    pub tool: String,
    /// The permission MATCH key — what rules and "allow always" patterns are
    /// derived from. For tools whose match key isn't the whole story (an MCP
    /// call matches on `mcp_tool:<server>:<tool>`, which says nothing about
    /// what it was asked to do), the rest goes in [`Self::details`].
    pub input: String,
    /// Display-only detail about the call that the match key omits — MCP tool
    /// arguments, for instance (dirge-hzd8 / #744). Rendered under the input
    /// in the prompt; never used for rule matching or pattern suggestion, so
    /// it can carry anything the user needs to see to decide.
    pub details: Option<String>,
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
    Deny {
        /// What the user typed at the deny prompt: what the agent should do
        /// INSTEAD (dirge-hzd8). Folded into the tool-result error so the
        /// model gets the redirection rather than a bare refusal it can only
        /// guess its way around. `None` for a plain deny.
        note: Option<String>,
    },
}

impl UserDecision {
    /// A plain deny with no guidance attached.
    pub fn deny() -> Self {
        Self::Deny { note: None }
    }
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
            let _ = req.reply.send(UserDecision::deny());
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
            details: None,
            reason: None,
            reply: reply_tx,
        })
        .await
        .unwrap();

        let decision = reply_rx.await.expect("responder should answer the ask");
        assert!(matches!(decision, UserDecision::Deny { .. }));
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
