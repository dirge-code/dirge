//! Swarm grid painter: every external panel and every in-flight
//! subagent at full size, one grid cell each, with a key-hint header
//! row. State and geometry are pure and live in `ui::swarm`; this
//! widget only paints them.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Style};
use ratatui::widgets::{Clear, Widget};

use crate::ui::panels_ext::ExternalPanels;
use crate::ui::swarm::{GRID_HINT, SwarmAgent, SwarmView, grid_geometry, swarm_cells};

use super::chat::crossterm_to_ratatui;
use super::panels::{SubPanel, ellipsize_width, subagent_preview_line};

/// Shown when the grid is open but there is nothing to paint.
const EMPTY_LINES: [&str; 2] = [
    "no external panels or subagents",
    "subagents appear here while they run; a panel feed paints panels (see docs/panel-feed.md)",
];

/// The swarm grid widget.
pub struct SwarmGrid<'a> {
    panels: &'a ExternalPanels,
    agents: &'a [SwarmAgent],
    view: &'a SwarmView,
    border: Style,
    accent: Style,
}

impl<'a> SwarmGrid<'a> {
    pub fn new(panels: &'a ExternalPanels, view: &'a SwarmView) -> Self {
        Self {
            panels,
            agents: &[],
            view,
            border: Style::default().fg(RColor::Green),
            accent: Style::default().fg(crossterm_to_ratatui(crate::ui::theme::accent())),
        }
    }

    /// In-flight subagents, painted after the external panels.
    pub fn agents(mut self, agents: &'a [SwarmAgent]) -> Self {
        self.agents = agents;
        self
    }

    /// Border style of unselected cells (the frame colour).
    pub fn border_style(mut self, style: Style) -> Self {
        self.border = style;
        self
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

impl<'a> Widget for SwarmGrid<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 4 || area.height < 3 {
            return;
        }
        Clear.render(area, buf);
        let all = self.panels.panels();
        let n_panels = all.len();
        let cells = swarm_cells(self.panels, self.agents.iter().map(|a| a.id.as_str()));
        let n = cells.len();
        let width = area.width as usize;

        let mut count = plural(n_panels, "panel", "panels");
        if !self.agents.is_empty() {
            count.push_str(" · ");
            count.push_str(&plural(self.agents.len(), "subagent", "subagents"));
        }
        let header = format!(" SWARM · {count} · {GRID_HINT}");
        buf.set_stringn(
            area.x,
            area.y,
            ellipsize_width(&header, width),
            width,
            self.accent,
        );

        let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        if n == 0 {
            let dim = Style::default().fg(RColor::DarkGray);
            let top = body.y + body.height / 2;
            for (i, text) in EMPTY_LINES.iter().enumerate() {
                let y = top + i as u16;
                if y >= body.bottom() {
                    break;
                }
                let text = ellipsize_width(text, width);
                let w = unicode_width::UnicodeWidthStr::width(text.as_str()) as u16;
                let x = body.x + body.width.saturating_sub(w) / 2;
                buf.set_stringn(x, y, &text, width, dim);
            }
            return;
        }

        let selected = self.view.selected_index(&cells);
        let geo = grid_geometry(n, body.width, body.height, selected);
        let cell_w = body.width / geo.cols;
        let cell_h = body.height / geo.rows;
        let focused = self.panels.focused();
        let shown = (geo.first..n).take(geo.per_page());
        for (slot, i) in shown.enumerate() {
            let (col, row) = (
                (slot % geo.cols as usize) as u16,
                (slot / geo.cols as usize) as u16,
            );
            // The last column / row absorbs the division remainder.
            let x = body.x + col * cell_w;
            let y = body.y + row * cell_h;
            let w = if col + 1 == geo.cols {
                body.right() - x
            } else {
                cell_w
            };
            let h = if row + 1 == geo.rows {
                body.bottom() - y
            } else {
                cell_h
            };
            let cell = Rect::new(x, y, w, h);
            let mut marker = String::new();
            if i == selected {
                marker.push('▸');
            }
            if i < n_panels && focused == Some(all[i].id.as_str()) {
                marker.push('●');
            }
            if !marker.is_empty() {
                marker.push(' ');
            }
            let style = if i == selected {
                self.accent
            } else {
                self.border
            };
            let rows = h.saturating_sub(2) as usize;
            if i < n_panels {
                let p = all[i];
                let badge = format!("{marker}{}/{n}", i + 1);
                let mut sub = SubPanel::new(&p.title)
                    .badge(Some(badge))
                    .border_style(style);
                if p.lines.is_empty() {
                    sub = sub.line("·", RColor::DarkGray);
                } else {
                    for l in p.visible_lines(rows) {
                        sub = sub.line(l.text.clone(), crossterm_to_ratatui(l.face.color()));
                    }
                }
                sub.render(cell, buf);
            } else {
                paint_agent(
                    &self.agents[i - n_panels],
                    &marker,
                    i,
                    n,
                    rows,
                    style,
                    cell,
                    buf,
                );
            }
        }
    }
}

/// One subagent cell: title = agent profile (or "subagent"), badge =
/// short id + position, body = the `[AGENTS]` preview line followed by
/// the newest lines of its chat tab.
#[allow(clippy::too_many_arguments)]
fn paint_agent(
    a: &SwarmAgent,
    marker: &str,
    i: usize,
    n: usize,
    rows: usize,
    style: Style,
    cell: Rect,
    buf: &mut Buffer,
) {
    let title = a.row.agent.as_deref().unwrap_or("subagent");
    let badge = format!("{marker}{} {}/{n}", a.row.id_short, i + 1);
    let mut sub = SubPanel::new(title).badge(Some(badge)).border_style(style);
    let mut room = rows;
    if let Some(preview) = subagent_preview_line(&a.row) {
        sub = sub.line(preview, RColor::DarkGray);
        room = room.saturating_sub(1);
    }
    let tail: Vec<_> = a
        .tail
        .iter()
        .filter(|(t, _)| !t.trim().is_empty())
        .collect();
    if tail.is_empty() && room == rows {
        sub = sub.line("·", RColor::DarkGray);
    }
    for (text, color) in &tail[tail.len().saturating_sub(room)..] {
        sub = sub.line(text.clone(), crossterm_to_ratatui(*color));
    }
    sub.render(cell, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::panels_ext::{PanelFace, PanelLine, PanelOp};

    fn show(s: &mut ExternalPanels, id: &str, title: &str, lines: &[&str]) {
        s.apply(PanelOp::Show {
            id: id.into(),
            title: title.into(),
            lines: lines
                .iter()
                .map(|l| PanelLine::new(*l, PanelFace::Normal))
                .collect(),
        });
    }

    fn paint(s: &ExternalPanels, v: &SwarmView, w: u16, h: u16) -> Vec<String> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        SwarmGrid::new(s, v).render(area, &mut buf);
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn paints_every_panel_in_its_own_cell() {
        let mut s = ExternalPanels::default();
        show(&mut s, "a", "Alpha", &["a-one", "a-two"]);
        show(&mut s, "b", "Beta", &["b-one"]);
        show(&mut s, "c", "Gamma", &[]);
        let v = SwarmView::new();
        let rows = paint(&s, &v, 60, 14);
        let text = rows.join("\n");
        assert!(rows[0].contains("SWARM · 3 panels"), "{text}");
        // 2 x 2 grid: Alpha and Beta share the first cell row.
        let row1 = &rows[1];
        assert!(row1.contains("Alpha") && row1.contains("Beta"), "{text}");
        assert!(row1.contains("▸ 1/3"), "selected cell is marked: {text}");
        assert!(row1.contains("2/3"), "{text}");
        assert!(text.contains("Gamma") && text.contains("3/3"), "{text}");
        for body in ["a-one", "a-two", "b-one"] {
            assert!(text.contains(body), "{body} missing: {text}");
        }
        // Beta's cell starts at column 30 (60 / 2).
        assert_eq!(rows[1].chars().nth(30), Some('╭'), "{text}");
    }

    #[test]
    fn producer_focus_is_marked_and_painted_first() {
        let mut s = ExternalPanels::default();
        show(&mut s, "a", "Alpha", &["1"]);
        s.apply(PanelOp::FocusTab {
            id: "log".into(),
            title: "Log".into(),
        });
        for i in 0..20 {
            s.apply(PanelOp::AppendTab {
                id: "log".into(),
                line: PanelLine::new(format!("line {i}"), PanelFace::Dim),
            });
        }
        let v = SwarmView::new();
        let rows = paint(&s, &v, 50, 8);
        let text = rows.join("\n");
        assert!(
            rows[1].contains("Log") && rows[1].contains("▸● 1/2"),
            "{text}"
        );
        // A log-style panel keeps its newest lines.
        assert!(text.contains("line 19"), "{text}");
        assert!(!text.contains("line 0 "), "{text}");
    }

    #[test]
    fn empty_grid_says_so() {
        let s = ExternalPanels::default();
        let v = SwarmView::new();
        let text = paint(&s, &v, 70, 8).join("\n");
        assert!(text.contains("SWARM · 0 panels"), "{text}");
        assert!(text.contains("no external panels or subagents"), "{text}");
    }

    #[test]
    fn tiny_area_paints_nothing_and_does_not_panic() {
        let mut s = ExternalPanels::default();
        show(&mut s, "a", "Alpha", &["x"]);
        let v = SwarmView::new();
        let rows = paint(&s, &v, 3, 2);
        assert!(rows.iter().all(|r| r.trim().is_empty()));
        // Narrow but valid: one column, header ellipsized.
        let rows = paint(&s, &v, 20, 6);
        assert!(rows[0].ends_with('…'), "{rows:?}");
    }

    fn agent(id: &str, name: Option<&str>, tail: &[&str]) -> SwarmAgent {
        SwarmAgent {
            id: id.into(),
            chat_idx: Some(1),
            row: crate::ui::panel_data::SubagentStatusRow {
                id_short: id.chars().take(6).collect(),
                agent: name.map(Into::into),
                activity: Some("read path=src/lib.rs".into()),
                tool_calls: 2,
                elapsed_secs: 7,
            },
            tail: tail
                .iter()
                .map(|t| (t.to_string(), crossterm::style::Color::White))
                .collect(),
        }
    }

    #[test]
    fn subagents_get_cells_after_the_panels() {
        let mut s = ExternalPanels::default();
        show(&mut s, "a", "Alpha", &["a-one"]);
        let agents = vec![
            agent(
                "abc123xyz",
                Some("architect"),
                &["old line", "<dirge> newest"],
            ),
            agent("zzz999", None, &[]),
        ];
        let mut v = SwarmView::new();
        let cells = swarm_cells(&s, agents.iter().map(|a| a.id.as_str()));
        v.select_index(&cells, 1);
        let area = Rect::new(0, 0, 80, 16);
        let mut buf = Buffer::empty(area);
        SwarmGrid::new(&s, &v)
            .agents(&agents)
            .render(area, &mut buf);
        let rows: Vec<String> = (0..16)
            .map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        let text = rows.join("\n");
        assert!(rows[0].contains("SWARM · 1 panel · 2 subagents"), "{text}");
        assert!(text.contains("architect"), "{text}");
        assert!(
            text.contains("▸ abc123 2/3"),
            "selected agent marked: {text}"
        );
        assert!(
            text.contains("↳ 7s · 2 tools · read path=src/lib.rs"),
            "{text}"
        );
        assert!(text.contains("<dirge> newest"), "{text}");
        assert!(
            text.contains("subagent") && text.contains("zzz999 3/3"),
            "{text}"
        );
    }
}
