use std::time::Duration;

/// First retry is immediate; subsequent retries have capped exponential jitter.
pub(super) fn delay(failures: u32, max_seconds: u64) -> Duration {
    if failures <= 1 {
        return Duration::ZERO;
    }
    let ceiling = 1_u64
        .checked_shl((failures - 2).min(63))
        .unwrap_or(u64::MAX)
        .saturating_mul(1000)
        .min(max_seconds.saturating_mul(1000));
    let lower = ceiling / 2;
    Duration::from_millis(rand::random_range(lower..=ceiling))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_is_immediate_then_bounded_even_after_overflow() {
        assert_eq!(delay(1, 30), Duration::ZERO);
        for failures in 2..200 {
            let value = delay(failures, 30);
            assert!(value <= Duration::from_secs(30));
            assert!(value >= Duration::from_millis(500));
        }
        assert_eq!(delay(u32::MAX, 0), Duration::ZERO);
    }

    #[test]
    fn retry_jitter_stays_inside_each_exponential_window_and_cap() {
        for (failures, cap, minimum_ms, maximum_ms) in [
            (0, 30, 0, 0),
            (1, 30, 0, 0),
            (2, 30, 500, 1000),
            (3, 30, 1000, 2000),
            (4, 30, 2000, 4000),
            (7, 30, 15000, 30000),
            (20, 1, 500, 1000),
            (u32::MAX, 0, 0, 0),
            (u32::MAX, u64::MAX, u64::MAX / 2, u64::MAX),
        ] {
            for _ in 0..32 {
                let value = delay(failures, cap);
                assert!(
                    (Duration::from_millis(minimum_ms)..=Duration::from_millis(maximum_ms))
                        .contains(&value),
                    "failure {failures}, cap {cap}: {value:?}"
                );
            }
        }
    }
}
