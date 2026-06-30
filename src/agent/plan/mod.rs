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
//!   off-thread ([`runtime::spawn_review`]), and the live-workflow state carried
//!   across `Done` events ([`runtime::ActivePlan`] / [`runtime::PlanKickoff`]).
//!
//! Entry + wiring (outside this module): `ui/slash/cmd_plan.rs` runs the
//! explore→plan forks; the UI loop launches the streamed implement run; and
//! `ui/run_handlers/plan_review.rs` drives the reviewer loop after each turn.
//!
//! # Four work-tracking concepts — don't cross the wires
//!
//! dirge has four independently-scoped surfaces with overlapping vocabulary
//! ("plan", "task", "todo"). This is the canonical map; the sibling modules
//! link here. They share **no state**.
//!
//! | Concept | Where | What it is | Trigger | Lifetime | State |
//! |---------|-------|-----------|---------|----------|-------|
//! | **Phased `/plan` workflow** | `agent::plan` (this module) | explore→plan→implement→review for one complex request | user runs `/plan <req>` (needs `phased_workflow_enabled`) | one request | [`runtime::ActivePlan`] / [`runtime::PlanKickoff`] (ephemeral) |
//! | **Plan *mode*** | [`crate::agent::tools::plan`] (`plan_enter`/`plan_exit`) | a read-only session lock: the model proposes before touching anything | model calls `plan_enter`, or a prompt's `deny_tools` | until `plan_exit` | `PlanSwitchRequest` channel → session mode |
//! | **Todo list** | `crate::agent::tools::todo` (`write_todo_list`) | bulk planning over the persistent issue board — a todo IS an issue; the model is nudged to finish open ones | model calls `write_todo_list` (or the `issue` tool) | durable (in the `issues` table; session-scoped for panel/nudge) | the `issues` table; `TODO_LIST` is a render mirror |
//! | **Task / subagent** | `crate::agent::tools::task` (`task` + `task_status`) | spawn a background subagent for independent work | model calls `task` | per background job | `BackgroundStore` + abort registry |
//!
//! **Plan-mode × phased `/plan`:** orthogonal and composable. Plan-mode is a
//! read-only lock enforced at the *permission layer*; the phased workflow's
//! implement phase issues ordinary (writing) tool calls. So if plan-mode is
//! active, those writes are denied like any other write — the two don't share
//! code, they compose through the permission checker.

pub mod runtime;
pub mod workflow;
