//! Promote: raw UI input (a key, a submitted line) into view events,
//! decided from the latest model alone so the UI loop never asks the
//! engine. Pure.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::domain::{GridCell, ViewEvent, ViewModel};
use crate::ui::keymap::KeyAction;

/// Where a key goes while the grid is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRoute {
    /// A grid key, by name.
    Grid(String),
    /// Not a grid key: the normal dispatch handles it (global commands,
    /// Ctrl+C).
    PassThrough,
    /// Swallow it (the editor is inert while the grid is open).
    Swallow,
}

/// The name an unmodified key goes by in a grid keymap; `None` with
/// Ctrl or Alt held, or for a key no keymap can name.
pub fn key_name(key: &KeyEvent) -> Option<String> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    let named = match key.code {
        KeyCode::Esc => "Esc",
        KeyCode::Tab => "Tab",
        KeyCode::BackTab => "BackTab",
        KeyCode::Enter => "Enter",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::Char(c) => return Some(c.to_string()),
        _ => return None,
    };
    Some(named.to_string())
}

/// Route `key` while the grid is open. Grid keys win over the global
/// keymap (Shift+Tab is `cycle_prompt` elsewhere, prev-tab here); any
/// other key the keymap resolved (`action`) passes through, and so does
/// Ctrl+C.
pub fn route_key(model: &ViewModel, key: &KeyEvent, action: Option<KeyAction>) -> KeyRoute {
    if let Some(name) = key_name(key)
        && model.grid_consumes(&name)
    {
        return KeyRoute::Grid(name);
    }
    let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if action.is_some() || ctrl_c {
        KeyRoute::PassThrough
    } else {
        KeyRoute::Swallow
    }
}

/// The grid event for key `name`, with the cells the grid paints right
/// now (in paint order).
pub fn grid_event(name: String, cells: Vec<GridCell>, columns: usize) -> ViewEvent {
    ViewEvent::Grid {
        key: name,
        cells,
        columns,
    }
}

/// The event for `text` when it is a slash command the view owns.
/// View commands never reach the agent's busy gate.
pub fn view_command(model: &ViewModel, text: &str) -> Option<ViewEvent> {
    let mut words = text.split_whitespace();
    let name = words.next()?.strip_prefix('/')?;
    if !model.owns_command(name) {
        return None;
    }
    let args: Vec<&str> = words.collect();
    Some(ViewEvent::command(name, &args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(open: bool) -> ViewModel {
        ViewModel {
            swarm: open.then(Default::default),
            grid_keys: vec!["BackTab".into(), "Esc".into(), "q".into()],
            view_commands: vec!["panel".into(), "swarm".into()],
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn keys_are_named_only_without_ctrl_or_alt() {
        assert_eq!(
            key_name(&key(KeyCode::Esc, KeyModifiers::NONE)).as_deref(),
            Some("Esc")
        );
        assert_eq!(
            key_name(&key(KeyCode::Char('q'), KeyModifiers::NONE)).as_deref(),
            Some("q")
        );
        assert_eq!(
            key_name(&key(KeyCode::BackTab, KeyModifiers::SHIFT)).as_deref(),
            Some("BackTab")
        );
        assert_eq!(key_name(&key(KeyCode::Char('s'), KeyModifiers::ALT)), None);
        assert_eq!(
            key_name(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(key_name(&key(KeyCode::F(2), KeyModifiers::NONE)), None);
    }

    #[test]
    fn grid_keys_win_other_actions_pass_the_rest_is_swallowed() {
        let m = model(true);
        let back = key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(
            route_key(&m, &back, Some(KeyAction::CyclePrompt)),
            KeyRoute::Grid("BackTab".into())
        );
        let alt_s = key(KeyCode::Char('s'), KeyModifiers::ALT);
        assert_eq!(
            route_key(&m, &alt_s, Some(KeyAction::ToggleSwarm)),
            KeyRoute::PassThrough
        );
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(route_key(&m, &ctrl_c, None), KeyRoute::PassThrough);
        assert_eq!(
            route_key(&m, &key(KeyCode::Char('x'), KeyModifiers::NONE), None),
            KeyRoute::Swallow
        );
    }

    #[test]
    fn only_owned_commands_become_view_events() {
        let m = model(false);
        assert_eq!(
            view_command(&m, "/swarm on"),
            Some(ViewEvent::command("swarm", &["on"]))
        );
        assert_eq!(
            view_command(&m, "/panel focus  a1 "),
            Some(ViewEvent::command("panel", &["focus", "a1"]))
        );
        assert_eq!(view_command(&m, "/model"), None);
        assert_eq!(view_command(&m, "swarm"), None);
        assert_eq!(view_command(&m, ""), None);
    }
}
