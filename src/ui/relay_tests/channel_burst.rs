#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;

    /// 1000 events at 1ms intervals (~1000/s).
    #[test]
    fn event_channel_sustained() {
        channel_stress("event_channel_sustained", 1000, 1_000);
    }

    /// 5000 events with no gap — full-speed burst.
    #[test]
    fn event_channel_burst() {
        channel_stress("event_channel_burst", 5000, 0);
    }
}
