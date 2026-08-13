//! Retry a transiently-failed tool call — but only where re-running it cannot
//! duplicate an effect (dirge-61sv).
//!
//! # The asymmetry that shapes this
//!
//! Provider requests retry freely ([`crate::agent::recovery`]) because a
//! request that failed in transport did not reach the model, or produced
//! nothing we kept. Tool calls are not like that. **A timeout does not mean
//! the work did not happen.** A `bash` command killed at its time budget may
//! have run to completion, or half of it. dirge-e31n.5's taxonomy names this
//! state exactly — an aborted or timed-out `bash` is `Unknown`, not
//! `NoEffect` — and re-issuing an `Unknown` is how one `git push` becomes two.
//!
//! So the retry is gated on the tool, not on the error. The error class says
//! *retrying could work*; the tool's operation says *retrying is allowed to be
//! tried*. Both must hold.
//!
//! # Why this is worth having even so narrowly scoped
//!
//! [`Operation::Read`] covers `read`, `grep`, `glob`, `list_dir` and the whole
//! LSP family (`find_definition`, `find_callers`, `list_symbols`, …). The LSP
//! tools time out routinely while a language server is still indexing — a
//! failure that is purely a function of *when* the call was made, that the
//! model can do nothing useful about, and that today is handed back to it as
//! an error to reason about. That is the case this exists for.
//!
//! # What is deliberately NOT retried
//!
//! Everything else, by a positive allowlist rather than a deny-list, so a new
//! [`Operation`] is non-retryable until someone states otherwise.
//!
//! In particular this does NOT reuse [`Operation::is_side_effecting`], which
//! looks like the right predicate and is not. That method answers "should the
//! loop guard gate repetition of this?" — under it `Operation::Other` (unknown
//! MCP and plugin tools) and `Operation::Memory` are both non-side-effecting,
//! and re-running either is exactly the hazard. Borrowing a predicate that
//! answers a neighbouring question is how a guard ends up protecting something
//! it was never measured against.

use std::time::Duration;

use crate::permission::engine::tool_operation;
use crate::permission::engine::types::Operation;

use super::tool_error_class::{ErrorClass, classify};

/// Total attempts for a retryable failure — so 3 means the original call plus
/// two retries.
///
/// Small on purpose. This sits INSIDE the model's turn with the user waiting
/// on it, and a read that is still failing on its third try is reporting a
/// real condition, not a blip.
pub const MAX_ATTEMPTS: u32 = 3;

/// First backoff, doubled per attempt: 250ms then 500ms.
///
/// Deliberately NOT [`crate::agent::recovery::ErrorKind::backoff_duration`],
/// which is tuned for provider rate limits and can wait for a `Retry-After`
/// measured in minutes. A tool retry is a warming LSP server or an `EAGAIN`;
/// the right wait is a beat, and a minute-long pause mid-turn would look like
/// a hang.
const BASE_BACKOFF: Duration = Duration::from_millis(250);

/// Whether re-running a tool of this operation can be assumed not to duplicate
/// an effect.
///
/// Exhaustive, so a new [`Operation`] fails to compile until it states which
/// it is — and the safe answer is the one it should have to argue against.
pub const fn is_retry_safe(op: Operation) -> bool {
    match op {
        // Pure reads: read, grep, glob, list_dir, repo_overview, and the LSP
        // family. Running one twice returns the same answer or a better one.
        Operation::Read => true,
        // Mutates the tree.
        Operation::Edit
        // A timed-out command may have committed anything. This is the
        // dirge-e31n.5 `Unknown` state and the whole reason for the gate.
        | Operation::Execute
        // A GET is idempotent and a POST is not, and nothing here can tell
        // them apart. Revisit once the side-effect taxonomy can.
        | Operation::Network
        // Unknown remote side effects by definition.
        | Operation::Mcp
        // Arbitrary file/network/shell code behind a Janet handler.
        | Operation::Plugin
        // Spawns a whole sub-agent run.
        | Operation::Agent
        // Writes rows.
        | Operation::Memory
        | Operation::Skill
        // Looks harmless — task_status and question are pure — but the same
        // operation carries `issue`, where a retried create makes two issues.
        // The operation is too coarse to split here, so it stays out.
        | Operation::Meta
        // Unknown tool. The one case where the default MUST be no.
        | Operation::Other => false,
    }
}

/// Whether a failed tool result should be retried.
///
/// `attempt` is 1-based: the value passed on the first failure is 1.
pub fn should_retry(tool_name: &str, is_error: bool, excerpt: &str, attempt: u32) -> bool {
    if !is_error || attempt >= MAX_ATTEMPTS {
        return false;
    }
    if !is_retry_safe(tool_operation(tool_name)) {
        return false;
    }
    classify(tool_name, excerpt) == ErrorClass::Transient
}

/// Backoff before the `attempt`-th retry (1-based), doubling each time.
pub fn backoff(attempt: u32) -> Duration {
    BASE_BACKOFF * 2u32.saturating_pow(attempt.saturating_sub(1).min(8))
}

/// Per-run retry counters, mirroring
/// [`super::tool_input_repair::RepairStats`] — one `Arc` on `LoopConfig`,
/// snapshotted into the tally at run end.
///
/// Two counters, not one, because they answer different questions.
/// `attempted` says the mechanism FIRED; without it a report showing no
/// change cannot distinguish "retrying doesn't help" from "nothing was ever
/// retried". `recovered` says it EARNED ITS KEEP: a retry that failed again
/// only spent latency.
#[derive(Debug, Default)]
pub struct RetryStats {
    attempted: std::sync::atomic::AtomicU64,
    recovered: std::sync::atomic::AtomicU64,
}

/// Immutable read of [`RetryStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetryStatsSnapshot {
    pub attempted: u64,
    pub recovered: u64,
}

impl RetryStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// One retry was issued.
    pub fn record_attempt(&self) {
        self.attempted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// A retry turned an errored result into a successful one.
    pub fn record_recovery(&self) {
        self.recovered
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RetryStatsSnapshot {
        RetryStatsSnapshot {
            attempted: self.attempted.load(std::sync::atomic::Ordering::Relaxed),
            recovered: self.recovered.load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: &str = "operation timed out after 5s";

    /// The point of the whole module: a read that timed out is retried.
    #[test]
    fn a_transient_read_failure_is_retried() {
        assert!(should_retry("read", true, TIMEOUT, 1));
        assert!(should_retry(
            "find_callers",
            true,
            "lsp request timed out",
            1
        ));
    }

    /// THE SAFETY PROPERTY. A timed-out `bash` may have already done the work,
    /// so it is never re-issued — however transient the error text looks.
    #[test]
    fn a_timed_out_bash_is_never_retried() {
        assert!(
            !should_retry("bash", true, "Command timed out after 120s", 1),
            "re-issuing a timed-out shell command can duplicate a committed effect"
        );
        // Same error text, same attempt number — only the tool differs. Without
        // this pair the test above would pass on a build that retried nothing.
        assert!(should_retry(
            "read",
            true,
            "Command timed out after 120s",
            1
        ));
    }

    /// Every mutating operation, stated one by one rather than trusting the
    /// match to have been written correctly.
    #[test]
    fn no_effectful_operation_is_retry_safe() {
        for op in [
            Operation::Edit,
            Operation::Execute,
            Operation::Network,
            Operation::Mcp,
            Operation::Plugin,
            Operation::Agent,
            Operation::Memory,
            Operation::Skill,
            Operation::Meta,
            Operation::Other,
        ] {
            assert!(!is_retry_safe(op), "{op:?} must not be retried");
        }
        assert!(is_retry_safe(Operation::Read));
    }

    /// An unknown tool name maps to `Operation::Other`, which must be refused.
    /// This is the fail-safe direction: a tool nobody classified is a tool
    /// nobody has argued is safe to run twice.
    #[test]
    fn an_unknown_tool_is_not_retried() {
        assert_eq!(tool_operation("some_mcp_thing"), Operation::Other);
        assert!(!should_retry("some_mcp_thing", true, TIMEOUT, 1));
    }

    /// Only `Transient` retries. A missing file will still be missing, and a
    /// malformed call will still be malformed.
    #[test]
    fn only_transient_failures_retry() {
        assert!(!should_retry("read", true, "No such file or directory", 1));
        assert!(!should_retry(
            "read",
            true,
            "invalid arguments: bad schema",
            1
        ));
        assert!(!should_retry("read", true, "make: *** Error 1", 1));
        // Control: the same tool DOES retry on transient text, so the three
        // above are being refused for their class and not for their tool.
        assert!(should_retry("read", true, TIMEOUT, 1));
    }

    #[test]
    fn a_success_is_never_retried() {
        assert!(!should_retry("read", false, TIMEOUT, 1));
    }

    /// The budget is spent, not unbounded.
    #[test]
    fn the_attempt_budget_is_bounded() {
        for attempt in 1..MAX_ATTEMPTS {
            assert!(
                should_retry("read", true, TIMEOUT, attempt),
                "attempt {attempt} is within budget"
            );
        }
        assert!(
            !should_retry("read", true, TIMEOUT, MAX_ATTEMPTS),
            "the {MAX_ATTEMPTS}th attempt exhausts the budget"
        );
        assert!(!should_retry("read", true, TIMEOUT, MAX_ATTEMPTS + 1));
    }

    /// Backoff grows and stays bounded — a mid-turn pause the user is waiting
    /// through must not become a hang.
    #[test]
    fn backoff_grows_and_stays_short() {
        assert_eq!(backoff(1), BASE_BACKOFF);
        assert_eq!(backoff(2), BASE_BACKOFF * 2);
        assert!(backoff(1) < backoff(2), "backoff must actually grow");
        // Across the whole real budget the added wait is well under a second.
        let total: Duration = (1..MAX_ATTEMPTS).map(backoff).sum();
        assert!(
            total < Duration::from_secs(2),
            "retries added {total:?} to a turn the user is waiting on"
        );
    }

    /// `backoff` must not panic or wrap on an attempt number far past the
    /// budget — cheap, since a caller bug would otherwise be an overflow.
    #[test]
    fn backoff_saturates_instead_of_overflowing() {
        let _ = backoff(0);
        let _ = backoff(u32::MAX);
    }
}
