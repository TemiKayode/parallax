use crate::contract::CanonicalContractId;
use crate::time::Timestamp;
use crate::validate::{self, ValidationError};
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
    /// Facts about a prediction market's underlying reference asset itself
    /// — price ticks, order-flow imbalance, a correlated asset's move —
    /// as opposed to facts about the world the contract asks about. Added
    /// for APERTURE (design doc companion, crypto up/down markets): all
    /// three of its alpha sources (reference-distance, order-flow
    /// imbalance, correlated-asset residual) key off this one kind, the
    /// same way every weather source keys off `Weather`.
    ReferenceAsset,
    /// A live state snapshot of a sporting contest whose outcome a
    /// contract resolves on — score, who is serving/possessing, whether
    /// the current point is structurally decisive. Added for the tennis
    /// match-state source: any future sport source (a football match
    /// clock, a cricket innings state) keys off this same kind and
    /// discriminates on the payload's own `sport` field, the same way
    /// every crypto source keys off `ReferenceAsset`.
    SportsMatchState,
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

/// How a `ProbabilityEstimate` should be combined into the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EstimateKind {
    /// A direct opinion about P(YES) — pooled by inverse-variance weighting
    /// against every other absolute estimate.
    Absolute,
    /// A directional nudge in log-odds space, applied additively on top of
    /// the pool of absolute estimates rather than voted into it — the only
    /// representation under which "how confident the source is" and
    /// "which direction it thinks the truth moved" don't fight each other.
    /// An absolute estimate of exactly 0.5 silently anchors the pool
    /// toward a coin flip; a log-odds shift of 0 is a true no-op, and a
    /// fixed-magnitude shift moves a 0.50 contract far more than a 0.97
    /// one, in log-odds space, which is the calibrated behavior (design
    /// doc review 2.6).
    LogOddsShift,
}

/// How this estimate's weight should evolve with `as_of`'s age.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StalenessPolicy {
    /// Down-weighted exponentially with age — the right default for a
    /// still-unsettled forecast or proposal.
    Decays,
    /// Never down-weighted. Appropriate only for a settled fact (a
    /// finalized oracle resolution): the underlying truth does not become
    /// less true as time passes, so decaying it back toward the model's
    /// prior about an event that has already happened is itself the bug
    /// (design doc review 2.7).
    Permanent,
}

/// One alpha source's opinion on one contract: a probability (or a
/// log-odds shift) and its own uncertainty about that number. The
/// aggregator in `parallax-alpha` combines many of these into a single
/// `FairValue`; this type carries no aggregation logic itself, only the
/// estimate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbabilityEstimate {
    pub source: String,
    pub contract: CanonicalContractId,
    /// `EstimateKind::Absolute`: P(contract resolves YES), in `[0.0,
    /// 1.0]`. `EstimateKind::LogOddsShift`: an additive shift in log-odds
    /// space (can be any finite value; 0 is a no-op).
    pub probability: f64,
    /// Standard deviation of `probability`, in the same space `kind`
    /// implies (probability space for `Absolute`, log-odds space for
    /// `LogOddsShift`). Larger = less confident; feeds inverse-variance
    /// weighting in the aggregator and directly widens the resulting
    /// confidence band.
    pub std_dev: f64,
    pub as_of: Timestamp,
    pub kind: EstimateKind,
    pub staleness: StalenessPolicy,
    /// Estimates sharing a `Some(group)` are not independent evidence —
    /// five ensemble members sharing initial conditions, or six outlets
    /// reprinting one wire story — and the aggregator haircuts their
    /// combined weight accordingly rather than pooling them as `N`
    /// independent observations (design doc review 2.9). `None` means
    /// "independent of everything else."
    pub correlation_group: Option<String>,
}

impl ProbabilityEstimate {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self.kind {
            EstimateKind::Absolute => validate::probability("probability", self.probability)?,
            EstimateKind::LogOddsShift => validate::finite("probability", self.probability)?,
        }
        validate::positive("std_dev", self.std_dev)?;
        Ok(())
    }
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
    /// Effective independent sample size after the correlation haircut
    /// (design doc review 2.9): `N` genuinely independent sources ≈ `N`;
    /// `N` sources all at correlation ρ ≈ 1 regardless of `N`. Engines can
    /// refuse to trade below a floor rather than trusting a band that
    /// looks tight only because it pooled correlated noise as if it were
    /// independent evidence.
    pub effective_sample_size: f64,
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

    /// Asserts the invariants every constructor is supposed to guarantee:
    /// every number is a finite probability, and the midpoint actually
    /// lies inside its own band. A band that can drift outside `[0,1]` or
    /// a midpoint that clamped to one side while the band clamped to the
    /// other both read, downstream, as "every price is outside the band"
    /// — i.e. a permanent trading signal (design doc review 2.11).
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate::probability("midpoint", self.midpoint)?;
        validate::probability("band_low", self.band_low)?;
        validate::probability("band_high", self.band_high)?;
        const EPS: f64 = 1e-9;
        if self.midpoint < self.band_low - EPS || self.midpoint > self.band_high + EPS {
            return Err(ValidationError {
                field: "midpoint",
                message: format!(
                    "midpoint {} must lie within [{}, {}]",
                    self.midpoint, self.band_low, self.band_high
                ),
            });
        }
        validate::non_negative("effective_sample_size", self.effective_sample_size)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> CanonicalContractId {
        CanonicalContractId("wx.temp.chicago.gt_869.2026-08-12.nws_official".into())
    }

    fn fv(midpoint: f64, band_low: f64, band_high: f64) -> FairValue {
        FairValue {
            contract: contract(),
            midpoint,
            band_low,
            band_high,
            as_of: Timestamp::from_nanos(0),
            inputs: vec![],
            effective_sample_size: 1.0,
        }
    }

    #[test]
    fn validate_accepts_a_well_formed_fair_value() {
        assert!(fv(0.6, 0.5, 0.7).validate().is_ok());
    }

    #[test]
    fn validate_rejects_a_midpoint_outside_its_own_band() {
        assert!(fv(0.9, 0.5, 0.7).validate().is_err());
    }

    #[test]
    fn validate_rejects_out_of_range_probabilities() {
        assert!(fv(1.2, 0.5, 1.2).validate().is_err());
    }

    #[test]
    fn absolute_estimate_requires_a_probability_in_range() {
        let est = ProbabilityEstimate {
            source: "x".into(),
            contract: contract(),
            probability: 1.5,
            std_dev: 0.1,
            as_of: Timestamp::from_nanos(0),
            kind: EstimateKind::Absolute,
            staleness: StalenessPolicy::Decays,
            correlation_group: None,
        };
        assert!(est.validate().is_err());
    }

    #[test]
    fn log_odds_shift_estimate_allows_any_finite_value() {
        let est = ProbabilityEstimate {
            source: "x".into(),
            contract: contract(),
            probability: -3.5,
            std_dev: 0.1,
            as_of: Timestamp::from_nanos(0),
            kind: EstimateKind::LogOddsShift,
            staleness: StalenessPolicy::Decays,
            correlation_group: None,
        };
        assert!(est.validate().is_ok());
    }

    #[test]
    fn zero_or_negative_std_dev_is_rejected() {
        let est = ProbabilityEstimate {
            source: "x".into(),
            contract: contract(),
            probability: 0.5,
            std_dev: 0.0,
            as_of: Timestamp::from_nanos(0),
            kind: EstimateKind::Absolute,
            staleness: StalenessPolicy::Decays,
            correlation_group: None,
        };
        assert!(est.validate().is_err());
    }
}
