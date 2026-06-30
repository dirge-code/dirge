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
