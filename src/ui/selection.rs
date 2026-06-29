//! App-level text selection handler for mouse drag in the chat buffer.
//!
//! `handle()` consumes a `UserEvent` (Mouse Down / Drag / Up) and the
//! current `Renderer` state, producing an `Outcome` the UI loop's
//! `tokio::select!` arm dispatches on: repaint the viewport, copy to
//! clipboard on mouse-up, or pass through as unhandled.
//!
//! Selection state lives on `Renderer` (`selection_active`,
//! `selection_start`, `selection_end`); this module is stateless.

use crate::event::UserEvent;
use crate::ui::renderer::{Renderer, copy_to_clipboard};

#[derive(Debug)]
pub enum Outcome {
    /// Nothing matched — pass the event on to the consumer.
    NotHandled,
    /// Buffer state changed (drag started / moved); repaint needed.
    Repaint,
    /// Selection completed (mouse-up) and `String` was copied to
    /// the system clipboard. Repaint still needed.
    RepaintAndCopied,
}

pub fn handle(ev: &UserEvent, renderer: &mut Renderer) -> Outcome {
    // #387: this module mutates the renderer's selection fields directly and
    // signals the caller via the Outcome. Any repaint outcome means visible
    // state changed, so mark the frame dirty here — otherwise the model-driven
    // render effect (which paints only when dirty) wouldn't repaint a drag
    // inside the modal sub-loops.
    let outcome = handle_inner(ev, renderer);
    if matches!(outcome, Outcome::Repaint | Outcome::RepaintAndCopied) {
        renderer.request_repaint();
    }
    outcome
}

fn handle_inner(ev: &UserEvent, renderer: &mut Renderer) -> Outcome {
    match ev {
        UserEvent::MouseDown { row, col } => {
            // Double-click → select the word under the cursor. Detected
            // when a second mouse-down lands on the same cell within the
            // double-click window of the previous one.
            const DOUBLE_CLICK_MS: u128 = 400;
            let now = std::time::Instant::now();
            let is_double = matches!(
                renderer.last_click,
                Some((t, r, c))
                    if r == *row && c == *col && now.duration_since(t).as_millis() <= DOUBLE_CLICK_MS
            );
            renderer.last_click = Some((now, *row, *col));

            let Some(pos) = renderer.buffer_pos_at(*row, *col) else {
                return Outcome::NotHandled;
            };

            if is_double {
                renderer.last_click = None; // consume so a 3rd click isn't a new double
                if let Some((start, end)) = renderer.word_bounds_at(pos) {
                    renderer.selection_active = true;
                    renderer.selection_start = Some(start);
                    renderer.selection_end = Some(end);
                    // The trailing mouse-up must not collapse the word.
                    renderer.suppress_next_mouseup = true;
                    return match renderer.selected_text() {
                        Some(t) => {
                            copy_to_clipboard(&t);
                            renderer.notify_copied();
                            Outcome::RepaintAndCopied
                        }
                        None => Outcome::Repaint,
                    };
                }
                // Double-clicked on a gap: fall through to a normal click.
            }

            renderer.selection_active = true;
            renderer.selection_start = Some(pos);
            renderer.selection_end = Some(pos);
            Outcome::Repaint
        }
        UserEvent::MouseDrag { row, col } => {
            if !renderer.selection_active {
                return Outcome::NotHandled;
            }
            let Some(pos) = renderer.buffer_pos_at(*row, *col) else {
                return Outcome::NotHandled;
            };
            renderer.selection_end = Some(pos);
            Outcome::Repaint
        }
        UserEvent::MouseUp { row, col } => {
            // The mouse-up that ends a double-click must leave the word
            // selection intact (it was already copied on the down).
            if renderer.suppress_next_mouseup {
                renderer.suppress_next_mouseup = false;
                return Outcome::Repaint;
            }
            if !renderer.selection_active {
                return Outcome::NotHandled;
            }
            if let Some(pos) = renderer.buffer_pos_at(*row, *col) {
                renderer.selection_end = Some(pos);
            }
            renderer.selection_active = false;
            let text = renderer.selected_text();
            renderer.clear_selection();
            match text {
                Some(t) => {
                    copy_to_clipboard(&t);
                    renderer.notify_copied();
                    Outcome::RepaintAndCopied
                }
                None => Outcome::Repaint,
            }
        }
        _ => Outcome::NotHandled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::renderer::Renderer;

    #[test]
    fn mouse_down_starts_selection() {
        let mut r = Renderer::new().unwrap();
        r.set_chat_rect_for_test(ratatui::layout::Rect::new(0, 1, 80, 24));
        r.write_line("hello world", crossterm::style::Color::White)
            .unwrap();
        let outcome = handle(&UserEvent::MouseDown { row: 1, col: 0 }, &mut r);
        assert!(matches!(outcome, Outcome::Repaint));
        assert!(r.selection_active);
    }

    #[test]
    fn mouse_up_completes_selection() {
        let mut r = Renderer::new().unwrap();
        r.set_chat_rect_for_test(ratatui::layout::Rect::new(0, 1, 80, 24));
        r.write_line("hello world", crossterm::style::Color::White)
            .unwrap();
        handle(&UserEvent::MouseDown { row: 1, col: 0 }, &mut r);
        handle(&UserEvent::MouseDrag { row: 1, col: 5 }, &mut r);
        let outcome = handle(&UserEvent::MouseUp { row: 1, col: 5 }, &mut r);
        // Outcome reports selection completed; clipboard copy is
        // best-effort (depends on OS tool availability).
        assert!(
            matches!(outcome, Outcome::RepaintAndCopied | Outcome::Repaint),
            "got {outcome:?}"
        );
        assert!(!r.selection_active);
    }

    #[test]
    fn double_click_selects_word_under_cursor() {
        let mut r = Renderer::new().unwrap();
        r.set_chat_rect_for_test(ratatui::layout::Rect::new(0, 1, 80, 24));
        r.write_line("hello world", crossterm::style::Color::White)
            .unwrap();
        // Two rapid clicks on the same cell (col 7 = inside "world").
        handle(&UserEvent::MouseDown { row: 1, col: 7 }, &mut r);
        handle(&UserEvent::MouseUp { row: 1, col: 7 }, &mut r);
        let out = handle(&UserEvent::MouseDown { row: 1, col: 7 }, &mut r);
        assert!(
            matches!(out, Outcome::RepaintAndCopied | Outcome::Repaint),
            "got {out:?}"
        );
        // The whole word "world" is selected (chars 6..11).
        assert_eq!(r.selection_start, Some((0, 6)));
        assert_eq!(r.selection_end, Some((0, 11)));
        assert_eq!(r.selected_text().as_deref(), Some("world"));
        // The trailing mouse-up keeps the word selected, doesn't collapse it.
        handle(&UserEvent::MouseUp { row: 1, col: 7 }, &mut r);
        assert_eq!(r.selected_text().as_deref(), Some("world"));
    }

    #[test]
    fn double_click_on_whitespace_selects_nothing() {
        let mut r = Renderer::new().unwrap();
        r.set_chat_rect_for_test(ratatui::layout::Rect::new(0, 1, 80, 24));
        r.write_line("hi   there", crossterm::style::Color::White)
            .unwrap();
        // col 3 is a space between the words.
        handle(&UserEvent::MouseDown { row: 1, col: 3 }, &mut r);
        handle(&UserEvent::MouseUp { row: 1, col: 3 }, &mut r);
        handle(&UserEvent::MouseDown { row: 1, col: 3 }, &mut r);
        // No word selected → falls through to a single-point selection.
        assert_eq!(r.selected_text(), None);
    }

    #[test]
    fn two_slow_clicks_are_not_a_double_click() {
        let mut r = Renderer::new().unwrap();
        r.set_chat_rect_for_test(ratatui::layout::Rect::new(0, 1, 80, 24));
        r.write_line("hello world", crossterm::style::Color::White)
            .unwrap();
        handle(&UserEvent::MouseDown { row: 1, col: 7 }, &mut r);
        handle(&UserEvent::MouseUp { row: 1, col: 7 }, &mut r);
        // Simulate time passing beyond the double-click window.
        std::thread::sleep(std::time::Duration::from_millis(450));
        handle(&UserEvent::MouseDown { row: 1, col: 7 }, &mut r);
        // Single click → point selection, not a word.
        assert_eq!(r.selection_start, r.selection_end);
    }

    #[test]
    fn non_mouse_events_are_not_handled() {
        let mut r = Renderer::new().unwrap();
        let outcome = handle(
            &UserEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('y'),
                crossterm::event::KeyModifiers::NONE,
            )),
            &mut r,
        );
        assert!(matches!(outcome, Outcome::NotHandled));
    }

    #[test]
    fn mouse_outside_chat_is_not_handled() {
        let mut r = Renderer::new().unwrap();
        let outcome = handle(&UserEvent::MouseDown { row: 999, col: 999 }, &mut r);
        assert!(matches!(outcome, Outcome::NotHandled));
        assert!(!r.selection_active);
    }
}
