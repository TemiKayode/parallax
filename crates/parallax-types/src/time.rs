use std::time::{SystemTime, UNIX_EPOCH};

/// Nanoseconds since the Unix epoch. Deliberately a plain integer, not a
/// wall-clock wrapper: the hot path must never touch a syscall to read time
/// on a per-event basis, so timestamps are stamped once at the edge
/// (ingestion, order construction) and carried through as data.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// Never panics: a pre-epoch system clock (a misconfigured container,
    /// a VM that hasn't synced yet) saturates to the epoch rather than
    /// taking the process down. A wrong-but-bounded timestamp is a
    /// staleness-check false positive; a panic on the hot ingestion path
    /// is an outage.
    pub fn now() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        Timestamp(nanos)
    }

    pub fn from_nanos(nanos: i64) -> Self {
        Timestamp(nanos)
    }

    pub fn as_nanos(self) -> i64 {
        self.0
    }

    /// Saturating: an overflowing subtraction (a corrupt or adversarial
    /// timestamp) must not wrap into a small — and therefore falsely
    /// "fresh" — age. Wrapping here is exactly the bug that lets a
    /// malformed timestamp pass every staleness check downstream.
    pub fn since(self, earlier: Timestamp) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_saturates_instead_of_wrapping_on_overflow() {
        let huge_future = Timestamp::from_nanos(i64::MAX);
        let huge_past = Timestamp::from_nanos(i64::MIN);
        // A naive `self.0 - earlier.0` overflows and panics/wraps in debug
        // vs release; saturating_sub must return the clamped max instead.
        assert_eq!(huge_future.since(huge_past), i64::MAX);
    }

    #[test]
    fn since_is_negative_for_a_timestamp_before_the_reference() {
        let earlier = Timestamp::from_nanos(100);
        let later = Timestamp::from_nanos(50);
        assert_eq!(later.since(earlier), -50);
    }
}
