//! Swarm view: a full-screen grid of the external panels and of
//! dirge's own in-flight subagents.
//!
//! The left side panel shows external panels as compact boxes and
//! subagents as `[AGENTS]` rows; the swarm view paints both at full
//! size, one grid cell each, above the input strip. External panels
//! come first (the producer's focused one leads), then the subagents
//! in spawn order. A panel cell repaints the latest frame its producer
//! sent; a subagent cell shows the `[AGENTS]` preview line and the
//! tail of the subagent's chat tab.
//!
//! This module is the pure half the painter needs: the cell list
//! ([`swarm_cells`]), the selection it paints ([`SwarmView`]), `/swarm`
//! argument parsing ([`SwarmCmd::parse`]) and the grid geometry
//! ([`grid_geometry`]). The grid's state and keys belong to the view
//! seam (`ui::view`); the painter lives in `ui::tui::swarm`.

use crate::ui::panels_ext::ExternalPanels;

/// A grid cell, as the view seam's domain names it.
pub use crate::ui::view::domain::GridCell as SwarmCell;

/// Narrowest grid cell (columns) before the grid drops a column.
pub const MIN_CELL_W: u16 = 24;
/// Shortest grid cell (rows: two borders plus three body rows).
pub const MIN_CELL_H: u16 = 5;

/// Key hint painted in the grid's header row.
pub const GRID_HINT: &str = "arrows/1-9 select · Enter focus/open · m msg agent · Tab/S-Tab view · r refresh · u unfocus · Esc close";

/// The grid's cells in paint order: external panels (as the producer
/// orders them), then subagents (spawn order).
pub fn swarm_cells<'a>(
    panels: &ExternalPanels,
    agent_ids: impl IntoIterator<Item = &'a str>,
) -> Vec<SwarmCell> {
    panels
        .panels()
        .iter()
        .map(|p| SwarmCell::Panel(p.id.clone()))
        .chain(
            agent_ids
                .into_iter()
                .map(|id| SwarmCell::Agent(id.to_string())),
        )
        .collect()
}

/// A subagent as the grid paints it: its `[AGENTS]` row, the chat tab it
/// streams into, and that tab's newest lines (filled by the renderer
/// just before a paint, empty otherwise).
#[derive(Debug, Clone, Default)]
pub struct SwarmAgent {
    /// Full task id (the `/msg` and chat-map key).
    pub id: String,
    /// Index of the subagent's chat tab, when it has one.
    pub chat_idx: Option<usize>,
    pub row: crate::ui::panel_data::SubagentStatusRow,
    /// Newest chat-tab lines, oldest first.
    pub tail: Vec<(String, crossterm::style::Color)>,
}

/// The selection the open grid paints, mirrored from the view model.
/// Kept by cell identity so a producer that reorders its panels, or a
/// subagent that finishes, does not move the highlight.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SwarmView {
    selected: Option<SwarmCell>,
}

impl SwarmView {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// The grid with `selected` highlighted (`None`: the first cell).
    pub fn selecting(selected: Option<SwarmCell>) -> Self {
        Self { selected }
    }

    /// Index of the selected cell in paint order, falling back to the
    /// first cell when the selection is gone.
    pub fn selected_index(&self, cells: &[SwarmCell]) -> usize {
        self.selected
            .as_ref()
            .and_then(|sel| cells.iter().position(|c| c == sel))
            .unwrap_or(0)
    }

    /// Select the cell at `index` in paint order (clamped).
    #[cfg(test)]
    pub fn select_index(&mut self, cells: &[SwarmCell], index: usize) {
        self.selected = cells.get(index.min(cells.len().saturating_sub(1))).cloned();
    }
}

/// `/swarm` arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwarmCmd {
    Toggle,
    Open,
    Close,
}

/// Usage line for `/swarm`.
pub const SWARM_USAGE: &str = "usage: /swarm [on|off]";

impl SwarmCmd {
    /// Parse the words after `/swarm` (pure). `Err` carries the
    /// user-facing usage message.
    pub fn parse(args: &[&str]) -> Result<Self, String> {
        match args {
            [] => Ok(Self::Toggle),
            [one] => match one.trim() {
                "" | "toggle" => Ok(Self::Toggle),
                "on" | "open" | "show" => Ok(Self::Open),
                "off" | "close" | "hide" => Ok(Self::Close),
                other => Err(format!("unknown /swarm argument '{other}' ({SWARM_USAGE})")),
            },
            _ => Err(format!("/swarm takes at most one argument ({SWARM_USAGE})")),
        }
    }
}

/// Grid geometry for `n` cells in a `width` x `height` region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridGeometry {
    /// Columns of cells.
    pub cols: u16,
    /// Rows of cells shown at once.
    pub rows: u16,
    /// Index (paint order) of the first panel shown: the page that
    /// holds the selected panel.
    pub first: usize,
}

impl GridGeometry {
    /// Cells shown at once.
    pub fn per_page(&self) -> usize {
        self.cols as usize * self.rows as usize
    }
}

/// Lay out `n` cells in a `width` x `height` region (pure). The grid
/// is as square as the cell minimums allow (`ceil(sqrt(n))` columns);
/// when not every panel fits, the page holding `selected` is shown.
pub fn grid_geometry(n: usize, width: u16, height: u16, selected: usize) -> GridGeometry {
    let n = n.max(1);
    let max_cols = (width / MIN_CELL_W).max(1) as usize;
    let mut cols = 1usize;
    while cols * cols < n {
        cols += 1;
    }
    let cols = cols.min(max_cols).min(n);
    let want_rows = n.div_ceil(cols);
    let max_rows = (height / MIN_CELL_H).max(1) as usize;
    let rows = want_rows.min(max_rows);
    let per_page = cols * rows;
    let first = (selected.min(n - 1) / per_page) * per_page;
    GridGeometry {
        cols: cols as u16,
        rows: rows as u16,
        first,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::panels_ext::{PanelFace, PanelLine, PanelOp};

    fn panels(ids: &[&str]) -> ExternalPanels {
        let mut s = ExternalPanels::default();
        for id in ids {
            s.apply(PanelOp::Show {
                id: (*id).into(),
                title: id.to_uppercase(),
                lines: vec![PanelLine::new("x", PanelFace::Normal)],
            });
        }
        s
    }

    #[test]
    fn swarm_args_parse() {
        assert_eq!(SwarmCmd::parse(&[]), Ok(SwarmCmd::Toggle));
        assert_eq!(SwarmCmd::parse(&["on"]), Ok(SwarmCmd::Open));
        assert_eq!(SwarmCmd::parse(&["off"]), Ok(SwarmCmd::Close));
        assert_eq!(SwarmCmd::parse(&["toggle"]), Ok(SwarmCmd::Toggle));
        let bad = SwarmCmd::parse(&["sideways"]).unwrap_err();
        assert!(bad.contains("usage: /swarm"), "{bad}");
        let extra = SwarmCmd::parse(&["on", "now"]).unwrap_err();
        assert!(extra.contains("at most one"), "{extra}");
    }

    #[test]
    fn cells_list_panels_then_agents() {
        let c = swarm_cells(&panels(&["a"]), ["t1", "t2"]);
        assert_eq!(
            c,
            vec![
                SwarmCell::Panel("a".into()),
                SwarmCell::Agent("t1".into()),
                SwarmCell::Agent("t2".into()),
            ]
        );
    }

    #[test]
    fn the_painted_selection_follows_the_cell() {
        let mut s = panels(&["a", "b", "c"]);
        assert_eq!(SwarmView::new().selected_index(&swarm_cells(&s, [])), 0);
        let v = SwarmView::selecting(Some(SwarmCell::Panel("c".into())));
        assert_eq!(v.selected_index(&swarm_cells(&s, [])), 2);
        // A producer focus moves `c` first; the highlight stays on it.
        s.apply(PanelOp::FocusTab {
            id: "c".into(),
            title: "C".into(),
        });
        assert_eq!(v.selected_index(&swarm_cells(&s, [])), 0);
        // A closed panel falls back to the first cell.
        s.apply(PanelOp::Close { id: "c".into() });
        assert_eq!(v.selected_index(&swarm_cells(&s, [])), 0);
    }

    #[test]
    fn agent_selection_survives_a_sibling_finishing() {
        let s = panels(&["a"]);
        let v = SwarmView::selecting(Some(SwarmCell::Agent("t2".into())));
        assert_eq!(v.selected_index(&swarm_cells(&s, ["t1", "t2"])), 2);
        // t1 completes and drops out; the highlight stays on t2.
        assert_eq!(v.selected_index(&swarm_cells(&s, ["t2"])), 1);
    }

    #[test]
    fn geometry_is_square_ish_and_pages() {
        // Three panels on a roomy screen: 2 x 2, all shown.
        let g = grid_geometry(3, 200, 50, 0);
        assert_eq!((g.cols, g.rows, g.first), (2, 2, 0));
        // One panel fills the region.
        assert_eq!(grid_geometry(1, 80, 20, 0).cols, 1);
        // Narrow: one column; short: two rows per page.
        let g = grid_geometry(6, 30, 10, 0);
        assert_eq!((g.cols, g.rows), (1, 2));
        assert_eq!(g.per_page(), 2);
        // The page holding the selection is shown.
        assert_eq!(grid_geometry(6, 30, 10, 5).first, 4);
        assert_eq!(grid_geometry(6, 30, 10, 99).first, 4);
        // Nothing to show still yields a 1 x 1 grid.
        let g = grid_geometry(0, 10, 3, 0);
        assert_eq!((g.cols, g.rows, g.first), (1, 1, 0));
    }
}
