//! Session compaction.
//!
//! Token-budget checks, summary insertion, and tree-pruning logic.
//! Extracted from `session/mod.rs` to keep the Session struct file
//! focused on data model + message operations.

use crate::session::{Compaction, MessageRole, Session, SessionMessage, TreeNode};
use compact_str::CompactString;

/// Return the latest compaction summary (if any) and the first
/// message index still live in `messages`.
pub(crate) fn compacted_context(
    compactions: &[Compaction],
    messages_len: usize,
) -> (Option<&str>, usize) {
    match compactions.last() {
        Some(c) => (
            Some(c.summary.as_str()),
            c.first_kept_index.min(messages_len),
        ),
        None => (None, 0),
    }
}

/// Same as `compress_reporting` but discards the pruned-siblings
/// count. Used only by tests that don't care about the count.
#[cfg(test)]
pub(crate) fn compress(
    session: &mut Session,
    summary: String,
    first_kept_index: usize,
    token_savings: u64,
) {
    let _ = compress_reporting(session, summary, first_kept_index, token_savings);
}

/// Compress the session by replacing the first `first_kept_index`
/// messages with a single system summary message. Returns the number
/// of NON-active-path tree nodes that were pruned because their
/// ancestor was dropped (sibling branches). The host uses this to
/// surface a "discarded N forked branches" notification.
pub(crate) fn compress_reporting(
    session: &mut Session,
    summary: String,
    first_kept_index: usize,
    token_savings: u64,
) -> usize {
    let first_kept_index = first_kept_index.min(session.messages.len());
    let summarized_count = first_kept_index;

    session.total_estimated_tokens = session.total_estimated_tokens.saturating_sub(token_savings);
    let summary_tokens = Session::estimate_tokens(&summary);
    session.total_estimated_tokens = session
        .total_estimated_tokens
        .saturating_add(summary_tokens);

    let summary_id = crate::session::new_message_id();
    let summary_ts = chrono::Utc::now().timestamp();
    let summary_msg = SessionMessage {
        role: MessageRole::System,
        content: CompactString::from(summary.clone()),
        estimated_tokens: summary_tokens,
        id: summary_id.clone(),
        timestamp: summary_ts,
        tool_calls: Vec::new(),
    };

    let dropped_ids: Vec<CompactString> = session.messages[..first_kept_index]
        .iter()
        .map(|m| m.id.clone())
        .collect();
    let dropped_set: std::collections::HashSet<CompactString> =
        dropped_ids.iter().cloned().collect();

    session.messages.drain(..first_kept_index);
    session.messages.insert(0, summary_msg.clone());

    session.ensure_back_compat_initialized();

    let active_ids: std::collections::HashSet<CompactString> =
        session.messages.iter().map(|m| m.id.clone()).collect();

    let mut to_prune: std::collections::HashSet<CompactString> = dropped_set.clone();
    loop {
        let new_ids: Vec<CompactString> = session
            .tree
            .entries
            .iter()
            .filter(|(id, node)| {
                !to_prune.contains(id.as_str())
                    && !active_ids.contains(id.as_str())
                    && node
                        .parent
                        .as_ref()
                        .map(|p| to_prune.contains(p))
                        .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if new_ids.is_empty() {
            break;
        }
        for id in new_ids {
            to_prune.insert(id);
        }
    }

    let sibling_pruned_count = to_prune.len().saturating_sub(dropped_set.len());

    // Phase 4: capture BranchSummary per pruned subtree before removal.
    let now_rfc = chrono::Utc::now().to_rfc3339();
    let mut subtree_summaries: Vec<crate::session::BranchSummary> = Vec::new();
    for id in &to_prune {
        if dropped_set.contains(id) {
            continue;
        }
        let node = match session.tree.entries.get(id) {
            Some(n) => n,
            None => continue,
        };
        let parent = match &node.parent {
            Some(p) => p,
            None => continue,
        };
        if !dropped_set.contains(parent) {
            continue;
        }
        let mut count = 0usize;
        let mut stack = vec![id.clone()];
        while let Some(cur) = stack.pop() {
            if !to_prune.contains(&cur) {
                continue;
            }
            count += 1;
            for (child_id, child_node) in session.tree.entries.iter() {
                if child_node.parent.as_ref() == Some(&cur) {
                    stack.push(child_id.clone());
                }
            }
        }
        let label_prefix = node
            .label
            .as_deref()
            .map(|l| format!("[{}] ", l))
            .unwrap_or_default();
        let body_preview = session
            .message_store
            .get(id)
            .map(|m| {
                let s: String = m.content.chars().take(80).collect();
                if m.content.chars().count() > 80 {
                    format!("{}…", s)
                } else {
                    s
                }
            })
            .unwrap_or_default();
        subtree_summaries.push(crate::session::BranchSummary {
            root_id: id.clone(),
            parent_id: parent.clone(),
            message_count: count,
            preview: format!("{}{}", label_prefix, body_preview),
            created_at: now_rfc.clone(),
        });
    }
    session.branch_summaries.extend(subtree_summaries);

    for id in &to_prune {
        session.tree.entries.remove(id);
        session.message_store.remove(id);
    }
    let new_root = TreeNode {
        id: summary_id.clone(),
        parent: None,
        timestamp: summary_ts,
        label: None,
    };
    session.tree.entries.insert(summary_id.clone(), new_root);
    session
        .message_store
        .insert(summary_id.clone(), summary_msg);
    if let Some(first_kept) = session.messages.get(1) {
        if let Some(node) = session.tree.entries.get_mut(&first_kept.id) {
            node.parent = Some(summary_id.clone());
        }
    } else {
        session.tree.leaf_id = Some(summary_id.clone());
    }
    let leaf_dropped = session
        .tree
        .leaf_id
        .as_ref()
        .map(|id| dropped_set.contains(id))
        .unwrap_or(false);
    if leaf_dropped {
        session.tree.leaf_id = session
            .messages
            .get(1)
            .map(|m| m.id.clone())
            .or(Some(summary_id.clone()));
    }

    session.compactions.clear();
    session.compactions.push(Compaction {
        summary: CompactString::from(summary),
        first_kept_index: 1,
        summarized_count,
        token_savings,
        created_at: CompactString::new(chrono::Utc::now().to_rfc3339()),
    });

    session.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
    sibling_pruned_count
}

/// Smallest index `>= cut_idx` whose message is a User turn, clamped to
/// `messages.len()` when none is found at or after `cut_idx`. Cutting the
/// session on a user boundary guarantees the kept tail never begins with an
/// orphaned tool_result (which would 400 the next request) — same discipline
/// as the loop-space `compression::compute_compress_window`. Snapping FORWARD
/// only ever keeps a whole recent turn, never half of a tool_use↔result pair.
pub(crate) fn align_cut_to_user_boundary(messages: &[SessionMessage], cut_idx: usize) -> usize {
    let mut i = cut_idx;
    while i < messages.len() && messages[i].role != MessageRole::User {
        i += 1;
    }
    i
}

/// Compute the SESSION-SPACE compaction cut: the index of the first message
/// to KEEP verbatim, chosen so the kept tail holds roughly `keep_recent_tokens`
/// of the most recent conversation, aligned to a user boundary.
///
/// This is the canonical cut used by BOTH the `/compress` slash path and the
/// auto-fold persistence handler. The distinction matters for dirge-4kgk: the
/// agent loop reports its fold boundary in LOOP space (`current_context.messages`,
/// where every tool_result is its own entry), but `compress_reporting` drains
/// `session.messages` (tool results embedded in their assistant message, far
/// fewer entries). Feeding a loop-space index straight into the session vec
/// over-drains and destroys the verbatim tail; recompute the cut here in
/// session space instead.
pub(crate) fn compaction_cut_idx(session: &Session, keep_recent_tokens: u64) -> usize {
    let mut accumulated = 0u64;
    let mut cut_idx = session.messages.len();
    for (i, msg) in session.messages.iter().enumerate().rev() {
        if accumulated >= keep_recent_tokens {
            cut_idx = i + 1;
            break;
        }
        accumulated = accumulated.saturating_add(msg.estimated_tokens);
    }
    align_cut_to_user_boundary(&session.messages, cut_idx)
}

#[cfg(test)]
mod cut_tests {
    use super::*;
    use crate::session::{ToolCallEntry, ToolCallState};

    fn three_tools(prefix: &str) -> Vec<ToolCallEntry> {
        (0..3)
            .map(|i| ToolCallEntry {
                id: format!("{prefix}-{i}"),
                name: "bash".to_string(),
                args: serde_json::json!({ "command": "echo hi" }),
                state: ToolCallState::Completed {
                    result: "ok".repeat(20),
                },
            })
            .collect()
    }

    /// dirge-4kgk: the auto-fold handler must cut the session in SESSION
    /// space. A tool-heavy conversation expands to many more LOOP entries
    /// than session messages; feeding a loop-space index to `compress_reporting`
    /// over-drains and destroys the recent verbatim tail. The session-space
    /// cut keeps it.
    #[test]
    fn compaction_cut_is_session_space_and_keeps_recent_tail() {
        let mut s = Session::new("p", "m", 128_000);
        // Old, token-heavy head + tool-heavy middle (each assistant here
        // expands to 1 + 3 = 4 loop entries).
        s.add_message(MessageRole::User, &"original ask ".repeat(40));
        s.add_message_with_tool_calls(
            MessageRole::Assistant,
            &"working on it ".repeat(40),
            three_tools("a"),
        );
        s.add_message_with_tool_calls(
            MessageRole::Assistant,
            &"still working ".repeat(40),
            three_tools("b"),
        );
        // Small, recent verbatim tail that MUST survive a fold.
        s.add_message(MessageRole::User, "recent question");
        s.add_message(MessageRole::Assistant, "recent answer");

        // Loop space is strictly larger than session space because each
        // tool_result becomes its own loop entry.
        let loop_len = crate::agent::runner::convert_history(&s).len();
        assert!(
            loop_len > s.messages.len(),
            "expected loop expansion ({loop_len}) > session len ({})",
            s.messages.len()
        );

        // keep_recent picked to retain just the two small tail messages.
        let cut = compaction_cut_idx(&s, 20);
        assert!(cut > 0 && cut < s.messages.len(), "cut={cut}");
        assert!(
            s.messages[cut..]
                .iter()
                .any(|m| m.content.contains("recent question")),
            "recent user turn must be in the kept tail"
        );
        assert!(
            s.messages[cut..]
                .iter()
                .any(|m| m.content.contains("recent answer")),
            "recent assistant turn must be in the kept tail"
        );

        // Fix: compressing at the session-space cut preserves the tail.
        let mut fixed = s.clone();
        compress(&mut fixed, "SUMMARY".to_string(), cut, 0);
        assert!(
            fixed
                .messages
                .iter()
                .any(|m| m.content.contains("recent answer")),
            "session-space cut must keep the verbatim tail"
        );

        // Regression: applying the LOOP-space index (the pre-fix bug) clamps
        // to the session length and drains everything, destroying the tail.
        let mut buggy = s.clone();
        compress(&mut buggy, "SUMMARY".to_string(), loop_len, 0);
        assert!(
            !buggy
                .messages
                .iter()
                .any(|m| m.content.contains("recent answer")),
            "a loop-space index over-drains and loses the tail (the bug)"
        );
    }
}
