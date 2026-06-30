//! Whether a `!`/`!!` bang command feeds its captured output to the agent
//! (`Visible`) or shows it live only (`Invisible`). Execution lives in
//! [`crate::ui::shell_session`] (headless: piped stdio, no PTY/screen takeover).

/// Whether the command's output is fed to the agent as a new turn (`Visible`)
/// or merely shown live on the terminal (`Invisible`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellKind {
    Visible,
    Invisible,
}
