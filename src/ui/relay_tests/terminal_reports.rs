//! Unsolicited terminal reports must not reach the UI as text (dirge-v4xf).
//!
//! These drive real bytes through a PTY on fd 0 and the production input
//! reader, so they check what `ui::input_reader::ReportFilter`'s unit tests
//! have to assume: that crossterm 0.29 reports an OSC / DCS introducer as an
//! Alt-modified character and then parses the payload as ordinary text.
//! Before the filter, phase 1 below collected 22 events — `Alt+]` plus every
//! character of `11;rgb:2e2e/3434/3636`, all of which the compose editor
//! would have inserted.
//!
//! One test with phases, like `crossterm_suite`: crossterm's event source is
//! a process-wide singleton bound to whatever fd 0 was when it was first
//! created, so only the first fd-0 swap in a process takes effect. Split into
//! separate `#[test]`s, everything after the first sees no input at all.
//!
//! Failures are collected rather than asserted in place so fd 0 is always
//! restored — a panic mid-test would strand the process's stdin on a closed
//! PTY and take every later test with it.
//!
//! All tests in this module require `sandbox-microvm`.

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use crate::event::UserEvent;
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::io::Write;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    type Rx = tokio::sync::mpsc::UnboundedReceiver<UserEvent>;

    /// Collect events for `window`, stopping early once `want` have arrived —
    /// then keep draining briefly, because a leak arrives right behind the
    /// events we expected.
    fn collect(rx: &mut Rx, window: Duration, want: usize) -> Vec<UserEvent> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) => {
                    out.push(ev);
                    if out.len() >= want {
                        std::thread::sleep(Duration::from_millis(20));
                        while let Ok(ev) = rx.try_recv() {
                            out.push(ev);
                        }
                        return out;
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    fn describe(events: &[UserEvent]) -> String {
        events
            .iter()
            .map(|e| match e {
                UserEvent::Key(k) => format!("{:?}+{:?}", k.code, k.modifiers),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// One report followed by one keystroke: only the keystroke may arrive.
    /// The trailing keystroke is what proves the filter *closed* the report
    /// rather than swallowing everything after it, and the gap before it
    /// exceeds the burst window so an unclosed run would be released as text
    /// (a visible failure) instead of vanishing.
    fn check_report(
        label: &str,
        report: &[u8],
        follow: u8,
        pty: &mut std::fs::File,
        rx: &mut Rx,
        failures: &mut Vec<String>,
    ) {
        pty.write_all(report).expect("write report");
        pty.flush().ok();
        std::thread::sleep(Duration::from_millis(30));
        pty.write_all(&[follow]).expect("write keystroke");
        pty.flush().ok();

        let events = collect(rx, Duration::from_millis(500), 1);
        let ok = match events.as_slice() {
            [UserEvent::Key(k)] => {
                k.code == KeyCode::Char(follow as char) && !k.modifiers.contains(KeyModifiers::ALT)
            }
            _ => false,
        };
        if !ok {
            failures.push(format!(
                "{label}: expected only the trailing keystroke {:?}, got [{}]",
                follow as char,
                describe(&events)
            ));
        }
    }

    #[test]
    fn terminal_report_filter_suite() {
        let _guard = serial_fd_test();

        // ── save fd 0 ──
        let saved_stdin = unsafe { OwnedFd::from_raw_fd(libc::dup(0)) };
        if saved_stdin.as_raw_fd() < 0 {
            eprintln!("skipping: dup(0) failed");
            return;
        }
        let saved_termios: Option<libc::termios> = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) >= 0 {
                Some(t)
            } else {
                None
            }
        };
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);

        // ── PTY as fd 0, one pair for every phase ──
        let Some((mut pty, secondary)) = open_pty_pair() else {
            eprintln!("skipping: no PTY available");
            return;
        };
        assert!(
            unsafe { libc::dup2(secondary.as_raw_fd(), 0) } >= 0,
            "dup2 to fd 0 failed"
        );
        make_raw_fd(0).expect("make_raw on fd 0");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UserEvent>();
        crate::ui::input_reader::spawn_input_reader(tx);
        // Let the reader reach its poll loop before the first write.
        std::thread::sleep(Duration::from_millis(20));

        let mut failures: Vec<String> = Vec::new();

        // ── Phase 1: OSC 11 background-colour report, BEL-terminated ──
        // What kitty / ghostty / foot / iTerm2 emit on a theme change or an
        // alt-screen transition, and what a multiplexer may hand to the
        // wrong pane.
        check_report(
            "osc-11-bel",
            b"\x1b]11;rgb:2e2e/3434/3636\x07",
            b'x',
            &mut pty,
            &mut rx,
            &mut failures,
        );

        // ── Phase 2: the same shape terminated by ST, long enough to stand
        // in for an OSC 52 clipboard reply ──
        check_report(
            "osc-52-st",
            b"\x1b]52;c;SGVsbG8sIGNsaXBib2FyZCwgdGhpcyBpcyBhIGxvbmcgcmVwbHk=\x1b\\",
            b'y',
            &mut pty,
            &mut rx,
            &mut failures,
        );

        // ── Phase 3: a DCS reply (XTGETTCAP) ──
        check_report(
            "dcs-xtgettcap",
            b"\x1bP1+r5463=787465726d\x1b\\",
            b'z',
            &mut pty,
            &mut rx,
            &mut failures,
        );

        // ── Phase 4: a deliberate Alt+] is delayed by the burst window,
        // never dropped ──
        pty.write_all(b"\x1b]").expect("write alt-]");
        pty.flush().ok();
        let events = collect(&mut rx, Duration::from_millis(500), 1);
        let released = match events.as_slice() {
            [UserEvent::Key(k)] => {
                k.code == KeyCode::Char(']') && k.modifiers.contains(KeyModifiers::ALT)
            }
            _ => false,
        };
        if !released {
            failures.push(format!(
                "alt-]: expected the introducer to be released, got [{}]",
                describe(&events)
            ));
        }

        // ── Phase 5: a retired reader stops consuming input (dirge-xxo9) ──
        // Spawning a replacement — what `resume_tui_after_subprocess` does —
        // used to leave the previous thread running: it re-read a shutdown
        // flag the resume path had just cleared and kept pulling bytes off
        // fd 0 next to its replacement. The first reader must go quiet and
        // the second must see every keystroke. Last phase: it retires the
        // reader the phases above share.
        let (second_tx, mut second_rx) = tokio::sync::mpsc::unbounded_channel::<UserEvent>();
        crate::ui::input_reader::spawn_input_reader(second_tx);
        std::thread::sleep(Duration::from_millis(20));
        pty.write_all(b"abc").expect("write keystrokes");
        pty.flush().ok();
        let second = collect(&mut second_rx, Duration::from_millis(500), 3);
        let first = collect(&mut rx, Duration::from_millis(50), 1);
        if second.len() != 3 {
            failures.push(format!(
                "retired-reader: the live reader must see every keystroke, got [{}]",
                describe(&second)
            ));
        }
        if !first.is_empty() {
            failures.push(format!(
                "retired-reader: a retired reader kept consuming input: [{}]",
                describe(&first)
            ));
        }

        // ── restore fd 0 before asserting ──
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
        crate::ui::terminal::join_reader(Duration::from_millis(100));
        unsafe {
            libc::dup2(saved_stdin.as_raw_fd(), 0);
            if let Some(ref t) = saved_termios {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);
        drop(secondary);

        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }
}
