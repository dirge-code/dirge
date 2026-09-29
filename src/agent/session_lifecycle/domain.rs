//! Session lifecycle values: the two events and what they carry.

/// A session's first run in this process opening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStart {
    pub session_id: Option<String>,
    pub cwd: String,
    /// True when the session has no earlier conversation.
    pub first_prompt: bool,
    /// MCP servers connected when the session starts.
    pub mcp_servers: Vec<String>,
}

/// A session ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnd {
    pub session_id: Option<String>,
    pub cwd: String,
    pub reason: SessionEndReason,
}

/// Why a session ended, as [`SessionEnd`] carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEndReason {
    /// dirge is exiting.
    Exit,
    /// Another session, or a cleared one, takes its place.
    Swap,
}

impl SessionEndReason {
    /// The reason as listeners receive it.
    pub fn key(self) -> &'static str {
        match self {
            SessionEndReason::Exit => "exit",
            SessionEndReason::Swap => "swap",
        }
    }
}

/// Where a session was ended from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndCause {
    /// The interactive session quit.
    Quit,
    /// A `--print` run finished.
    Print,
    /// `/clear` emptied the conversation.
    Clear,
    /// `/sessions` switched to another session.
    Switch,
}

/// What a run knows about its session when it is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartFacts {
    pub session_id: Option<String>,
    pub cwd: String,
    pub first_prompt: bool,
}

/// The lifecycle hooks a [`super::SessionLifecycle`] may listen on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleHook {
    Start,
    End,
}
