//! /msg handler: send text to a running subagent.

use crate::agent::tools::task::{MessageOutcome, message_subagent};
use crate::ui::slash::{SlashCtx, c_agent, c_error};

/// `/msg <id-prefix> <text>` queues `text` for the subagent whose id starts
/// with `id-prefix`. The subagent reads it as a user message at its next turn
/// boundary. Typing on a subagent's own chat tab does the same without an id.
pub(crate) async fn cmd_msg(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
    text: &str,
) -> anyhow::Result<()> {
    let prefix = parts.get(1).copied().unwrap_or("").trim();
    let body = message_body(text);
    if prefix.is_empty() || body.is_empty() {
        ctx.renderer
            .write_line("usage: /msg <id-prefix> <text>", c_error())?;
        return Ok(());
    }
    match message_subagent(prefix, body) {
        MessageOutcome::Queued(id) => {
            ctx.renderer.write_line(
                &format!(
                    "queued for {} (delivered at its next turn boundary)",
                    crate::text::short_id(&id)
                ),
                c_agent(),
            )?;
        }
        MessageOutcome::NotFound => {
            ctx.renderer.write_line(
                &format!("no running tooled subagent matches '{}'", prefix),
                c_error(),
            )?;
        }
        MessageOutcome::Ambiguous(ids) => {
            ctx.renderer.write_line(
                &format!("ambiguous prefix '{}'; matches: {}", prefix, ids.join(" ")),
                c_error(),
            )?;
        }
    }
    Ok(())
}

/// Everything after the command and the id prefix, with inner whitespace
/// kept as typed.
fn message_body(text: &str) -> &str {
    let rest = text
        .trim_start()
        .split_once(char::is_whitespace)
        .map_or("", |(_, r)| r)
        .trim_start();
    rest.split_once(char::is_whitespace)
        .map_or("", |(_, r)| r)
        .trim()
}

#[cfg(test)]
mod tests {
    use super::message_body;

    #[test]
    fn body_keeps_inner_whitespace() {
        assert_eq!(
            message_body("/msg ab12  look at  foo.rs "),
            "look at  foo.rs"
        );
        assert_eq!(message_body("/msg ab12"), "");
        assert_eq!(message_body("/msg"), "");
    }
}
