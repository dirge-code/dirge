//! Prompt-cache TTL policy (dirge-cbgz).
//!
//! Anthropic offers two cache lifetimes on the automatic (top-level `cache_control`)
//! breakpoint, and the choice is a real cost decision rather than a detail:
//!
//! | | cache write | cache read |
//! |---|---|---|
//! | 5m (default) | 1.25× base input | 0.1× |
//! | 1h | 2× base input | 0.1× |
//!
//! A read refreshes the 5m entry, so a live agent loop keeps a 5m prefix warm
//! indefinitely and never pays the 1h premium's 0.75× surcharge. The premium is earned
//! only across idle gaps longer than five minutes — a user reading a diff, taking a
//! call — where a lapsed 5m entry means re-writing the whole prefix instead of the
//! turn's delta. Which of those dominates depends on how the session is used, so it is
//! a knob rather than a constant. `/cache` reports the cumulative write and read totals
//! that tell you which side you are on.
//!
//! Read from `[prompt_cache] ttl` in the config file, overridable at runtime with
//! `DIRGE_PROMPT_CACHE_TTL`. The default is 1h, which is what dirge has shipped since
//! prompt caching was enabled.

/// Process-global TTL, seeded once at startup from `[prompt_cache]`.
static TTL: std::sync::OnceLock<CacheTtl> = std::sync::OnceLock::new();

/// A cache lifetime Anthropic accepts. These are the only two values the API takes;
/// anything else is a 400, so an unparseable setting falls back to the default rather
/// than reaching the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    /// `{"type": "ephemeral"}` — the API default, refreshed on every read.
    FiveMinutes,
    /// `{"type": "ephemeral", "ttl": "1h"}`.
    #[default]
    OneHour,
}

impl CacheTtl {
    /// The wire spelling, or `None` for 5m — the API's own default, which is expressed
    /// by omitting `ttl` rather than by sending `"5m"`.
    pub fn wire_ttl(self) -> Option<&'static str> {
        match self {
            CacheTtl::FiveMinutes => None,
            CacheTtl::OneHour => Some("1h"),
        }
    }

    /// Parse a config/env spelling. Accepts the two wire values plus the spellings a
    /// user is likely to reach for; `None` when it isn't one of them.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "5m" | "5min" | "300" | "default" | "short" => Some(CacheTtl::FiveMinutes),
            "1h" | "60m" | "3600" | "long" => Some(CacheTtl::OneHour),
            _ => None,
        }
    }
}

/// Seed the TTL from the loaded runtime config. Call ONCE at startup, BEFORE any
/// provider client or stream fn is built. An unrecognized value warns and keeps the
/// default instead of failing the launch: a bad TTL should not stop dirge from running.
pub fn init_from_config(ttl: Option<&str>) {
    let resolved = match ttl {
        None => CacheTtl::default(),
        Some(raw) => CacheTtl::parse(raw).unwrap_or_else(|| {
            tracing::warn!(
                target: "dirge::provider",
                value = %raw,
                "unrecognized [prompt_cache] ttl; using the default (1h). Valid values: 5m, 1h",
            );
            CacheTtl::default()
        }),
    };
    let _ = TTL.set(resolved);
}

/// The TTL to put on an automatic cache breakpoint.
///
/// `DIRGE_PROMPT_CACHE_TTL` wins over the config file, and is read per call so it can be
/// set after startup (the same convention `DIRGE_COMPRESSION_PRESET` follows). Defaults to
/// 1h when neither is set, including when `init_from_config` never ran — a non-CLI
/// embedding of these modules then behaves like the shipped default.
pub fn ttl() -> CacheTtl {
    if let Ok(raw) = std::env::var("DIRGE_PROMPT_CACHE_TTL")
        && let Some(parsed) = CacheTtl::parse(&raw)
    {
        return parsed;
    }
    TTL.get().copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_two_wire_values_and_common_spellings() {
        assert_eq!(CacheTtl::parse("5m"), Some(CacheTtl::FiveMinutes));
        assert_eq!(CacheTtl::parse("1h"), Some(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse(" 1H "), Some(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse("60m"), Some(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse("short"), Some(CacheTtl::FiveMinutes));
    }

    #[test]
    fn rejects_values_the_api_would_reject() {
        // Anthropic takes only 5m and 1h; anything else is a 400, so it must never
        // reach the wire as a TTL.
        for raw in ["10m", "2h", "ephemeral", "", "true"] {
            assert_eq!(CacheTtl::parse(raw), None, "{raw} must not parse");
        }
    }

    #[test]
    fn five_minutes_is_expressed_by_omitting_ttl() {
        assert_eq!(CacheTtl::FiveMinutes.wire_ttl(), None);
        assert_eq!(CacheTtl::OneHour.wire_ttl(), Some("1h"));
    }

    #[test]
    fn default_is_one_hour() {
        // The value dirge has shipped since caching was enabled; changing it is a
        // pricing change and wants measurement, not a default flip.
        assert_eq!(CacheTtl::default(), CacheTtl::OneHour);
    }
}
