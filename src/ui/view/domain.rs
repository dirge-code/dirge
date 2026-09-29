//! The view bounded context's values (L0). Pure data: no renderer, no
//! channel, no engine. Every other stratum of `ui::view` speaks these.
//!
//! Ubiquitous language:
//! - a **cell** is one tile of the swarm grid: an external panel or an
//!   in-process subagent;
//! - a **view event** is something the user did to the view (a view
//!   command, a grid key);
//! - the **view model** is what the view engine publishes after folding
//!   an event: the swarm grid's state and what the view owns;
//! - a **view effect** is something the UI must do because of an event
//!   (a notice, a producer reply, a side-panel mode, opening or
//!   messaging a subagent);
//! - a **view update** is one model plus its effects.

use serde::{Deserialize, Serialize};

/// One swarm-grid cell: an external panel (by panel id) or an
/// in-process subagent (by full task id). Selection is kept by cell, so
/// a producer refocus or a finishing sibling does not move it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum GridCell {
    Panel(String),
    Agent(String),
}

/// Something the user did to the view. Closed set: the engines and the
/// UI loop agree on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ViewEvent {
    /// First event; answers the initial model.
    Init,
    /// `/name args..` for a command the view owns.
    Command { name: String, args: Vec<String> },
    /// A grid key (see `promote::key_name`), with the cells the grid
    /// shows in paint order and its current column count.
    Grid {
        key: String,
        cells: Vec<GridCell>,
        columns: usize,
    },
}

impl ViewEvent {
    pub fn command(name: &str, args: &[&str]) -> Self {
        Self::Command {
            name: name.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }
}

/// The swarm grid while it is open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SwarmModel {
    /// Selected cell; `None` paints the first cell.
    #[serde(default)]
    pub selected: Option<GridCell>,
}

/// What the engine publishes after every event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ViewModel {
    /// `Some` while the swarm grid is open.
    #[serde(default)]
    pub swarm: Option<SwarmModel>,
    /// Key names the open grid consumes, sorted.
    #[serde(default)]
    pub grid_keys: Vec<String>,
    /// Slash command names (no slash) the view owns, sorted. They run
    /// whether or not the agent is busy.
    #[serde(default)]
    pub view_commands: Vec<String>,
}

impl ViewModel {
    pub fn swarm_open(&self) -> bool {
        self.swarm.is_some()
    }

    pub fn owns_command(&self, name: &str) -> bool {
        self.view_commands.iter().any(|c| c == name)
    }

    pub fn grid_consumes(&self, key: &str) -> bool {
        self.grid_keys.iter().any(|k| k == key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoticeLevel {
    Info,
    Error,
}

/// Which side panels a panel-mode effect sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelScope {
    Both,
    Right,
}

/// Something the UI does after an update. Closed set: each variant has
/// one interpreter in `boundary`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum ViewEffect {
    /// A line in the chat area.
    Notify { level: NoticeLevel, text: String },
    /// A reply to the external panel producer, by wire action name.
    Reply {
        action: String,
        #[serde(default)]
        target: Option<String>,
    },
    /// Set side-panel mode(s): `on`, `off`, `auto` or `debug`.
    PanelMode { scope: PanelScope, mode: String },
    /// Force each side panel on or off.
    Panes { left: bool, right: bool },
    /// Print the side panels' state (it depends on the terminal width,
    /// which only the renderer knows).
    PanelStatus,
    /// Print which panes are shown.
    DisplayStatus,
    /// Show this subagent's chat tab (full task id).
    OpenAgent { id: String },
    /// Start a `/msg` to this subagent in the editor (full task id).
    MessageAgent { id: String },
}

impl ViewEffect {
    pub fn notify(level: NoticeLevel, text: impl Into<String>) -> Self {
        Self::Notify {
            level,
            text: text.into(),
        }
    }
}

/// One answer from the engine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewUpdate {
    pub model: ViewModel,
    pub effects: Vec<ViewEffect>,
}
