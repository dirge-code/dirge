//! Apply plugin-issued [`TreeOp`]s to a live [`Session`] (P4d).
//!
//! The plugin worker queues ops on `harness-tree-ops`; the UI loop
//! drains them between events via `PluginManager::drain_tree_ops` and
//! hands the result to [`apply_tree_op`] here.
//!
//! Mirrors pi's `ctx.setLabel` / `ctx.fork` / `ctx.navigateTree` /
//! `ctx.newSession` / `ctx.switchSession` semantics. See `P4d` notes in
//! the README for the user-facing surface.

use compact_str::CompactString;

use crate::plugin::TreeOp;
use crate::session::{MessageRole, Session};
use crate::ui::input::InputEditor;

/// Outcome surfaced to the UI so it can render a status line, redraw
/// the chat, etc. Plain enum (no Display impl — callers format).
#[derive(Debug, PartialEq, Eq)]
pub enum TreeOpEffect {
    /// State change happened; UI should re-render the session and
    /// show the optional confirmation message.
    Applied(String),
    /// Op failed — message describes why. Surface as an error line.
    Failed(String),
    /// Session itself was replaced (new-session / switch-session).
    /// Caller must rebuild the agent + repaint completely.
    SessionReplaced(String),
}

/// Apply one op. Returns the UI-visible effect. Restored editor text
/// (for fork :before / navigate-tree on user messages) is pushed
/// straight into `input`.
/// dirge-dp24: optional `agent` parameter wired so plugin-driven
/// session resets (`TreeOp::NewSession`) and switches
/// (`TreeOp::SwitchSession`) fire the same lifecycle hooks the
/// `/clear` / `/session` slash commands do. Tests pass `None` to
/// skip the hook fires (they're testing the tree-op semantics, not
/// the lifecycle wiring). Production call site at
/// `ui/mod.rs::run_interactive` passes `Some(&agent)`.
pub fn apply_tree_op(
    op: TreeOp,
    session: &mut Session,
    input: &mut InputEditor,
    agent: Option<&crate::provider::AnyAgent>,
) -> TreeOpEffect {
    match op {
        TreeOp::SetLabel { id, label } => {
            let cid = CompactString::new(id.clone());
            match session.set_label(&cid, label.clone()) {
                Ok(()) => TreeOpEffect::Applied(match label {
                    Some(l) => format!("[plugin] labeled {} as \"{}\"", short(&id), l),
                    None => format!("[plugin] cleared label on {}", short(&id)),
                }),
                Err(e) => TreeOpEffect::Failed(format!("[plugin] set-label: {}", e)),
            }
        }
        TreeOp::Fork { id, restore_text } => {
            let cid = CompactString::new(id.clone());
            match session.fork_at(&cid) {
                Ok(original) => {
                    if restore_text {
                        input.set_text(&original.content);
                    }
                    TreeOpEffect::Applied(format!("[plugin] forked at {}", short(&id)))
                }
                Err(e) => TreeOpEffect::Failed(format!("[plugin] fork: {}", e)),
            }
        }
        TreeOp::NavigateTree { id } => {
            let cid = CompactString::new(id.clone());
            // Pi's semantics: if the target is a user message, move
            // the leaf to its parent and restore the prompt; for any
            // other role, the target itself becomes the new leaf.
            let role = session.message_store.get(&cid).map(|m| m.role);
            match role {
                None => TreeOpEffect::Failed(format!(
                    "[plugin] navigate-tree: unknown entry {}",
                    short(&id)
                )),
                Some(MessageRole::User) => match session.fork_at(&cid) {
                    Ok(original) => {
                        input.set_text(&original.content);
                        TreeOpEffect::Applied(format!(
                            "[plugin] navigated to user message {} (prompt restored)",
                            short(&id),
                        ))
                    }
                    Err(e) => TreeOpEffect::Failed(format!("[plugin] navigate-tree: {}", e)),
                },
                Some(_) => match session.switch_to_leaf(&cid) {
                    Ok(()) => {
                        TreeOpEffect::Applied(format!("[plugin] navigated to {}", short(&id)))
                    }
                    Err(e) => TreeOpEffect::Failed(format!("[plugin] navigate-tree: {}", e)),
                },
            }
        }
        TreeOp::NewSession { parent } => {
            // Persist the current session before resetting so the user
            // can still recover it via `/sessions`. Failures here are
            // logged but don't block the reset — getting wedged on disk
            // I/O would be worse than losing a session.
            //
            // Audit L15: previously a `let _ =` swallowed save errors
            // silently. On disk-full / permission errors the user lost
            // the previous session with no warning. Now we include
            // the save failure in the effect message so the user sees
            // it before the destructive reset takes effect.
            let prev_id = session.id.to_string();
            let parent_id = parent.as_deref().unwrap_or(&prev_id);
            // dirge-dp24: fire on_session_end BEFORE save (the
            // hook receives a transcript built from the still-live
            // session) and on_session_switch AFTER reset_to_new
            // gives us the new id. reset=true because this is a
            // genuinely fresh conversation from the provider's POV.
            if let Some(a) = agent {
                crate::agent::review::maybe_fire_session_end(a, session);
            }
            let save_err = crate::session::storage::save_session(session).err();
            session.reset_to_new(Some(parent_id));
            if let Some(a) = agent {
                crate::agent::review::maybe_fire_session_switch(
                    a,
                    &session.id,
                    &prev_id,
                    /* reset = */ true,
                );
            }
            input.set_text("");
            let mut msg = format!(
                "[plugin] new session started (parent: {})",
                short(parent_id),
            );
            if let Some(e) = save_err {
                msg.push_str(&format!(
                    "\n  warning: previous session save failed ({}); previous state may not be recoverable",
                    e,
                ));
            }
            TreeOpEffect::SessionReplaced(msg)
        }
        TreeOp::SwitchSession { id_prefix } => {
            match crate::session::storage::find_sessions_by_prefix(&id_prefix) {
                Ok(matches) => match matches.len() {
                    0 => TreeOpEffect::Failed(format!(
                        "[plugin] switch-session: no session matching '{}'",
                        id_prefix
                    )),
                    1 => {
                        // dirge-vpma.14: load through load_session_tip so the
                        // switched-to session gets migrate_session + the schema
                        // version guard + mtime capture. The raw parse from
                        // find_sessions_by_prefix skips all three (understated
                        // context on old files, disabled concurrent-writer
                        // detection, and a newer-version file truncated on the
                        // next save). Load BEFORE firing on_session_end / saving
                        // prev so a load failure leaves the current session
                        // untouched.
                        let matched_id =
                            matches.into_iter().next().expect("len == 1").id.to_string();
                        match crate::session::storage::load_session_tip(&matched_id) {
                            Err(e) => TreeOpEffect::Failed(format!(
                                "[plugin] switch-session: failed to load '{}': {}",
                                id_prefix, e
                            )),
                            Ok(loaded) => {
                                // dirge-dp24: fire on_session_end on the
                                // outgoing session BEFORE we overwrite it,
                                // then on_session_switch with the loaded id.
                                // reset=false because switch-session restores
                                // an existing logical conversation rather
                                // than starting fresh.
                                if let Some(a) = agent {
                                    crate::agent::review::maybe_fire_session_end(a, session);
                                }
                                let prev_id = session.id.to_string();
                                let save_err = crate::session::storage::save_session(session).err();
                                let new_id = loaded.id.clone();
                                *session = loaded;
                                if let Some(a) = agent {
                                    crate::agent::review::maybe_fire_session_switch(
                                        a,
                                        &session.id,
                                        &prev_id,
                                        /* reset = */ false,
                                    );
                                }
                                input.set_text("");
                                let mut msg = format!(
                                    "[plugin] switched to session {}",
                                    short(new_id.as_str()),
                                );
                                if let Some(e) = save_err {
                                    msg.push_str(&format!(
                                        "\n  warning: previous session save failed ({}); previous state may not be recoverable",
                                        e,
                                    ));
                                }
                                TreeOpEffect::SessionReplaced(msg)
                            }
                        }
                    }
                    n => {
                        // Surface the first few matches so the plugin
                        // author / user can pick a longer prefix.
                        let ids: Vec<String> = matches
                            .iter()
                            .take(3)
                            .map(|s| short(s.id.as_str()))
                            .collect();
                        let suffix = if n > 3 {
                            format!(" (and {} more)", n - 3)
                        } else {
                            String::new()
                        };
                        TreeOpEffect::Failed(format!(
                            "[plugin] switch-session: prefix '{}' matches {} sessions ({}){}",
                            id_prefix,
                            n,
                            ids.join(", "),
                            suffix,
                        ))
                    }
                },
                Err(e) => TreeOpEffect::Failed(format!("[plugin] switch-session: {}", e)),
            }
        }
    }
}

fn short(s: &str) -> String {
    crate::text::short_id(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    fn fresh_input() -> InputEditor {
        InputEditor::new()
    }

    #[test]
    fn set_label_applies_to_existing_node() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "hello");
        let id = s.messages[0].id.to_string();
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::SetLabel {
                id: id.clone(),
                label: Some("milestone".to_string()),
            },
            &mut s,
            &mut input,
            None,
        );
        assert!(matches!(effect, TreeOpEffect::Applied(_)));
        let node_label = s
            .tree
            .entries
            .get(&CompactString::new(&id))
            .and_then(|n| n.label.as_deref());
        assert_eq!(node_label, Some("milestone"));
    }

    /// SetLabel with None clears any existing label on the node.
    #[test]
    fn set_label_with_none_clears() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "hi");
        let id = s.messages[0].id.clone();
        s.set_label(&id, Some("old".to_string())).unwrap();
        let mut input = fresh_input();
        apply_tree_op(
            TreeOp::SetLabel {
                id: id.to_string(),
                label: None,
            },
            &mut s,
            &mut input,
            None,
        );
        assert_eq!(s.tree.entries[&id].label, None);
    }

    /// Fork with `restore_text=true` pushes the original prompt back
    /// into the editor so the user can re-edit.
    #[test]
    fn fork_with_restore_text_pushes_to_input() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "what's 2+2?");
        s.add_message(MessageRole::Assistant, "4");
        // Fork at the user message — its content should land back in
        // the editor and the assistant reply should be gone from the
        // current branch.
        let user_id = s.messages[0].id.to_string();
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::Fork {
                id: user_id,
                restore_text: true,
            },
            &mut s,
            &mut input,
            None,
        );
        assert!(matches!(effect, TreeOpEffect::Applied(_)));
        assert_eq!(input.buffer.as_str(), "what's 2+2?");
        assert!(s.messages.is_empty(), "leaf moved before user msg");
    }

    /// Fork with `restore_text=false` (the :at position) shifts the
    /// leaf but leaves the editor alone.
    #[test]
    fn fork_at_position_does_not_touch_input() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "q1");
        s.add_message(MessageRole::Assistant, "a1");
        let user_id = s.messages[0].id.to_string();
        let mut input = fresh_input();
        input.set_text("user-was-typing");
        apply_tree_op(
            TreeOp::Fork {
                id: user_id,
                restore_text: false,
            },
            &mut s,
            &mut input,
            None,
        );
        // Editor untouched.
        assert_eq!(input.buffer.as_str(), "user-was-typing");
    }

    /// Fork with unknown id surfaces a Failed effect, not a panic.
    #[test]
    fn fork_with_unknown_id_returns_failed() {
        let mut s = Session::new("p", "m", 0);
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::Fork {
                id: "ghost".to_string(),
                restore_text: true,
            },
            &mut s,
            &mut input,
            None,
        );
        assert!(matches!(effect, TreeOpEffect::Failed(_)));
    }

    /// NavigateTree to a user message restores text + moves to parent
    /// (pi parity).
    #[test]
    fn navigate_tree_user_message_restores_text() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "redo me");
        s.add_message(MessageRole::Assistant, "won't survive");
        let user_id = s.messages[0].id.to_string();
        let mut input = fresh_input();
        apply_tree_op(
            TreeOp::NavigateTree { id: user_id },
            &mut s,
            &mut input,
            None,
        );
        assert_eq!(input.buffer.as_str(), "redo me");
        assert!(s.messages.is_empty());
    }

    /// NavigateTree to a non-user (assistant) message sets that node
    /// as the leaf — no editor restore.
    #[test]
    fn navigate_tree_assistant_message_switches_leaf() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "q");
        s.add_message(MessageRole::Assistant, "a");
        s.add_message(MessageRole::User, "q2");
        let asst_id = s.messages[1].id.clone();
        let mut input = fresh_input();
        input.set_text("hands-off");
        apply_tree_op(
            TreeOp::NavigateTree {
                id: asst_id.to_string(),
            },
            &mut s,
            &mut input,
            None,
        );
        assert_eq!(input.buffer.as_str(), "hands-off");
        assert_eq!(s.tree.leaf_id.as_deref(), Some(asst_id.as_str()));
        // messages was rebuilt to the path-from-leaf for the new leaf.
        assert_eq!(s.messages.last().map(|m| m.content.as_str()), Some("a"));
    }

    /// NavigateTree with unknown id surfaces a Failed effect (we look
    /// up the role in message_store first to decide branch vs. switch).
    #[test]
    fn navigate_tree_unknown_id_returns_failed() {
        let mut s = Session::new("p", "m", 0);
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::NavigateTree {
                id: "missing".to_string(),
            },
            &mut s,
            &mut input,
            None,
        );
        assert!(matches!(effect, TreeOpEffect::Failed(_)));
    }

    /// NewSession wipes session state and assigns a fresh id; the
    /// effect must be SessionReplaced so the host rebuilds the agent.
    #[test]
    fn new_session_returns_session_replaced() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "stale");
        let old_id = s.id.clone();
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::NewSession { parent: None },
            &mut s,
            &mut input,
            None,
        );
        assert!(matches!(effect, TreeOpEffect::SessionReplaced(_)));
        assert!(s.messages.is_empty());
        assert_ne!(s.id, old_id);
    }

    /// dirge-vpma.14: switch-session must load through load_session_tip
    /// (migrate + version guard + mtime capture), not adopt the raw-parsed
    /// session from find_sessions_by_prefix. Proof: loaded_mtime is set only
    /// by the real load path; the raw parse leaves it None.
    #[test]
    fn switch_session_routes_through_load_session_tip() {
        let target_id = format!("switchtest-{}", uuid::Uuid::new_v4().simple());
        let mut target = Session::new("p", "m", 100_000);
        target.id = compact_str::CompactString::new(target_id.clone());
        target.add_message(MessageRole::User, "restore me");
        crate::session::storage::save_session(&mut target).expect("save target");

        let mut s = Session::new("p", "m", 0);
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::SwitchSession {
                id_prefix: target_id.clone(),
            },
            &mut s,
            &mut input,
            None,
        );
        assert!(matches!(effect, TreeOpEffect::SessionReplaced(_)));
        assert_eq!(s.id.as_str(), target_id);
        assert!(
            s.loaded_mtime.is_some(),
            "switch must route through load_session_tip (sets loaded_mtime); \
             a raw find_sessions_by_prefix parse leaves it None"
        );

        crate::session::storage::delete_session(&target_id).ok();
    }

    /// SwitchSession with a non-matching prefix returns Failed without
    /// touching the session.
    #[test]
    fn switch_session_unknown_prefix_returns_failed() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "keep me");
        let id_before = s.id.clone();
        let msg_count_before = s.messages.len();
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::SwitchSession {
                id_prefix: "zzzzzzzz-nope".to_string(),
            },
            &mut s,
            &mut input,
            None,
        );
        // No matching session on disk -> Failed.
        assert!(matches!(effect, TreeOpEffect::Failed(_)));
        // Session untouched.
        assert_eq!(s.id, id_before);
        assert_eq!(s.messages.len(), msg_count_before);
    }

    // ── dirge-dp24: plugin-driven session boundaries fire hooks ──

    use crate::agent::tools::ToolCache;
    use crate::extras::memory_provider::MemoryProvider;
    use crate::provider::{AnyAgent, AnyAgentInner};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingProvider {
        ends: Mutex<Vec<String>>,
        switches: Mutex<Vec<(String, String, bool)>>,
    }
    impl MemoryProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }
        fn view(&self, _: &str) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn add(&self, _: &str, _: &str, _kind: Option<&str>) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        fn replace(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _kind: Option<&str>,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        fn remove(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        fn on_session_end(&self, t: &str) {
            self.ends.lock().unwrap().push(t.to_string());
        }
        fn on_session_switch(&self, new_id: &str, parent_id: &str, reset: bool) {
            self.switches
                .lock()
                .unwrap()
                .push((new_id.into(), parent_id.into(), reset));
        }
    }

    fn agent_with_provider() -> (AnyAgent, Arc<RecordingProvider>) {
        use rig::client::CompletionClient;
        use rig::providers::openai;
        let client = openai::CompletionsClient::builder()
            .api_key("test-key")
            .build()
            .expect("openai client");
        let model = client.completion_model("gpt-4o");
        let inner = rig::agent::AgentBuilder::new(model).build();
        let provider = Arc::new(RecordingProvider::default());
        let provider_dyn: Arc<dyn MemoryProvider> = provider.clone();
        let agent = AnyAgent::new(
            AnyAgentInner::OpenAI(inner),
            ToolCache::new(),
            std::time::Duration::from_secs(300),
            Vec::new(),
            String::new(),
            "gpt-4o".to_string(),
        )
        .with_memory_provider(provider_dyn);
        (agent, provider)
    }

    /// dirge-dp24 — `TreeOp::NewSession` fires on_session_end on the
    /// outgoing session AND on_session_switch on the rotated id.
    /// `reset=true` because new-session is a fresh chat.
    #[test]
    fn new_session_fires_end_and_switch_with_reset_true() {
        let (agent, provider) = agent_with_provider();
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "outgoing");
        let old_id = s.id.to_string();
        let mut input = fresh_input();

        let effect = apply_tree_op(
            TreeOp::NewSession { parent: None },
            &mut s,
            &mut input,
            Some(&agent),
        );
        assert!(matches!(effect, TreeOpEffect::SessionReplaced(_)));

        let ends = provider.ends.lock().unwrap();
        assert_eq!(ends.len(), 1, "exactly one on_session_end fire");
        assert!(ends[0].contains("User: outgoing"));

        let switches = provider.switches.lock().unwrap();
        assert_eq!(switches.len(), 1, "exactly one on_session_switch fire");
        let (new_id, parent_id, reset) = &switches[0];
        assert_ne!(new_id, &old_id, "switch must report the new id");
        assert_eq!(parent_id, &old_id, "switch must carry the old id as parent");
        assert!(*reset, "new-session implies reset=true");
    }

    /// dirge-dp24 — `TreeOp::SwitchSession` against a non-matching
    /// prefix returns Failed and fires NO hooks.
    #[test]
    fn switch_session_no_match_fires_no_hooks() {
        let (agent, provider) = agent_with_provider();
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "stays");
        let mut input = fresh_input();

        let effect = apply_tree_op(
            TreeOp::SwitchSession {
                id_prefix: "zzzz-no-match".into(),
            },
            &mut s,
            &mut input,
            Some(&agent),
        );
        assert!(matches!(effect, TreeOpEffect::Failed(_)));

        assert!(
            provider.ends.lock().unwrap().is_empty(),
            "failed switch must NOT fire on_session_end"
        );
        assert!(
            provider.switches.lock().unwrap().is_empty(),
            "failed switch must NOT fire on_session_switch"
        );
    }
}
