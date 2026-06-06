#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;

    /// 100 events at 10ms intervals (~100/s).
    #[test]
    fn event_channel_slow_typist() {
        channel_stress("event_channel_slow_typist", 100, 10_000);
    }

    /// 500 events at 2ms intervals (~500/s).
    #[test]
    fn event_channel_fast_typist() {
        channel_stress("event_channel_fast_typist", 500, 2_000);
    }
}
