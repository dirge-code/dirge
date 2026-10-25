//! The view reducer's behaviour, through the [`Reducer`] port. The
//! native reducer is the reference; `addons::cljrs::view_isolate` runs
//! the same scripts against `dirge.view` for parity.

use super::domain::{
    GridCell, NoticeLevel, PanelScope, SwarmModel, ViewEffect, ViewEvent, ViewUpdate,
};
use super::native::NativeReducer;
use super::port::Reducer;
use super::*;

fn panel(id: &str) -> GridCell {
    GridCell::Panel(id.into())
}

fn agent(id: &str) -> GridCell {
    GridCell::Agent(id.into())
}

/// A grid key over panel cells `ids`.
fn grid(key: &str, ids: &[&str], columns: usize) -> ViewEvent {
    grid_over(key, ids.iter().map(|id| panel(id)).collect(), columns)
}

fn grid_over(key: &str, cells: Vec<GridCell>, columns: usize) -> ViewEvent {
    ViewEvent::Grid {
        key: key.into(),
        cells,
        columns,
    }
}

fn cmd(name: &str, args: &[&str]) -> ViewEvent {
    ViewEvent::command(name, args)
}

fn info(text: &str) -> ViewEffect {
    ViewEffect::notify(NoticeLevel::Info, text)
}

fn run(r: &mut impl Reducer, event: ViewEvent) -> ViewUpdate {
    r.step(&event).unwrap()
}

fn selected(u: &ViewUpdate) -> Option<Option<GridCell>> {
    u.model.swarm.as_ref().map(|s| s.selected.clone())
}

/// Events every reducer must answer alike, covering each command, each
/// grid verb on each kind of cell, and the refusals. Shared with the
/// cljrs parity test.
pub(crate) fn parity_script() -> Vec<ViewEvent> {
    let ids = ["a", "b", "c", "d", "e"];
    let mixed = || vec![panel("a"), agent("t1"), agent("t2")];
    vec![
        ViewEvent::Init,
        cmd("swarm", &[]),
        cmd("swarm", &["on"]),
        grid("Right", &ids, 3),
        grid("Down", &ids, 3),
        grid("Down", &ids, 3),
        grid("Up", &ids, 3),
        grid("End", &ids, 3),
        grid("Home", &ids, 3),
        grid("3", &ids, 3),
        grid("9", &ids, 3),
        grid("l", &ids, 3),
        grid("Enter", &ids, 3),
        grid("m", &ids, 3),
        grid("Tab", &ids, 3),
        grid("BackTab", &ids, 3),
        grid("r", &ids, 3),
        grid("u", &ids, 3),
        grid("z", &ids, 3),
        grid("Enter", &[], 1),
        grid("m", &[], 1),
        grid("Right", &[], 1),
        grid("Enter", &["x", "y"], 0),
        grid_over("2", mixed(), 2),
        grid_over("Right", mixed(), 2),
        grid_over("m", mixed(), 2),
        cmd("swarm", &["on"]),
        grid_over("End", mixed(), 2),
        grid_over("Enter", mixed(), 2),
        grid("Tab", &ids, 3),
        cmd("swarm", &["on"]),
        grid("Esc", &ids, 3),
        cmd("swarm", &["off"]),
        cmd("swarm", &["toggle"]),
        grid("q", &ids, 3),
        cmd("swarm", &["sideways"]),
        cmd("swarm", &["on", "now"]),
        cmd("panel", &[]),
        cmd("panel", &["on"]),
        cmd("panel", &["off"]),
        cmd("panel", &["auto"]),
        cmd("panel", &["debug"]),
        cmd("panel", &["next"]),
        cmd("panel", &["prev-tab"]),
        cmd("panel", &["refresh"]),
        cmd("panel", &["unfocus"]),
        cmd("panel", &["focus", "a1"]),
        cmd("panel", &["focus"]),
        cmd("panel", &["focus", "a", "b"]),
        cmd("panel", &["next", "x"]),
        cmd("panel", &["warp"]),
        cmd("display", &[]),
        cmd("display", &["left|main|right"]),
        cmd("display", &["MAIN,", "right"]),
        cmd("display", &["main"]),
        cmd("display", &["center"]),
        cmd("display", &["|"]),
        cmd("quit", &[]),
    ]
}

#[test]
fn the_model_publishes_what_the_view_owns() {
    let u = run(&mut NativeReducer::default(), ViewEvent::Init);
    assert_eq!(u.model.view_commands, ["display", "panel", "swarm"]);
    assert!(u.model.grid_consumes("BackTab") && u.model.grid_consumes("9"));
    assert!(u.model.grid_consumes("m"));
    assert!(!u.model.grid_consumes("x"));
    let mut sorted = u.model.grid_keys.clone();
    sorted.sort();
    assert_eq!(u.model.grid_keys, sorted);
    assert!(u.effects.is_empty());
}

#[test]
fn swarm_opens_toggles_and_closes() {
    let mut r = NativeReducer::default();
    let u = run(&mut r, cmd("swarm", &[]));
    assert_eq!(selected(&u), Some(None));
    assert!(u.effects.is_empty());
    // Opening again is a no-op.
    assert!(run(&mut r, cmd("swarm", &["on"])).effects.is_empty());
    let u = run(&mut r, cmd("swarm", &[]));
    assert_eq!(selected(&u), None);
    assert_eq!(u.effects, vec![info("swarm grid closed")]);
    let u = run(&mut r, cmd("swarm", &["sideways"]));
    assert!(matches!(
        &u.effects[0],
        ViewEffect::Notify { level: NoticeLevel::Error, text } if text.contains("usage: /swarm")
    ));
}

#[test]
fn grid_keys_move_the_selection_by_cell() {
    let mut r = NativeReducer::default();
    let ids = ["a", "b", "c", "d", "e"];
    run(&mut r, cmd("swarm", &["on"]));
    let mut key = |k: &str| selected(&run(&mut r, grid(k, &ids, 3)));
    assert_eq!(key("Right"), Some(Some(panel("b"))));
    assert_eq!(key("Down"), Some(Some(panel("e"))));
    // No cell below the last row: stay put.
    assert_eq!(key("Down"), Some(Some(panel("e"))));
    assert_eq!(key("Up"), Some(Some(panel("b"))));
    assert_eq!(key("3"), Some(Some(panel("c"))));
    // Out of range digits change nothing.
    assert_eq!(key("9"), Some(Some(panel("c"))));
    // A producer reorder keeps the highlight on the same cell.
    let u = run(&mut r, grid("Enter", &["c", "a", "b"], 3));
    assert_eq!(
        u.effects,
        vec![ViewEffect::Reply {
            action: "focus".into(),
            target: Some("c".into())
        }]
    );
    // A vanished selection falls back to the first cell.
    let u = run(&mut r, grid("Enter", &["x"], 1));
    assert_eq!(
        u.effects,
        vec![ViewEffect::Reply {
            action: "focus".into(),
            target: Some("x".into())
        }]
    );
    let u = run(&mut r, grid("Esc", &ids, 3));
    assert_eq!(selected(&u), None);
}

#[test]
fn agent_cells_open_and_message_and_leave_the_grid() {
    let mut r = NativeReducer::default();
    let cells = || vec![panel("a"), agent("t1")];
    run(&mut r, cmd("swarm", &["on"]));
    // `m` means nothing on a panel cell.
    assert!(run(&mut r, grid_over("m", cells(), 2)).effects.is_empty());
    assert_eq!(
        selected(&run(&mut r, grid_over("2", cells(), 2))),
        Some(Some(agent("t1")))
    );
    let u = run(&mut r, grid_over("m", cells(), 2));
    assert_eq!(
        u.effects,
        vec![ViewEffect::MessageAgent { id: "t1".into() }]
    );
    assert_eq!(selected(&u), None);
    run(&mut r, cmd("swarm", &["on"]));
    run(&mut r, grid_over("End", cells(), 2));
    let u = run(&mut r, grid_over("Enter", cells(), 2));
    assert_eq!(u.effects, vec![ViewEffect::OpenAgent { id: "t1".into() }]);
    assert_eq!(selected(&u), None);
}

#[test]
fn agent_selection_survives_a_sibling_finishing() {
    let mut r = NativeReducer::default();
    run(&mut r, cmd("swarm", &["on"]));
    run(
        &mut r,
        grid_over("End", vec![panel("a"), agent("t1"), agent("t2")], 2),
    );
    // t1 finishes; Enter still opens t2.
    let u = run(&mut r, grid_over("Enter", vec![panel("a"), agent("t2")], 2));
    assert_eq!(u.effects, vec![ViewEffect::OpenAgent { id: "t2".into() }]);
}

#[test]
fn grid_keys_do_nothing_while_closed_or_empty() {
    let mut r = NativeReducer::default();
    assert!(run(&mut r, grid("Tab", &["a"], 1)).effects.is_empty());
    run(&mut r, cmd("swarm", &["on"]));
    let u = run(&mut r, grid("Right", &[], 1));
    assert_eq!(selected(&u), Some(None));
    assert!(run(&mut r, grid("Enter", &[], 1)).effects.is_empty());
    // Replies still go out on an empty grid.
    assert_eq!(
        run(&mut r, grid("r", &[], 1)).effects,
        vec![ViewEffect::Reply {
            action: "refresh".into(),
            target: None
        }]
    );
}

#[test]
fn panel_sets_modes_and_replies() {
    let mut r = NativeReducer::default();
    assert_eq!(
        run(&mut r, cmd("panel", &[])).effects,
        vec![ViewEffect::PanelStatus]
    );
    assert_eq!(
        run(&mut r, cmd("panel", &["debug"])).effects,
        vec![
            ViewEffect::PanelMode {
                scope: PanelScope::Right,
                mode: "debug".into()
            },
            ViewEffect::PanelStatus
        ]
    );
    assert_eq!(
        run(&mut r, cmd("panel", &["focus", "a1"])).effects,
        vec![
            ViewEffect::Reply {
                action: "focus".into(),
                target: Some("a1".into())
            },
            info("panel reply 'focus' requested")
        ]
    );
    let u = run(&mut r, cmd("panel", &["warp"]));
    assert!(matches!(
        &u.effects[0],
        ViewEffect::Notify { level: NoticeLevel::Error, text } if text.contains("display modes")
    ));
}

#[test]
fn display_forces_panes() {
    let mut r = NativeReducer::default();
    assert_eq!(
        run(&mut r, cmd("display", &[])).effects,
        vec![ViewEffect::DisplayStatus]
    );
    assert_eq!(
        run(&mut r, cmd("display", &["main", "right"])).effects,
        vec![
            ViewEffect::Panes {
                left: false,
                right: true
            },
            info("display: main|right")
        ]
    );
}

#[test]
fn unknown_commands_are_refused_not_run() {
    let u = run(&mut NativeReducer::default(), cmd("quit", &[]));
    assert!(matches!(
        &u.effects[0],
        ViewEffect::Notify { level: NoticeLevel::Error, text } if text == "not a view command: /quit"
    ));
}

#[test]
fn the_native_script_runs_clean() {
    let mut r = NativeReducer::default();
    for event in parity_script() {
        r.step(&event).unwrap();
    }
    assert_eq!(r.model().swarm, None::<SwarmModel>);
}

#[test]
fn a_wanted_engine_goes_first() {
    fn a(_: port::UpdateSink) -> Result<(engine::ThreadEngine, ViewModel), String> {
        Err("a".into())
    }
    fn b(_: port::UpdateSink) -> Result<(engine::ThreadEngine, ViewModel), String> {
        Err("b".into())
    }
    let all: Vec<(&'static str, EngineFactory)> = vec![("a", a), ("b", b)];
    let names =
        |v: Vec<(&'static str, EngineFactory)>| v.into_iter().map(|(n, _)| n).collect::<Vec<_>>();
    assert_eq!(names(preferred(all.clone(), Some("b"))), ["b", "a"]);
    assert_eq!(names(preferred(all.clone(), Some("zzz"))), ["a", "b"]);
    assert_eq!(names(preferred(all, None)), ["a", "b"]);
}
