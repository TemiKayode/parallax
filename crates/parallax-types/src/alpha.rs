use crate::contract::CanonicalContractId;
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};

/// The domain an external signal belongs to. `AlphaSource` implementations
/// declare which kinds they consume so the ingestion bus can route without
/// every source parsing every message (design doc §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlphaEventKind {
    Weather,
    EconRelease,
    NewsHeadline,
    Oracle,
}

/// The common envelope every external fact arrives in, regardless of
/// source cadence — a scheduled econ release and a continuous weather
/// observation both become one of these before touching the ring buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEvent {
    pub source: String,
    pub kind: AlphaEventKind,
    /// When the source itself says this fact became true/known, if known.
    pub publish_ts: Option<Timestamp>,
    /// When PARALLAX observed it. Confidence-band widening is driven off
    /// `receive_ts`, never `publish_ts` — we cannot be faster than our own
    /// clock.
    pub receive_ts: Timestamp,
    pub payload: serde_json::Value,
}

/// One alpha source's opinion on one contract: a probability and its own
/// uncertainty about that probability. The aggregator in `parallax-alpha`
/// combines many of these into a single `FairValue`; this type carries no
/// aggregation logic itself, only the estimate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbabilityEstimate {
    pub source: String,
    pub contract: CanonicalContractId,
    /// P(contract resolves YES), in `[0.0, 1.0]`.
    pub probability: f64,
    /// Standard deviation of `probability`. Larger = less confident; feeds
    /// inverse-variance weighting in the aggregator and directly widens
    /// the resulting confidence band.
    pub std_dev: f64,
    pub as_of: Timestamp,
}

/// The single number the strategy core actually consumes: an aggregated
/// probability plus a confidence band wide enough to reflect model
/// disagreement, staleness, and low sample size (design doc §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FairValue {
    pub contract: CanonicalContractId,
    pub midpoint: f64,
    pub band_low: f64,
    pub band_high: f64,
    pub as_of: Timestamp,
    pub inputs: Vec<ProbabilityEstimate>,
}

impl FairValue {
    pub fn band_width(&self) -> f64 {
        (self.band_high - self.band_low).max(0.0)
    }

    /// True when `price` sits fully outside the confidence band — the
    /// trigger condition for the stat-arb engine (design doc §8), not
    /// merely away from the midpoint.
    pub fn is_outside_band(&self, price: f64) -> bool {
        price < self.band_low || price > self.band_high
    }
}
