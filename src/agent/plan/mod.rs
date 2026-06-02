//! Phased `/plan` workflow (vix port): explore → plan → implement → review.
//!
//! An opt-in, per-task command (gated by `phased_workflow_enabled`) that splits
//! a complex request into context-isolated phases. The pieces:
//!
//! - [`workflow`] — **pure logic, no runtime**: the phase prompts + tool
//!   allow-lists, the machine-parsed reviewer verdict ([`workflow::parse_review_verdict`]),
//!   and the per-step review policy ([`workflow::next_review_step`]). Unit-tested
//!   without a model.
//! - [`runtime`] — **the runtime glue**: drain a forked phase runner to text
//!   ([`runtime::collect_runner_text`]), fork a write-disabled reviewer
//!   ([`runtime::review_once`]), and the live-workflow state carried across
//!   `Done` events ([`runtime::ActivePlan`] / [`runtime::PlanKickoff`]).
//!
//! Entry + wiring (outside this module): `ui/slash/cmd_plan.rs` runs the
//! explore→plan forks; the UI loop launches the streamed implement run; and
//! `ui/run_handlers/plan_review.rs` drives the reviewer loop after each turn.
//!
//! NOTE: distinct from plan-**mode** (`agent::tools::plan`, the
//! `plan_enter`/`plan_exit` read-only lock) — unrelated feature, similar name.

pub mod runtime;
pub mod workflow;
