//! /cache handler — session-cumulative prefix-cache hit ratio.
//!
//! Providers like DeepSeek and Anthropic serve repeated request
//! prefixes from a cache at a steep discount (DeepSeek ~1/10 the
//! input price). dirge keeps the cache warm by holding the system
//! prompt + tool defs at a stable, sorted prefix; `/cache` is the
//! instrument that tells you whether that discipline is actually
//! landing hits. See `Session::cache_hit_ratio`.

use crate::session::Session;
use crate::ui::slash::{SlashCtx, c_result};
use crate::ui::theme;

/// Render the cumulative cache report lines for a session. Pure so
/// it can be unit-tested without a renderer. Returns one line per
/// `Vec` entry.
pub(crate) fn report_lines(session: &Session) -> Vec<String> {
    match session.cache_hit_ratio() {
        None => vec![
            "cache: no provider usage recorded yet this session.".to_string(),
            "  (the provider hasn't reported token usage, or no turn has run)".to_string(),
        ],
        Some(ratio) => {
            let input = session.cumulative_input_tokens;
            let cached = session.cumulative_cached_input_tokens;
            let created = session.cumulative_cache_creation_tokens;
            let mut lines = vec![
                format!("cache hit ratio: {:.1}%", ratio * 100.0),
                format!("  cached input:  {cached} / {input} tokens"),
            ];
            if created > 0 {
                lines.push(format!("  cache writes:  {created} tokens"));
            }
            lines
        }
    }
}

pub(crate) async fn cmd_cache(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let lines = report_lines(ctx.session);
    for (i, line) in lines.iter().enumerate() {
        // First line in the accent/result color; continuation lines dim.
        let color = if i == 0 { c_result() } else { theme::dim() };
        ctx.renderer.write_line(line, color)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_usage_reports_no_data() {
        let s = Session::new("p", "m", 0);
        let lines = report_lines(&s);
        assert!(
            lines[0].contains("no provider usage"),
            "expected no-data line, got: {lines:?}"
        );
    }

    #[test]
    fn reports_ratio_and_counts() {
        let mut s = Session::new("p", "m", 0);
        s.record_token_usage(1000, 800, 0);
        s.record_token_usage(500, 100, 0);
        let lines = report_lines(&s);
        // 900 / 1500 = 60.0%
        assert!(lines[0].contains("60.0%"), "got: {lines:?}");
        assert!(lines[1].contains("900 / 1500"), "got: {lines:?}");
        // No cache-creation line when zero writes.
        assert_eq!(lines.len(), 2, "got: {lines:?}");
    }

    #[test]
    fn shows_cache_writes_line_when_nonzero() {
        let mut s = Session::new("p", "m", 0);
        s.record_token_usage(1000, 200, 300);
        let lines = report_lines(&s);
        assert!(
            lines.iter().any(|l| l.contains("cache writes:  300")),
            "got: {lines:?}"
        );
    }
}
