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
    pub fn now() -> Self {
        let dur = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch");
        Timestamp(dur.as_nanos() as i64)
    }

    pub fn from_nanos(nanos: i64) -> Self {
        Timestamp(nanos)
    }

    pub fn as_nanos(self) -> i64 {
        self.0
    }

    pub fn since(self, earlier: Timestamp) -> i64 {
        self.0 - earlier.0
    }
}
