//! Pipeline: the view reducer in Rust, the fallback for the cljrs
//! `dirge.view` engine (the parity tests hold both to the same answers).
//!
//! What the view owns is registered in two tables, [`COMMANDS`] and
//! [`GRID_KEYMAP`]; the model's `view_commands` and `grid_keys` are
//! derived from them. Adding a view command or a grid key is a table
//! row, never an edit to a match.

use super::domain::{
    GridCell, NoticeLevel, PanelScope, SwarmModel, ViewEffect, ViewEvent, ViewModel, ViewUpdate,
};
use super::port::Reducer;
use crate::extras::panel_feed::ReplyAction;
use crate::ui::renderer::parse_display_spec;
use crate::ui::swarm::SwarmCmd;

/// A view command: folds its arguments into the view state and names
/// the effects.
type Command = fn(&mut NativeReducer, &[&str]) -> Vec<ViewEffect>;

/// The view commands, by name (no slash).
const COMMANDS: &[(&str, Command)] = &[
    ("display", NativeReducer::display),
    ("panel", NativeReducer::panel),
    ("swarm", NativeReducer::swarm),
];

/// A cursor move in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Move {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

/// A verb that acts on the selected cell; what it does depends on the
/// cell's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellVerb {
    /// A panel: reply `focus`. A subagent: open its chat tab.
    Focus,
    /// A subagent: start a `/msg` to it. Nothing on a panel.
    Message,
}

/// What a grid key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridVerb {
    Close,
    /// Reply to the producer with this wire action.
    Reply(&'static str),
    OnCell(CellVerb),
    Move(Move),
    /// Select the cell at this index (paint order).
    Nth(usize),
}

/// The grid keymap, by key name (see `promote::key_name`).
const GRID_KEYMAP: &[(&str, GridVerb)] = &[
    ("Esc", GridVerb::Close),
    ("q", GridVerb::Close),
    ("Tab", GridVerb::Reply("next-tab")),
    ("BackTab", GridVerb::Reply("prev-tab")),
    ("r", GridVerb::Reply("refresh")),
    ("u", GridVerb::Reply("unfocus")),
    ("Enter", GridVerb::OnCell(CellVerb::Focus)),
    ("m", GridVerb::OnCell(CellVerb::Message)),
    ("Left", GridVerb::Move(Move::Left)),
    ("h", GridVerb::Move(Move::Left)),
    ("Right", GridVerb::Move(Move::Right)),
    ("l", GridVerb::Move(Move::Right)),
    ("Up", GridVerb::Move(Move::Up)),
    ("k", GridVerb::Move(Move::Up)),
    ("Down", GridVerb::Move(Move::Down)),
    ("j", GridVerb::Move(Move::Down)),
    ("Home", GridVerb::Move(Move::Home)),
    ("End", GridVerb::Move(Move::End)),
    ("1", GridVerb::Nth(0)),
    ("2", GridVerb::Nth(1)),
    ("3", GridVerb::Nth(2)),
    ("4", GridVerb::Nth(3)),
    ("5", GridVerb::Nth(4)),
    ("6", GridVerb::Nth(5)),
    ("7", GridVerb::Nth(6)),
    ("8", GridVerb::Nth(7)),
    ("9", GridVerb::Nth(8)),
];

/// The grid as a key sees it: cells in paint order, the cursor (the
/// selected cell, else the first) and the column count.
struct Grid<'a> {
    cells: &'a [GridCell],
    cur: usize,
    cols: usize,
}

impl<'a> Grid<'a> {
    fn new(cells: &'a [GridCell], selected: Option<&GridCell>, columns: usize) -> Self {
        let cur = selected
            .and_then(|sel| cells.iter().position(|c| c == sel))
            .unwrap_or(0);
        Self {
            cells,
            cur,
            cols: columns.max(1),
        }
    }

    fn last(&self) -> usize {
        self.cells.len().saturating_sub(1)
    }

    fn moved(&self, m: Move) -> usize {
        let (cur, last) = (self.cur, self.last());
        match m {
            Move::Left => cur.saturating_sub(1),
            Move::Right => (cur + 1).min(last),
            Move::Up => cur.saturating_sub(self.cols),
            Move::Down if cur + self.cols <= last => cur + self.cols,
            Move::Down => cur,
            Move::Home => 0,
            Move::End => last,
        }
    }

    /// The selected cell, `None` when the grid is empty.
    fn current(&self) -> Option<&'a GridCell> {
        self.cells.get(self.cur)
    }

    /// The cell at `index`, clamped to the last one; `None` when empty.
    fn cell_at(&self, index: usize) -> Option<&'a GridCell> {
        self.cells.get(index.min(self.last()))
    }
}

/// View state: `Some` while the swarm grid is open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeReducer {
    swarm: Option<SwarmModel>,
}

impl Reducer for NativeReducer {
    fn step(&mut self, event: &ViewEvent) -> Result<ViewUpdate, String> {
        let effects = match event {
            ViewEvent::Init => Vec::new(),
            ViewEvent::Command { name, args } => self.command(name, args),
            ViewEvent::Grid {
                key,
                cells,
                columns,
            } => self.grid(key, cells, *columns),
        };
        Ok(ViewUpdate {
            model: self.model(),
            effects,
        })
    }
}

fn sorted<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = names.map(str::to_string).collect();
    out.sort();
    out
}

fn notify(level: NoticeLevel, text: impl Into<String>) -> ViewEffect {
    ViewEffect::notify(level, text)
}

fn reply(action: &str, target: Option<String>) -> ViewEffect {
    ViewEffect::Reply {
        action: action.to_string(),
        target,
    }
}

impl NativeReducer {
    pub fn model(&self) -> ViewModel {
        ViewModel {
            swarm: self.swarm.clone(),
            grid_keys: sorted(GRID_KEYMAP.iter().map(|(k, _)| *k)),
            view_commands: sorted(COMMANDS.iter().map(|(n, _)| *n)),
        }
    }

    fn command(&mut self, name: &str, args: &[String]) -> Vec<ViewEffect> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match COMMANDS.iter().find(|(n, _)| *n == name) {
            Some((_, run)) => run(self, &args),
            None => vec![notify(
                NoticeLevel::Error,
                format!("not a view command: /{name}"),
            )],
        }
    }

    fn swarm(&mut self, args: &[&str]) -> Vec<ViewEffect> {
        let cmd = match SwarmCmd::parse(args) {
            Ok(cmd) => cmd,
            Err(usage) => return vec![notify(NoticeLevel::Error, usage)],
        };
        let open = self.swarm.is_some();
        let want = match cmd {
            SwarmCmd::Toggle => !open,
            SwarmCmd::Open => true,
            SwarmCmd::Close => false,
        };
        match (want, open) {
            (true, true) => Vec::new(),
            (true, false) => {
                self.swarm = Some(SwarmModel::default());
                Vec::new()
            }
            (false, _) => {
                self.swarm = None;
                vec![notify(NoticeLevel::Info, "swarm grid closed")]
            }
        }
    }

    fn panel(&mut self, args: &[&str]) -> Vec<ViewEffect> {
        let arg = args.first().map(|s| s.trim()).unwrap_or("");
        let mode = |scope, mode: &str| ViewEffect::PanelMode {
            scope,
            mode: mode.to_string(),
        };
        match arg {
            "" => vec![ViewEffect::PanelStatus],
            "on" | "off" | "auto" => vec![mode(PanelScope::Both, arg), ViewEffect::PanelStatus],
            "debug" => vec![mode(PanelScope::Right, "debug"), ViewEffect::PanelStatus],
            _ => match ReplyAction::parse(args) {
                Ok(action) => {
                    let target = match &action {
                        ReplyAction::Focus(id) => Some(id.clone()),
                        _ => None,
                    };
                    vec![
                        reply(action.name(), target),
                        notify(
                            NoticeLevel::Info,
                            format!("panel reply '{}' requested", action.name()),
                        ),
                    ]
                }
                Err(usage) => vec![notify(
                    NoticeLevel::Error,
                    format!("{usage} (display modes: on|off|auto|debug)"),
                )],
            },
        }
    }

    fn display(&mut self, args: &[&str]) -> Vec<ViewEffect> {
        let spec = args.join(" ");
        if spec.trim().is_empty() {
            return vec![ViewEffect::DisplayStatus];
        }
        match parse_display_spec(&spec) {
            Ok(vis) => {
                let mut shown = vec!["main"];
                if vis.left {
                    shown.insert(0, "left");
                }
                if vis.right {
                    shown.push("right");
                }
                vec![
                    ViewEffect::Panes {
                        left: vis.left,
                        right: vis.right,
                    },
                    notify(NoticeLevel::Info, format!("display: {}", shown.join("|"))),
                ]
            }
            Err(msg) => vec![notify(NoticeLevel::Error, msg)],
        }
    }

    fn grid(&mut self, key: &str, cells: &[GridCell], columns: usize) -> Vec<ViewEffect> {
        let Some(swarm) = self.swarm.as_ref() else {
            return Vec::new();
        };
        let Some(verb) = GRID_KEYMAP.iter().find(|(k, _)| *k == key).map(|(_, v)| *v) else {
            return Vec::new();
        };
        let grid = Grid::new(cells, swarm.selected.as_ref(), columns);
        match verb {
            GridVerb::Close => {
                self.swarm = None;
                Vec::new()
            }
            GridVerb::Reply(action) => vec![reply(action, None)],
            GridVerb::OnCell(verb) => self.on_cell(verb, grid.current()),
            GridVerb::Move(m) => {
                self.select(grid.cell_at(grid.moved(m)));
                Vec::new()
            }
            GridVerb::Nth(i) => {
                if i < cells.len() {
                    self.select(grid.cell_at(i));
                }
                Vec::new()
            }
        }
    }

    /// `verb` on the selected cell. Opening or messaging a subagent
    /// leaves the grid, so it closes it.
    fn on_cell(&mut self, verb: CellVerb, cell: Option<&GridCell>) -> Vec<ViewEffect> {
        match (verb, cell) {
            (CellVerb::Focus, Some(GridCell::Panel(id))) => vec![reply("focus", Some(id.clone()))],
            (CellVerb::Focus, Some(GridCell::Agent(id))) => {
                self.swarm = None;
                vec![ViewEffect::OpenAgent { id: id.clone() }]
            }
            (CellVerb::Message, Some(GridCell::Agent(id))) => {
                self.swarm = None;
                vec![ViewEffect::MessageAgent { id: id.clone() }]
            }
            _ => Vec::new(),
        }
    }

    fn select(&mut self, cell: Option<&GridCell>) {
        if let Some(cell) = cell {
            self.swarm = Some(SwarmModel {
                selected: Some(cell.clone()),
            });
        }
    }
}
