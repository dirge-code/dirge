//! `dirge.harness` output in the TUI: `notify` as a chat-area line, `panel`
//! as a box in the left side panel.

use crate::ui::notifications::{Notification, notify_send};
use crate::ui::panels_ext::{self, PanelFace, PanelLine, PanelOp};

use super::domain::PanelRequest;
use super::port::{HarnessSink, Level, PanelSink};

/// Width markdown panel bodies are wrapped at; the painter clips anything
/// wider to the panel.
const PANEL_WIDTH: usize = 40;

pub struct TuiSink;

impl HarnessSink for TuiSink {
    fn notify(&self, level: Level, message: &str) {
        let line = format!("[addon] {message}");
        notify_send(match level {
            Level::Info => Notification::Info(line),
            Level::Warn => Notification::Warn(line),
            Level::Error => Notification::Error(line),
        });
    }
}

impl PanelSink for TuiSink {
    fn panel(&self, request: PanelRequest) -> bool {
        panels_ext::panel_send(panel_op(request))
    }
}

/// The panel channel's op for an addon's request.
fn panel_op(request: PanelRequest) -> PanelOp {
    let line = |text: String, face: &str| PanelLine::new(text, PanelFace::from_name(face));
    match request {
        PanelRequest::Show { id, title, lines } => PanelOp::Show {
            id,
            title,
            lines: lines
                .into_iter()
                .map(|(text, face)| line(text, &face))
                .collect(),
        },
        PanelRequest::Markdown {
            id,
            title,
            markdown,
        } => PanelOp::Show {
            id,
            title,
            lines: panels_ext::lines_from_markdown(&markdown, PANEL_WIDTH),
        },
        PanelRequest::Append { id, text, face } => PanelOp::AppendTab {
            id,
            line: line(text, &face),
        },
        PanelRequest::Focus { id, title } => PanelOp::FocusTab { id, title },
        PanelRequest::Close { id } => PanelOp::Close { id },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_map_onto_panel_ops_with_named_faces() {
        let op = panel_op(PanelRequest::Show {
            id: "swarm".into(),
            title: "Swarm".into(),
            lines: vec![("3 running".into(), "success".into())],
        });
        assert_eq!(
            op,
            PanelOp::Show {
                id: "swarm".into(),
                title: "Swarm".into(),
                lines: vec![PanelLine::new("3 running", PanelFace::Success)],
            }
        );
        assert_eq!(
            panel_op(PanelRequest::Append {
                id: "log".into(),
                text: "x".into(),
                face: "warn".into()
            }),
            PanelOp::AppendTab {
                id: "log".into(),
                line: PanelLine::new("x", PanelFace::Warn)
            }
        );
        assert_eq!(
            panel_op(PanelRequest::Close { id: "log".into() }),
            PanelOp::Close { id: "log".into() }
        );
    }

    #[test]
    fn markdown_bodies_become_plain_lines() {
        let PanelOp::Show { lines, .. } = panel_op(PanelRequest::Markdown {
            id: "k".into(),
            title: "Kanban".into(),
            markdown: "- **todo** 16".into(),
        }) else {
            panic!("markdown is a show");
        };
        assert!(lines.iter().any(|l| l.text.contains("todo")));
        assert!(lines.iter().all(|l| !l.text.contains('\u{1b}')));
    }
}
