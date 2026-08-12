use serde::{Deserialize, Serialize};
use std::fmt;

/// The class of underlying event a contract resolves on. Deliberately an
/// open-ended string newtype rather than a fixed enum: new event classes
/// (a new weather variable, a new econ series) must be addable without a
/// core recompile, per the "plug in a new alpha source without a restart"
/// requirement.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventClass(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    GreaterThan,
    LessThan,
    Between,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Direction::GreaterThan => "gt",
            Direction::LessThan => "lt",
            Direction::Between => "between",
        };
        write!(f, "{s}")
    }
}

/// The structured fields a venue-specific adapter must resolve its native
/// listing into. This is the normalization contract described in the
/// design doc §6: two venues framing the same bet in different units or
/// strike conventions must map to the identical `CanonicalContractSpec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalContractSpec {
    pub event_class: EventClass,
    pub location: String,
    /// Threshold value normalized to a single canonical unit for this
    /// event class (e.g. weather temperature contracts are always stored
    /// in tenths of a degree Fahrenheit, regardless of how the source
    /// venue quoted them). The adapter, not this struct, owns the
    /// conversion.
    pub threshold: i64,
    pub direction: Direction,
    /// The resolution date/window as an ISO-8601 date string (UTC), e.g. "2026-08-12".
    pub resolution_window: String,
    /// The authority whose data resolves the contract, e.g. "nws_official",
    /// "bls_cpi", "uma_oo". Two venues resolving off different sources for
    /// an otherwise-identical bet are NOT the same canonical contract —
    /// resolution-source risk is real risk.
    pub resolution_source: String,
}

impl CanonicalContractSpec {
    /// Deterministic, human-readable canonical id, e.g.
    /// `wx.temp.chicago.gt.869.2026-08-12.nws_official`
    pub fn to_id(&self) -> CanonicalContractId {
        CanonicalContractId(format!(
            "{}.{}.{}.{}.{}.{}",
            self.event_class.0,
            self.location.to_lowercase().replace(' ', "_"),
            self.direction,
            self.threshold,
            self.resolution_window,
            self.resolution_source,
        ))
    }

    /// Contracts sharing this key are risk-correlated and must be netted
    /// as one exposure by the risk gate (design doc §10) — e.g. three
    /// temperature thresholds on the same city/date share a cluster even
    /// though their `to_id()` differs on threshold.
    pub fn cluster_key(&self) -> ClusterKey {
        ClusterKey(format!(
            "{}.{}.{}",
            self.event_class.0,
            self.location.to_lowercase().replace(' ', "_"),
            self.resolution_window,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CanonicalContractId(pub String);

impl fmt::Display for CanonicalContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClusterKey(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_event_different_units_map_to_same_id() {
        // Polymarket: "Chicago temperature > 30C, Aug 12" normalized by its adapter to °F tenths.
        let from_polymarket = CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "Chicago".into(),
            threshold: 869, // 30C -> 86.9F, stored in tenths
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        };
        // Kalshi: "Chicago high > 86.9F, Aug 12" already native units.
        let from_kalshi = CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: 869,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        };
        assert_eq!(from_polymarket.to_id(), from_kalshi.to_id());
    }

    #[test]
    fn different_thresholds_share_a_cluster() {
        let base = CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "Chicago".into(),
            threshold: 869,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        };
        let mut other = base.clone();
        other.threshold = 900;
        assert_ne!(base.to_id(), other.to_id());
        assert_eq!(base.cluster_key(), other.cluster_key());
    }
}
