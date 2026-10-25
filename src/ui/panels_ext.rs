//! External panels: boxes in the left side panel whose content is
//! driven by code outside the agent loop (an extension, a feed, a
//! background integration).
//!
//! Three layers, kept separate so each is testable on its own:
//!
//! 1. **Channel boundary** — a process-global bounded sender
//!    ([`panel_send`]) callable from any thread, drained by the UI
//!    loop through [`take_receiver`]. Same shape as
//!    `ui::notifications`: `try_send` drops on overflow so a runaway
//!    producer can't grow memory or starve the UI.
//! 2. **Pure reducer** — [`ExternalPanels::apply`] folds one
//!    [`PanelOp`] into the panel state. No I/O, no globals; it also
//!    sanitises producer text and enforces every bound (panel count,
//!    line count, line width).
//! 3. **Painter** — `ui::tui::panels` paints the state as sub-panels
//!    above the AGENTS box (see [`ExternalPanels::panels`]).
//!
//! Lines arrive pre-rendered as `{text, face}`: the producer decides
//! the wording, dirge decides the colours via [`PanelFace`].

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use crossterm::style::Color;
use tokio::sync::mpsc::{Receiver, Sender, channel};

#[allow(unused_imports)]
use crate::sync_util::LockExt;

/// Channel capacity. Producers that outrun the UI lose ops rather
/// than queueing unboundedly.
const PANEL_CHAN_CAP: usize = 1024;
/// Maximum number of panels kept at once. A `Show` / `FocusTab` for a
/// new id beyond this evicts the oldest panel.
pub const MAX_PANELS: usize = 16;
/// Maximum body lines per panel. `Show` truncates; `AppendTab` drops
/// the oldest line.
pub const MAX_PANEL_LINES: usize = 200;
/// Maximum characters per line / title / id after sanitising.
pub const MAX_LINE_CHARS: usize = 512;

/// Visual treatment of one panel line. Named faces map onto the
/// panel palette at paint time; `Fixed` carries an explicit colour
/// (used for markdown-rendered bodies, which already chose one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelFace {
    #[default]
    Normal,
    Dim,
    Accent,
    Success,
    Warn,
    Error,
    Fixed(Color),
}

impl PanelFace {
    /// Map a wire-level face name onto a face. Unknown names fall
    /// back to `Normal` so a newer producer never breaks rendering.
    #[allow(dead_code)] // consumed by external producers (panel feeds)
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "dim" | "muted" | "comment" => Self::Dim,
            "accent" | "info" | "highlight" | "title" => Self::Accent,
            "success" | "ok" | "good" => Self::Success,
            "warn" | "warning" => Self::Warn,
            "error" | "err" | "bad" => Self::Error,
            _ => Self::Normal,
        }
    }

    /// The terminal colour this face paints with.
    pub fn color(self) -> Color {
        match self {
            Self::Normal => Color::Green,
            Self::Dim => Color::DarkGrey,
            Self::Accent => Color::Cyan,
            Self::Success => Color::Green,
            Self::Warn => Color::Yellow,
            Self::Error => Color::Red,
            Self::Fixed(c) => c,
        }
    }
}

/// One pre-rendered body line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelLine {
    pub text: String,
    pub face: PanelFace,
}

impl PanelLine {
    pub fn new(text: impl Into<String>, face: PanelFace) -> Self {
        Self {
            text: text.into(),
            face,
        }
    }
}

/// Render a markdown body into panel lines at `width` columns.
/// Inline styling escapes produced by the markdown renderer are
/// stripped (panel rows are painted as plain cells); each row keeps
/// the colour the renderer picked.
#[allow(dead_code)] // consumed by external producers (panel feeds)
pub fn lines_from_markdown(markdown: &str, width: usize) -> Vec<PanelLine> {
    crate::ui::markdown::markdown_to_styled(markdown, width.max(1), Color::Green)
        .into_iter()
        .map(|e| {
            PanelLine::new(
                crate::ui::ansi::strip_ansi(&e.text),
                PanelFace::Fixed(e.color),
            )
        })
        .collect()
}

/// One operation on the external panel set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelOp {
    /// Create panel `id`, or replace its title and body wholesale.
    Show {
        id: String,
        title: String,
        lines: Vec<PanelLine>,
    },
    /// Remove panel `id` (no-op when absent).
    Close { id: String },
    /// Create-or-retitle an accumulating (log-style) panel `id` and
    /// make it the focused panel, painted first.
    FocusTab { id: String, title: String },
    /// Append one line to panel `id`, creating it (titled `id`) when
    /// absent. Oldest lines drop once the panel is full.
    AppendTab { id: String, line: PanelLine },
}

/// One panel's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalPanel {
    pub id: String,
    pub title: String,
    pub lines: VecDeque<PanelLine>,
    /// Accumulating panel: when rows are short the painter keeps the
    /// NEWEST lines; a `Show` panel keeps the first ones.
    pub tail: bool,
}

impl ExternalPanel {
    /// The body rows to paint when only `rows` fit.
    pub fn visible_lines(&self, rows: usize) -> impl Iterator<Item = &PanelLine> {
        let skip = if self.tail {
            self.lines.len().saturating_sub(rows)
        } else {
            0
        };
        self.lines.iter().skip(skip).take(rows)
    }
}

/// The whole external panel set. Pure state; mutate only via
/// [`ExternalPanels::apply`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalPanels {
    /// Insertion order (oldest first).
    panels: Vec<ExternalPanel>,
    focused: Option<String>,
}

fn clean(s: &str) -> String {
    let stripped = crate::ui::ansi::strip_escapes(s, crate::ui::ansi::StripPolicy::STRICT);
    if stripped.chars().count() > MAX_LINE_CHARS {
        stripped.chars().take(MAX_LINE_CHARS).collect()
    } else {
        stripped
    }
}

fn clean_line(line: PanelLine) -> PanelLine {
    PanelLine {
        text: clean(&line.text),
        face: line.face,
    }
}

impl ExternalPanels {
    pub const fn new() -> Self {
        Self {
            panels: Vec::new(),
            focused: None,
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    pub fn len(&self) -> usize {
        self.panels.len()
    }

    #[allow(dead_code)]
    pub fn get(&self, id: &str) -> Option<&ExternalPanel> {
        self.panels.iter().find(|p| p.id == id)
    }

    #[allow(dead_code)]
    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    /// Panels in paint order: the focused panel first, then the rest
    /// in insertion order.
    pub fn panels(&self) -> Vec<&ExternalPanel> {
        let focused = self.focused.as_deref();
        let mut out: Vec<&ExternalPanel> = Vec::with_capacity(self.panels.len());
        if let Some(f) = focused
            && let Some(p) = self.panels.iter().find(|p| p.id == f)
        {
            out.push(p);
        }
        out.extend(
            self.panels
                .iter()
                .filter(|p| Some(p.id.as_str()) != focused),
        );
        out
    }

    /// Find panel `id`, creating it (evicting the oldest when full).
    fn entry(&mut self, id: &str, title: &str, tail: bool) -> &mut ExternalPanel {
        if let Some(i) = self.panels.iter().position(|p| p.id == id) {
            return &mut self.panels[i];
        }
        if self.panels.len() >= MAX_PANELS {
            let evicted = self.panels.remove(0);
            if self.focused.as_deref() == Some(evicted.id.as_str()) {
                self.focused = None;
            }
        }
        self.panels.push(ExternalPanel {
            id: id.to_string(),
            title: title.to_string(),
            lines: VecDeque::new(),
            tail,
        });
        self.panels.last_mut().expect("just pushed")
    }

    /// Fold one op into the state. Producer text is sanitised here
    /// (escape sequences and control bytes removed, lengths capped)
    /// so the painter can trust every string it gets.
    pub fn apply(&mut self, op: PanelOp) {
        match op {
            PanelOp::Show { id, title, lines } => {
                let id = clean(&id);
                let title = clean(&title);
                let p = self.entry(&id, &title, false);
                p.title = title;
                p.tail = false;
                p.lines = lines
                    .into_iter()
                    .take(MAX_PANEL_LINES)
                    .map(clean_line)
                    .collect();
            }
            PanelOp::Close { id } => {
                let id = clean(&id);
                self.panels.retain(|p| p.id != id);
                if self.focused.as_deref() == Some(id.as_str()) {
                    self.focused = None;
                }
            }
            PanelOp::FocusTab { id, title } => {
                let id = clean(&id);
                let title = clean(&title);
                let p = self.entry(&id, &title, true);
                p.title = title;
                p.tail = true;
                self.focused = Some(id);
            }
            PanelOp::AppendTab { id, line } => {
                let id = clean(&id);
                let p = self.entry(&id, &id.clone(), true);
                p.lines.push_back(clean_line(line));
                while p.lines.len() > MAX_PANEL_LINES {
                    p.lines.pop_front();
                }
            }
        }
    }
}

// ── channel boundary ────────────────────────────────────────────────

type Chan = (Sender<PanelOp>, Mutex<Option<Receiver<PanelOp>>>);

/// Created on first use, so producers never race an install step:
/// ops sent before the UI starts queue (up to the capacity) and are
/// drained once the loop claims the receiver.
static CHAN: OnceLock<Chan> = OnceLock::new();

fn chan() -> &'static Chan {
    CHAN.get_or_init(|| {
        let (tx, rx) = channel(PANEL_CHAN_CAP);
        (tx, Mutex::new(Some(rx)))
    })
}

/// Claim the receiver. The first caller (the UI loop) gets it; later
/// calls return `None` and should await a pending future instead.
pub fn take_receiver() -> Option<Receiver<PanelOp>> {
    chan().1.lock_ignore_poison().take()
}

/// Send one op from any thread. Returns `false` when it was dropped
/// (queue full, or the UI loop has exited).
#[allow(dead_code)] // consumed by external producers (panel feeds)
pub fn panel_send(op: PanelOp) -> bool {
    chan().0.try_send(op).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(id: &str, title: &str, lines: &[&str]) -> PanelOp {
        PanelOp::Show {
            id: id.into(),
            title: title.into(),
            lines: lines
                .iter()
                .map(|l| PanelLine::new(*l, PanelFace::Normal))
                .collect(),
        }
    }

    fn append(id: &str, text: &str) -> PanelOp {
        PanelOp::AppendTab {
            id: id.into(),
            line: PanelLine::new(text, PanelFace::Dim),
        }
    }

    fn texts(p: &ExternalPanel) -> Vec<&str> {
        p.lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn show_creates_panel() {
        let mut s = ExternalPanels::default();
        s.apply(show("a", "Alpha", &["one", "two"]));
        let p = s.get("a").expect("panel a");
        assert_eq!(p.title, "Alpha");
        assert_eq!(texts(p), ["one", "two"]);
        assert!(!p.tail);
    }

    #[test]
    fn show_replaces_in_place() {
        let mut s = ExternalPanels::default();
        s.apply(show("a", "Alpha", &["one", "two"]));
        s.apply(show("b", "Beta", &["x"]));
        s.apply(show("a", "Alpha 2", &["three"]));
        assert_eq!(s.len(), 2);
        let p = s.get("a").unwrap();
        assert_eq!(p.title, "Alpha 2");
        assert_eq!(texts(p), ["three"]);
        // Replacing keeps insertion order.
        let order: Vec<&str> = s.panels().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(order, ["a", "b"]);
    }

    #[test]
    fn close_removes_and_clears_focus() {
        let mut s = ExternalPanels::default();
        s.apply(show("a", "A", &[]));
        s.apply(PanelOp::FocusTab {
            id: "t".into(),
            title: "T".into(),
        });
        assert_eq!(s.focused(), Some("t"));
        s.apply(PanelOp::Close { id: "t".into() });
        assert!(s.get("t").is_none());
        assert_eq!(s.focused(), None);
        s.apply(PanelOp::Close {
            id: "missing".into(),
        });
        s.apply(PanelOp::Close { id: "a".into() });
        assert!(s.is_empty());
    }

    #[test]
    fn focus_tab_paints_first_and_retitles() {
        let mut s = ExternalPanels::default();
        s.apply(show("a", "A", &["1"]));
        s.apply(show("b", "B", &["2"]));
        s.apply(PanelOp::FocusTab {
            id: "b".into(),
            title: "B focused".into(),
        });
        let order: Vec<&str> = s.panels().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(order, ["b", "a"]);
        let b = s.get("b").unwrap();
        assert_eq!(b.title, "B focused");
        assert!(b.tail, "focused tab is log-style");
        assert_eq!(texts(b), ["2"], "focus keeps the body");
    }

    #[test]
    fn append_creates_and_accumulates() {
        let mut s = ExternalPanels::default();
        s.apply(append("log", "first"));
        s.apply(append("log", "second"));
        let p = s.get("log").unwrap();
        assert_eq!(p.title, "log");
        assert_eq!(texts(p), ["first", "second"]);
        assert_eq!(p.lines[0].face, PanelFace::Dim);
    }

    #[test]
    fn append_is_bounded_dropping_oldest() {
        let mut s = ExternalPanels::default();
        for i in 0..MAX_PANEL_LINES + 5 {
            s.apply(append("log", &format!("l{i}")));
        }
        let p = s.get("log").unwrap();
        assert_eq!(p.lines.len(), MAX_PANEL_LINES);
        assert_eq!(p.lines.front().unwrap().text, "l5");
        assert_eq!(
            p.lines.back().unwrap().text,
            format!("l{}", MAX_PANEL_LINES + 4)
        );
    }

    #[test]
    fn show_truncates_to_line_cap() {
        let mut s = ExternalPanels::default();
        let many: Vec<String> = (0..MAX_PANEL_LINES * 2).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        s.apply(show("a", "A", &refs));
        assert_eq!(s.get("a").unwrap().lines.len(), MAX_PANEL_LINES);
    }

    #[test]
    fn panel_count_is_bounded_evicting_oldest() {
        let mut s = ExternalPanels::default();
        for i in 0..MAX_PANELS + 3 {
            s.apply(show(&format!("p{i}"), "t", &[]));
        }
        assert_eq!(s.len(), MAX_PANELS);
        assert!(s.get("p0").is_none());
        assert!(s.get("p2").is_none());
        assert!(s.get("p3").is_some());
    }

    #[test]
    fn text_is_sanitised_and_capped() {
        let mut s = ExternalPanels::default();
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        s.apply(PanelOp::Show {
            id: "a".into(),
            title: "\x1b[31mRed\x1b[0m\x07".into(),
            lines: vec![
                PanelLine::new("ok\x1b]0;evil\x07 done", PanelFace::Normal),
                PanelLine::new(long, PanelFace::Normal),
            ],
        });
        let p = s.get("a").unwrap();
        assert_eq!(p.title, "Red");
        assert_eq!(p.lines[0].text, "ok done");
        assert_eq!(p.lines[1].text.chars().count(), MAX_LINE_CHARS);
    }

    #[test]
    fn visible_lines_head_vs_tail() {
        let mut s = ExternalPanels::default();
        s.apply(show("a", "A", &["1", "2", "3"]));
        for t in ["1", "2", "3"] {
            s.apply(append("b", t));
        }
        let head: Vec<&str> = s
            .get("a")
            .unwrap()
            .visible_lines(2)
            .map(|l| l.text.as_str())
            .collect();
        let tail: Vec<&str> = s
            .get("b")
            .unwrap()
            .visible_lines(2)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(head, ["1", "2"]);
        assert_eq!(tail, ["2", "3"]);
    }

    #[test]
    fn face_names_map_with_fallback() {
        assert_eq!(PanelFace::from_name("warn"), PanelFace::Warn);
        assert_eq!(PanelFace::from_name(" Error "), PanelFace::Error);
        assert_eq!(PanelFace::from_name("dim"), PanelFace::Dim);
        assert_eq!(PanelFace::from_name("something-new"), PanelFace::Normal);
    }

    #[test]
    fn markdown_body_becomes_plain_lines() {
        let lines = lines_from_markdown("# Title\n\nsome **bold** text", 40);
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|l| !l.text.contains('\x1b')));
        assert!(lines.iter().any(|l| l.text.contains("bold")));
    }

    /// The only test touching the process-global channel.
    #[test]
    fn channel_delivers_ops_sent_before_the_receiver_is_claimed() {
        assert!(panel_send(PanelOp::Close { id: "early".into() }));
        let mut rx = take_receiver().expect("first claim gets the receiver");
        assert!(take_receiver().is_none(), "receiver is claimed once");
        assert_eq!(
            rx.try_recv().unwrap(),
            PanelOp::Close { id: "early".into() }
        );
        drop(rx);
        assert!(!panel_send(PanelOp::Close { id: "late".into() }));
    }
}
