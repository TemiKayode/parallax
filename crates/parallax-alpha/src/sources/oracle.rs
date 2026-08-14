use crate::source::AlphaSource;
use crate::stats::clamp_probability;
use parallax_types::{
    AlphaEventKind, CanonicalContractId, EstimateKind, ProbabilityEstimate, RawEvent,
    StalenessPolicy,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct OraclePayload {
    contract: String,
    resolved_yes: bool,
    /// When an optimistic-oracle-style challenge window ends, if this
    /// resolution goes through one (UMA's OO resolves by *assertion*: a
    /// proposal stands only if nobody disputes it before this closes).
    /// `None` means a source with no challenge window at all (a venue's
    /// own official settlement) and is treated as immediately final.
    #[serde(default)]
    challenge_window_ends_ns: Option<i64>,
    /// Set once a challenger has disputed the proposal.
    #[serde(default)]
    disputed: bool,
}

/// A resolution-oracle update (UMA, Chainlink, or a venue's own official
/// source) is treated as a near-certainty override rather than just
/// another vote: once it fires, the aggregator's confidence band should
/// collapse toward the edges for that contract, because there is no
/// longer a meaningful "underlying probability" — the outcome is now
/// known (design doc §5, §7).
///
/// UMA's optimistic oracle specifically resolves by *assertion*: a
/// proposal stands only if nobody disputes it before the challenge window
/// closes. Treating every proposal as settled fact the instant it arrives
/// is a bet that nobody will dispute it — exactly the bet that loses on
/// contested outcomes, which are precisely the ones where the market is
/// mispriced and the resulting position is largest (design doc review
/// 2.8). A disputed proposal produces no estimate at all; a still-pending
/// one gets a lower confidence and a wider band, with staleness that
/// still decays like any other unsettled opinion; only a proposal whose
/// window has closed undisputed is treated as a permanent, near-certain
/// fact (design doc review 2.7).
pub struct OracleResolutionSource {
    name: String,
    kinds: [AlphaEventKind; 1],
}

impl OracleResolutionSource {
    pub fn new(name: impl Into<String>) -> Self {
        OracleResolutionSource {
            name: name.into(),
            kinds: [AlphaEventKind::Oracle],
        }
    }
}

impl AlphaSource for OracleResolutionSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn event_kinds(&self) -> &[AlphaEventKind] {
        &self.kinds
    }

    fn on_event(&self, event: &RawEvent) -> Option<ProbabilityEstimate> {
        if event.kind != AlphaEventKind::Oracle {
            return None;
        }
        let payload: OraclePayload = serde_json::from_value(event.payload.clone()).ok()?;

        if payload.disputed {
            return None;
        }

        let now_ns = event.receive_ts.as_nanos();
        let is_finalized = payload
            .challenge_window_ends_ns
            .map(|ends| now_ns >= ends)
            .unwrap_or(true);

        let (probability, std_dev, staleness) = if is_finalized {
            let p = if payload.resolved_yes { 1.0 } else { 0.0 };
            (clamp_probability(p), 0.001, StalenessPolicy::Permanent)
        } else {
            // Still inside the challenge window: directionally informative
            // but explicitly not yet settled — lower confidence, wider
            // band than a finalized resolution, and still subject to
            // ordinary staleness decay.
            let p = if payload.resolved_yes { 0.85 } else { 0.15 };
            (p, 0.08, StalenessPolicy::Decays)
        };

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability,
            std_dev,
            as_of: event.receive_ts,
            kind: EstimateKind::Absolute,
            staleness,
            correlation_group: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::{aggregate, AggregatorConfig};
    use parallax_types::{EstimateKind, StalenessPolicy, Timestamp};
    use serde_json::json;

    fn ev(
        resolved_yes: bool,
        challenge_window_ends_ns: Option<i64>,
        disputed: bool,
        now_ns: i64,
    ) -> RawEvent {
        RawEvent {
            source: "uma".into(),
            kind: AlphaEventKind::Oracle,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(now_ns),
            payload: json!({
                "contract": "wx.temp.chicago.gt_869.2026-08-12.nws_official",
                "resolved_yes": resolved_yes,
                "challenge_window_ends_ns": challenge_window_ends_ns,
                "disputed": disputed,
            }),
        }
    }

    #[test]
    fn a_finalized_resolution_is_permanent_and_near_certain() {
        let src = OracleResolutionSource::new("uma-oo");
        let est = src.on_event(&ev(true, Some(100), false, 200)).unwrap();
        assert_eq!(est.staleness, StalenessPolicy::Permanent);
        assert!(est.probability > 0.99);
    }

    #[test]
    fn a_proposal_still_inside_its_window_is_lower_confidence_and_decays() {
        let src = OracleResolutionSource::new("uma-oo");
        let est = src.on_event(&ev(true, Some(1_000), false, 100)).unwrap();
        assert_eq!(est.staleness, StalenessPolicy::Decays);
        assert!(
            est.probability < 0.99,
            "probability was {}",
            est.probability
        );
        assert!(est.std_dev > 0.01);
    }

    #[test]
    fn a_disputed_proposal_produces_no_estimate_at_all() {
        let src = OracleResolutionSource::new("uma-oo");
        assert!(src.on_event(&ev(true, Some(1_000), true, 100)).is_none());
    }

    #[test]
    fn no_challenge_window_is_treated_as_immediately_final() {
        let src = OracleResolutionSource::new("uma-oo");
        let est = src.on_event(&ev(false, None, false, 0)).unwrap();
        assert_eq!(est.staleness, StalenessPolicy::Permanent);
        assert!(est.probability < 0.01);
    }

    #[test]
    fn oracle_resolution_dominates_and_collapses_the_band() {
        let src = OracleResolutionSource::new("uma-oo");
        let oracle_est = src.on_event(&ev(true, Some(100), false, 200)).unwrap();

        // A prior, uncertain weather estimate that disagreed with the
        // resolution should be overwhelmed once the oracle fires.
        let prior = ProbabilityEstimate {
            source: "hrrr".into(),
            contract: oracle_est.contract.clone(),
            probability: 0.4,
            std_dev: 0.15,
            as_of: Timestamp::from_nanos(0),
            kind: EstimateKind::Absolute,
            staleness: StalenessPolicy::Decays,
            correlation_group: None,
        };

        let fv = aggregate(
            &oracle_est.contract.clone(),
            &[prior, oracle_est],
            &AggregatorConfig::default(),
            Timestamp::from_nanos(200),
        )
        .unwrap();

        assert!(fv.midpoint > 0.95, "midpoint was {}", fv.midpoint);
        assert!(fv.band_width() < 0.05, "band was {}", fv.band_width());
    }
}
