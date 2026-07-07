use std::io;
use std::process::{Command, Stdio};

use super::NotificationSpec;

pub(super) fn notify(spec: &NotificationSpec<'_>) -> io::Result<()> {
    let script = notification_script(spec.title, spec.message);
    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

fn notification_script(title: &str, message: &str) -> String {
    format!(
        "display notification {} with title {}",
        applescript_string(message),
        applescript_string(title),
    )
}

fn applescript_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_strings_escape_quotes_and_backslashes() {
        assert_eq!(
            applescript_string(r#"say "hi" \ now"#),
            r#""say \"hi\" \\ now""#,
        );
    }

    #[test]
    fn notification_script_uses_display_notification() {
        let script = notification_script("Dirge", "Needs input");
        assert_eq!(
            script,
            r#"display notification "Needs input" with title "Dirge""#,
        );
    }
}
