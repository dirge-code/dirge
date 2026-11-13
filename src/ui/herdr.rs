use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Idle,
    Working,
    Blocked,
    Done,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

pub(crate) struct Status {
    tty: Option<std::fs::File>,
    current: Option<State>,
    startup_reannounce_at: Option<std::time::Instant>,
}

impl Status {
    pub(crate) fn new() -> Self {
        Self {
            tty: super::terminal::open_tty_for_write(),
            current: None,
            startup_reannounce_at: None,
        }
    }

    pub(crate) fn startup_reannounce_at(&self) -> Option<std::time::Instant> {
        self.startup_reannounce_at
    }

    pub(crate) fn update(&mut self, requested: State) {
        let now = std::time::Instant::now();
        let next = announcement(self.current, requested, self.startup_reannounce_at, now);
        let Some(next) = next else {
            return;
        };

        if self.current.is_none() {
            self.startup_reannounce_at = Some(now + std::time::Duration::from_millis(250));
        } else {
            self.startup_reannounce_at = None;
        }
        self.current = Some(next);
        if let Some(tty) = self.tty.as_mut() {
            let _ = emit(tty, next);
        }
    }
}

impl Drop for Status {
    fn drop(&mut self) {
        if self.current != Some(State::Done)
            && let Some(tty) = self.tty.as_mut()
        {
            let _ = emit(tty, State::Done);
        }
    }
}

fn announcement(
    current: Option<State>,
    requested: State,
    startup_reannounce_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<State> {
    transition(current, requested).or_else(|| {
        startup_reannounce_at
            .is_some_and(|deadline| now >= deadline && current == Some(requested))
            .then_some(requested)
    })
}

fn transition(current: Option<State>, requested: State) -> Option<State> {
    let next = match (current, requested) {
        (Some(State::Working | State::Blocked), State::Idle) => State::Done,
        (Some(State::Done), State::Idle) => return None,
        _ => requested,
    };
    (current != Some(next)).then_some(next)
}

fn emit(writer: &mut impl Write, state: State) -> io::Result<()> {
    write!(writer, "\x1b]7501;state={}:app=dirge\x1b\\", state.as_str())?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_done_when_work_settles() {
        assert_eq!(transition(None, State::Idle), Some(State::Idle));
        assert_eq!(
            transition(Some(State::Idle), State::Working),
            Some(State::Working)
        );
        assert_eq!(
            transition(Some(State::Working), State::Idle),
            Some(State::Done)
        );
        assert_eq!(transition(Some(State::Done), State::Idle), None);
        assert_eq!(
            transition(Some(State::Done), State::Working),
            Some(State::Working)
        );
    }

    #[test]
    fn reports_blocked_modal_and_resumed_work() {
        assert_eq!(
            transition(Some(State::Working), State::Blocked),
            Some(State::Blocked)
        );
        assert_eq!(
            transition(Some(State::Blocked), State::Working),
            Some(State::Working)
        );
    }

    #[test]
    fn reannounces_only_the_unchanged_startup_state_after_deadline() {
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_millis(250);

        assert_eq!(
            announcement(Some(State::Idle), State::Idle, Some(deadline), now),
            None
        );
        assert_eq!(
            announcement(Some(State::Idle), State::Idle, Some(deadline), deadline),
            Some(State::Idle)
        );
        assert_eq!(
            announcement(Some(State::Idle), State::Working, Some(deadline), deadline),
            Some(State::Working)
        );
    }

    #[test]
    fn emits_herdr_status_sequence() {
        let mut output = Vec::new();
        emit(&mut output, State::Blocked).unwrap();
        assert_eq!(output, b"\x1b]7501;state=blocked:app=dirge\x1b\\");
    }
}
