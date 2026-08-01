//! Residual-objective handoff — what's still outstanding at a stop
//! (dirge-uw2l.5).
//!
//! The DS1 Remote Agent Experiment's 2-day scenario aborted at roughly 70% of
//! its validation objectives; within about 10 hours the team designed, tested,
//! and flew a 6-hour scenario targeting PRECISELY the remaining 30%, reaching
//! 100% (paper §3.4/§5). That follow-up is only possible when the outstanding
//! work is named at the boundary where someone — or a resumed session — looks
//! for it.
//!
//! dirge's max-turns truncation notice used to carry none of that: a resume
//! re-derived scope from the raw transcript. [`residual_block`] renders the
//! live board's outstanding objectives as a plain-text block the truncation
//! notice appends ([`super::run`]) and the session digest reuses
//! ([`crate::agent::session_digest`]), so a resume is told what's left rather
//! than rediscovering it.
//!
//! Outstanding-only by construction. The board mirror (and the digest's
//! `todos`) hold only non-terminal items — done/cancelled drop off the live
//! board — so this reports what REMAINS, not a done/remaining split it cannot
//! honestly compute from that data. Self-contained and pure: no globals, no
//! model call.

use crate::agent::tools::todo::TodoItem;

/// Cap on the per-title list; beyond this a "+N more" summary keeps the notice
/// short. Mirrors the digest's file/command caps — enough to characterize,
/// not flood.
const MAX_OUTSTANDING_TITLES: usize = 10;

/// Render the outstanding objectives on `board` as a plain-text block, or
/// `None` when the board is empty (the no-op case — nothing to hand off, so
/// the truncation notice and digest stay byte-identical to before).
///
/// The caller passes the non-terminal mirror (the todo list / digest `todos`),
/// so every item counts as outstanding. Deterministic: no model call, stable
/// ordering (input order), titles trimmed.
pub fn residual_block(board: &[TodoItem]) -> Option<String> {
    if board.is_empty() {
        return None;
    }
    let mut out = format!("Objectives still outstanding ({}):", board.len());
    let shown = board.len().min(MAX_OUTSTANDING_TITLES);
    for t in &board[..shown] {
        out.push_str("\n- ");
        out.push_str(t.content.trim());
    }
    if board.len() > MAX_OUTSTANDING_TITLES {
        out.push_str(&format!("\n+{} more", board.len() - MAX_OUTSTANDING_TITLES));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: &str, priority: &str) -> TodoItem {
        TodoItem {
            content: content.into(),
            status: status.into(),
            priority: priority.into(),
        }
    }

    #[test]
    fn empty_board_is_none_so_the_notice_is_unchanged() {
        assert!(residual_block(&[]).is_none());
    }

    #[test]
    fn lists_outstanding_count_and_titles() {
        let block = residual_block(&[
            item("ship the residual handoff", "in_progress", "high"),
            item("wire the digest reuse", "open", "normal"),
        ])
        .expect("non-empty board yields a block");
        assert!(
            block.contains("Objectives still outstanding (2):"),
            "headline names the count: {block}"
        );
        assert!(
            block.contains("- ship the residual handoff"),
            "first title listed: {block}"
        );
        assert!(
            block.contains("- wire the digest reuse"),
            "second title listed: {block}"
        );
    }

    #[test]
    fn caps_titles_at_ten_with_more_count() {
        let board: Vec<TodoItem> = (0..12)
            .map(|i| item(&format!("task {i}"), "open", "normal"))
            .collect();
        let block = residual_block(&board).expect("non-empty board yields a block");
        assert!(
            block.contains("Objectives still outstanding (12):"),
            "{block}"
        );
        assert!(block.contains("- task 0"), "first shown: {block}");
        assert!(block.contains("- task 9"), "tenth shown: {block}");
        assert!(!block.contains("- task 10"), "eleventh elided: {block}");
        assert!(block.contains("+2 more"), "overflow summarized: {block}");
    }

    #[test]
    fn trims_title_whitespace() {
        let block = residual_block(&[item("  padded title  ", "open", "low")])
            .expect("non-empty board yields a block");
        assert!(block.contains("- padded title"), "trimmed: {block}");
        assert!(!block.contains("padded title  "), "no trailing ws: {block}");
    }
}
