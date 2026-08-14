use serde::{Deserialize, Serialize};
use std::fmt;

/// The class of underlying event a contract resolves on. Deliberately an
/// open-ended string newtype rather than a fixed enum: new event classes
/// (a new weather variable, a new econ series) must be addable without a
/// core recompile, per the "plug in a new alpha source without a restart"
/// requirement.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventClass(pub String);

/// `Between` carries its own upper bound rather than being a unit variant:
/// "temp between 70 and 80" and "temp between 70 and 90" are different bets
/// and must not collapse to the same canonical id just because the lower
/// `threshold` on `CanonicalContractSpec` happens to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    GreaterThan,
    LessThan,
    Between { upper: i64 },
}

impl Direction {
    fn tag(&self) -> &'static str {
        match self {
            Direction::GreaterThan => "gt",
            Direction::LessThan => "lt",
            Direction::Between { .. } => "between",
        }
    }

    /// Encodes this direction together with the spec's lower `threshold`
    /// into the single id component the id format allots it — `gt_869`,
    /// `lt_869`, `between_869_900`. Using `_` rather than `.` keeps this
    /// sub-structure inside one component of the outer `.`-joined id, so
    /// the top-level parser never has to know direction has an arity of
    /// its own.
    fn token(&self, threshold: i64) -> String {
        match self {
            Direction::GreaterThan | Direction::LessThan => format!("{}_{threshold}", self.tag()),
            Direction::Between { upper } => format!("between_{threshold}_{upper}"),
        }
    }

    fn parse_token(token: &str) -> Option<(Direction, i64)> {
        let parts: Vec<&str> = token.split('_').collect();
        match parts.as_slice() {
            ["gt", t] => Some((Direction::GreaterThan, t.parse().ok()?)),
            ["lt", t] => Some((Direction::LessThan, t.parse().ok()?)),
            ["between", t, u] => Some((
                Direction::Between {
                    upper: u.parse().ok()?,
                },
                t.parse().ok()?,
            )),
            _ => None,
        }
    }

    /// Every axis this direction is net-long in a naive signed sum. `Between`
    /// is non-monotone in price — it is not simply "long" or "short" the
    /// underlying the way `GreaterThan`/`LessThan` are — so a long position
    /// in a `Between` contract nets grossly (as if it were its own
    /// direction) against other slots in the same cluster rather than
    /// cancelling them, which is conservative rather than wrong (design doc
    /// / review §3.24).
    pub fn exposure_sign(&self) -> f64 {
        match self {
            Direction::GreaterThan => 1.0,
            Direction::LessThan => -1.0,
            Direction::Between { .. } => 0.0,
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.tag())
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
    /// venue quoted them). For `Direction::Between`, this is the lower
    /// bound; `Direction::Between::upper` carries the other end.
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

/// Escapes the two characters that would otherwise be ambiguous inside a
/// `.`-joined id: a literal `.` (which would look like a component
/// boundary) and `%` itself (so the escaping is reversible). `event_class`
/// is deliberately never run through this — it is allowed genuine internal
/// dots (`wx.temp`, `econ.cpi_yoy`) because it sits leftmost and the parser
/// always peels a fixed number of escaped fields off the *right* first,
/// treating whatever remains as `event_class` verbatim.
fn escape_component(s: &str) -> String {
    s.replace('%', "%25").replace('.', "%2E")
}

fn unescape_component(s: &str) -> String {
    s.replace("%2E", ".").replace("%25", "%")
}

impl CanonicalContractSpec {
    /// Deterministic, human-readable canonical id, e.g.
    /// `wx.temp.chicago.gt_869.2026-08-12.nws_official`. Every component
    /// except `event_class` is case-folded and `.`-escaped so that
    /// `nws_official`/`NWS_Official` and `st. louis`/`st.louis` cannot
    /// collide into, or split across, a different id.
    pub fn to_id(&self) -> CanonicalContractId {
        let location = escape_component(&self.location.to_lowercase().replace(' ', "_"));
        let direction_token = self.direction.token(self.threshold);
        let window = escape_component(&self.resolution_window.to_lowercase());
        let source = escape_component(&self.resolution_source.to_lowercase());
        CanonicalContractId(format!(
            "{}.{location}.{direction_token}.{window}.{source}",
            self.event_class.0,
        ))
    }

    /// Contracts sharing this key are risk-correlated and must be netted
    /// as one exposure by the risk gate (design doc §10) — e.g. three
    /// temperature thresholds on the same city/date share a cluster even
    /// though their `to_id()` differs on threshold.
    pub fn cluster_key(&self) -> ClusterKey {
        let location = escape_component(&self.location.to_lowercase().replace(' ', "_"));
        let window = escape_component(&self.resolution_window.to_lowercase());
        ClusterKey(format!("{}.{location}.{window}", self.event_class.0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CanonicalContractId(pub String);

impl fmt::Display for CanonicalContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl CanonicalContractId {
    /// Splits into `(event_class, location, direction_token, window,
    /// source)` by peeling four escaped fields off the right — the same
    /// right-anchored parse `to_id` was built around, tolerant of
    /// `event_class` containing its own literal dots.
    fn rsplit_fields(&self) -> Option<(&str, &str, &str, &str, &str)> {
        let parts: Vec<&str> = self.0.rsplitn(5, '.').collect();
        match parts.as_slice() {
            [source, window, direction, location, event_class] => {
                Some((event_class, location, direction, window, source))
            }
            _ => None,
        }
    }

    /// Recovers the structured `Direction` this id was built from, if the
    /// id is well-formed. `None` for a hand-written or malformed id rather
    /// than a panic — this is a fallback path, not a hot one.
    pub fn direction(&self) -> Option<Direction> {
        let (_, _, direction_token, _, _) = self.rsplit_fields()?;
        Direction::parse_token(direction_token).map(|(d, _)| d)
    }

    /// Recovers the lower `threshold` this id was built from.
    pub fn threshold(&self) -> Option<i64> {
        let (_, _, direction_token, _, _) = self.rsplit_fields()?;
        Direction::parse_token(direction_token).map(|(_, t)| t)
    }

    pub fn location(&self) -> Option<String> {
        let (_, location, _, _, _) = self.rsplit_fields()?;
        Some(unescape_component(location))
    }

    pub fn resolution_source(&self) -> Option<String> {
        let (_, _, _, _, source) = self.rsplit_fields()?;
        Some(unescape_component(source))
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

    #[test]
    fn between_with_different_upper_bounds_are_different_contracts() {
        let base = CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: 70,
            direction: Direction::Between { upper: 80 },
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        };
        let mut other = base.clone();
        other.direction = Direction::Between { upper: 90 };
        assert_ne!(
            base.to_id(),
            other.to_id(),
            "different upper bounds must not collide"
        );
        assert_eq!(
            base.to_id().direction(),
            Some(Direction::Between { upper: 80 })
        );
        assert_eq!(
            other.to_id().direction(),
            Some(Direction::Between { upper: 90 })
        );
    }

    #[test]
    fn case_folding_makes_resolution_source_collide_as_intended() {
        let lower = CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: 869,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        };
        let mut upper = lower.clone();
        upper.resolution_source = "NWS_Official".into();
        assert_eq!(
            lower.to_id(),
            upper.to_id(),
            "differently-cased resolution sources must be the same contract"
        );
    }

    #[test]
    fn a_literal_dot_in_a_free_text_field_does_not_collide_with_a_different_split() {
        // event_class "wx" + location "temp.chicago" must not equal
        // event_class "wx.temp" + location "chicago" — the whole point of
        // escaping the non-event_class fields.
        let a = CanonicalContractSpec {
            event_class: EventClass("wx".into()),
            location: "temp.chicago".into(),
            threshold: 869,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        };
        let b = CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: 869,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        };
        assert_ne!(a.to_id(), b.to_id());
        assert_eq!(a.to_id().location(), Some("temp.chicago".to_string()));
        assert_eq!(b.to_id().location(), Some("chicago".to_string()));
    }

    #[test]
    fn direction_and_threshold_round_trip_through_the_id() {
        let spec = CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: -50,
            direction: Direction::LessThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        };
        let id = spec.to_id();
        assert_eq!(id.direction(), Some(Direction::LessThan));
        assert_eq!(id.threshold(), Some(-50));
    }

    #[test]
    fn exposure_sign_is_zero_for_between_and_opposite_for_gt_lt() {
        assert_eq!(Direction::GreaterThan.exposure_sign(), 1.0);
        assert_eq!(Direction::LessThan.exposure_sign(), -1.0);
        assert_eq!(Direction::Between { upper: 10 }.exposure_sign(), 0.0);
    }
}
