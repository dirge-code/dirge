//! Boundary: carry out a view update. The only stratum of `ui::view`
//! with effects: it sets the renderer's view state and side-panel modes
//! and fires producer replies. What needs state only the UI loop owns
//! (chat lines, chat tabs, the editor) is handed back in [`Applied`].

use crossterm::style::Color;

use super::domain::{NoticeLevel, PanelScope, ViewEffect, ViewUpdate};
use crate::extras::panel_feed::{self, ReplyAction};
use crate::ui::colors::{c_agent, c_error};
use crate::ui::renderer::{PaneVisibility, PanelMode, Renderer};

/// A line for the chat area.
pub type ChatLine = (String, Color);

/// An effect the UI loop carries out itself: it owns the chat tabs and
/// the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handoff {
    /// Show this subagent's chat tab (full task id).
    OpenAgent(String),
    /// Start `/msg <id> ` in the editor.
    MessageAgent(String),
}

/// What applying an update leaves for the UI loop.
#[derive(Debug, Default)]
pub struct Applied {
    pub lines: Vec<ChatLine>,
    pub handoffs: Vec<Handoff>,
}

/// One effect's outcome at the boundary.
enum Outcome {
    Done,
    Line(ChatLine),
    Handoff(Handoff),
}

/// Apply `update` to `renderer`; the lines and hand-offs left for the
/// UI loop.
pub fn apply(renderer: &mut Renderer, update: &ViewUpdate) -> Applied {
    renderer.set_swarm(update.model.swarm.as_ref());
    let mut applied = Applied::default();
    for effect in &update.effects {
        match interpret(renderer, effect) {
            Outcome::Done => {}
            Outcome::Line(line) => applied.lines.push(line),
            Outcome::Handoff(handoff) => applied.handoffs.push(handoff),
        }
    }
    applied
}

fn interpret(renderer: &mut Renderer, effect: &ViewEffect) -> Outcome {
    match effect {
        ViewEffect::Notify { level, text } => Outcome::Line((text.clone(), notice_color(*level))),
        ViewEffect::Reply { action, target } => match reply_action(action, target.as_deref()) {
            Some(reply) => {
                panel_feed::spawn_reply(reply);
                Outcome::Done
            }
            None => Outcome::Line((format!("unknown panel reply '{action}'"), c_error())),
        },
        ViewEffect::PanelMode { scope, mode } => match panel_mode(mode) {
            Some(mode) => {
                match scope {
                    PanelScope::Both => renderer.set_panel_mode(mode),
                    PanelScope::Right => renderer.set_right_panel_mode(mode),
                }
                Outcome::Done
            }
            None => Outcome::Line((format!("unknown panel mode '{mode}'"), c_error())),
        },
        ViewEffect::Panes { left, right } => {
            renderer.set_pane_visibility(PaneVisibility {
                left: *left,
                right: *right,
            });
            Outcome::Done
        }
        ViewEffect::PanelStatus => Outcome::Line((panel_status_line(renderer), c_agent())),
        ViewEffect::DisplayStatus => Outcome::Line((display_status_line(renderer), c_agent())),
        ViewEffect::OpenAgent { id } => Outcome::Handoff(Handoff::OpenAgent(id.clone())),
        ViewEffect::MessageAgent { id } => Outcome::Handoff(Handoff::MessageAgent(id.clone())),
    }
}

fn notice_color(level: NoticeLevel) -> Color {
    match level {
        NoticeLevel::Info => c_agent(),
        NoticeLevel::Error => c_error(),
    }
}

/// The producer reply a `reply` effect names; `None` for an unknown
/// action or a `focus` without a target.
pub fn reply_action(action: &str, target: Option<&str>) -> Option<ReplyAction> {
    Some(match (action, target) {
        ("focus", Some(id)) => ReplyAction::Focus(id.to_string()),
        ("unfocus", _) => ReplyAction::Unfocus,
        ("next-tab", _) => ReplyAction::NextTab,
        ("prev-tab", _) => ReplyAction::PrevTab,
        ("refresh", _) => ReplyAction::Refresh,
        _ => return None,
    })
}

fn panel_mode(name: &str) -> Option<PanelMode> {
    Some(match name {
        "on" => PanelMode::On,
        "off" => PanelMode::Off,
        "auto" => PanelMode::Auto,
        "debug" => PanelMode::Debug,
        _ => return None,
    })
}

fn shown_panes(left: bool, right: bool) -> String {
    let mut shown = vec!["main"];
    if left {
        shown.insert(0, "left");
    }
    if right {
        shown.push("right");
    }
    shown.join("|")
}

fn panel_status_line(renderer: &Renderer) -> String {
    let shown = |on: bool| if on { "shown" } else { "hidden" };
    format!(
        "left panel: {:?} ({})  right panel: {:?} ({}). Use /display for per-pane control.",
        renderer.left_panel_mode(),
        shown(renderer.left_panel_visible()),
        renderer.right_panel_mode(),
        shown(renderer.right_panel_visible()),
    )
}

fn display_status_line(renderer: &Renderer) -> String {
    format!(
        "display: {} (usage: /display left|main|right)",
        shown_panes(
            renderer.left_panel_visible(),
            renderer.right_panel_visible()
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_effects_name_producer_replies() {
        assert_eq!(
            reply_action("focus", Some("a")),
            Some(ReplyAction::Focus("a".into()))
        );
        assert_eq!(reply_action("focus", None), None);
        assert_eq!(reply_action("next-tab", None), Some(ReplyAction::NextTab));
        assert_eq!(reply_action("prev-tab", None), Some(ReplyAction::PrevTab));
        assert_eq!(reply_action("refresh", None), Some(ReplyAction::Refresh));
        assert_eq!(reply_action("unfocus", None), Some(ReplyAction::Unfocus));
        assert_eq!(reply_action("warp", None), None);
    }

    #[test]
    fn every_wire_reply_round_trips() {
        for action in [
            ReplyAction::Focus("x".into()),
            ReplyAction::Unfocus,
            ReplyAction::NextTab,
            ReplyAction::PrevTab,
            ReplyAction::Refresh,
        ] {
            let target = match &action {
                ReplyAction::Focus(id) => Some(id.as_str()),
                _ => None,
            };
            assert_eq!(reply_action(action.name(), target), Some(action.clone()));
        }
    }

    #[test]
    fn panel_modes_and_panes_read_as_the_renderer_names_them() {
        assert_eq!(panel_mode("on"), Some(PanelMode::On));
        assert_eq!(panel_mode("debug"), Some(PanelMode::Debug));
        assert_eq!(panel_mode("sideways"), None);
        assert_eq!(shown_panes(true, false), "left|main");
        assert_eq!(shown_panes(false, true), "main|right");
    }
}
