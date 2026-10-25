//! Pure decisions of the session lifecycle.

use super::domain::{EndCause, SessionEnd, SessionEndReason, SessionStart, StartFacts};
use crate::agent::agent_loop::hooks::RunOpening;

/// The session running in this process, once its start was announced. One
/// main session runs at a time, so an end closes whichever that is, under
/// the id its runs carried.
#[derive(Debug, Default)]
pub struct StartLedger(Option<Option<String>>);

impl StartLedger {
    /// Records that runs of `session_id` are opening; true when it was not
    /// already the running session, which is when its start is announced.
    /// A session under a new id (a compaction fold) takes the place of the
    /// one before it.
    pub fn admit(&mut self, session_id: Option<&str>) -> bool {
        let session_id = session_id.map(str::to_string);
        if self.0.as_ref() == Some(&session_id) {
            return false;
        }
        self.0 = Some(session_id);
        true
    }

    /// Forgets the running session and answers its id; `None` when no start
    /// was announced. A later run under the same id starts it again.
    pub fn close(&mut self) -> Option<Option<String>> {
        self.0.take()
    }
}

/// The start of the session `facts` describe, with the MCP servers
/// connected by then.
pub fn start_event(facts: StartFacts, mcp_servers: Vec<String>) -> SessionStart {
    SessionStart {
        session_id: facts.session_id,
        cwd: facts.cwd,
        first_prompt: facts.first_prompt,
        mcp_servers,
    }
}

/// The reason an end from `cause` carries.
pub fn end_reason(cause: EndCause) -> SessionEndReason {
    match cause {
        EndCause::Quit | EndCause::Print => SessionEndReason::Exit,
        EndCause::Clear | EndCause::Switch => SessionEndReason::Swap,
    }
}

/// The end of `session_id`, working in `cwd`, ended from `cause`.
pub fn end_event(session_id: Option<&str>, cwd: String, cause: EndCause) -> SessionEnd {
    SessionEnd {
        session_id: session_id.map(str::to_string),
        cwd,
        reason: end_reason(cause),
    }
}

/// One reminder holding every non-blank start answer, for the first turn;
/// `None` when there is none.
pub fn start_reminder(texts: &[String]) -> Option<String> {
    let texts: Vec<&str> = texts
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    (!texts.is_empty()).then(|| {
        format!(
            "<system-reminder>\nsession-start context:\n{}\n</system-reminder>",
            texts.join("\n\n")
        )
    })
}

/// `opening` with `reminder` added to what its first turn leads with.
pub fn with_reminder(mut opening: RunOpening, reminder: Option<String>) -> RunOpening {
    opening.reminders.extend(reminder);
    opening
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(id: Option<&str>, first_prompt: bool) -> StartFacts {
        StartFacts {
            session_id: id.map(str::to_string),
            cwd: "/w".into(),
            first_prompt,
        }
    }

    #[test]
    fn a_session_starts_once_while_it_runs() {
        let mut ledger = StartLedger::default();
        assert!(ledger.admit(Some("s1")));
        assert!(!ledger.admit(Some("s1")));
        assert!(ledger.admit(Some("s2")), "a fold's new id starts again");
        assert!(ledger.admit(None));
        assert!(!ledger.admit(None));
    }

    #[test]
    fn the_running_session_ends_once_and_may_start_again() {
        let mut ledger = StartLedger::default();
        assert_eq!(ledger.close(), None, "never started");
        ledger.admit(Some("s1"));
        ledger.admit(Some("s2"));
        assert_eq!(
            ledger.close(),
            Some(Some("s2".into())),
            "the fold's id ends"
        );
        assert_eq!(ledger.close(), None, "ends once");
        assert!(ledger.admit(Some("s2")), "a later run starts it again");
        ledger.close();
        ledger.admit(None);
        assert_eq!(ledger.close(), Some(None), "an unsaved session ends too");
    }

    #[test]
    fn the_start_event_carries_the_facts_and_the_servers() {
        let event = start_event(facts(Some("s1"), false), vec!["hive".into()]);
        assert_eq!(
            event,
            SessionStart {
                session_id: Some("s1".into()),
                cwd: "/w".into(),
                first_prompt: false,
                mcp_servers: vec!["hive".into()],
            }
        );
    }

    #[test]
    fn exits_end_with_exit_and_replacements_with_swap() {
        assert_eq!(end_reason(EndCause::Quit), SessionEndReason::Exit);
        assert_eq!(end_reason(EndCause::Print), SessionEndReason::Exit);
        assert_eq!(end_reason(EndCause::Clear), SessionEndReason::Swap);
        assert_eq!(end_reason(EndCause::Switch), SessionEndReason::Swap);
        let end = end_event(Some("s1"), "/w".into(), EndCause::Switch);
        assert_eq!(end.reason.key(), "swap");
        assert_eq!(end.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn start_answers_become_one_reminder() {
        assert_eq!(start_reminder(&[]), None);
        assert_eq!(start_reminder(&["  ".into()]), None);
        let reminder = start_reminder(&["axioms: a".into(), " open work: b ".into()]).unwrap();
        assert_eq!(
            reminder,
            "<system-reminder>\nsession-start context:\naxioms: a\n\nopen work: b\n</system-reminder>"
        );
    }

    #[test]
    fn the_reminder_joins_the_opening_and_nothing_else_changes() {
        let opening = RunOpening {
            system_prompt: "sys".into(),
            prompt: "hi".into(),
            reminders: Vec::new(),
            refusal: None,
        };
        assert_eq!(with_reminder(opening.clone(), None), opening);
        let with = with_reminder(opening, Some("r".into()));
        assert_eq!(with.reminders, vec!["r".to_string()]);
        assert_eq!(
            (with.system_prompt.as_str(), with.prompt.as_str()),
            ("sys", "hi")
        );
    }
}
