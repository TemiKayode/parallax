use crate::source::AlphaSource;
use parallax_types::{AlphaEventKind, CanonicalContractId, ProbabilityEstimate, RawEvent};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct OraclePayload {
    contract: String,
    resolved_yes: bool,
}

/// A resolution-oracle update (UMA, Chainlink, or a venue's own official
/// source) is treated as a near-certainty override rather than just
/// another vote: once it fires, the aggregator's confidence band should
/// collapse toward zero for that contract, because there is no longer a
/// meaningful "underlying probability" — the outcome is now known
/// (design doc §5, §7). `std_dev` is small but nonzero to keep downstream
/// variance arithmetic well-defined.
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

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability: if payload.resolved_yes { 1.0 } else { 0.0 },
            std_dev: 0.001,
            as_of: event.receive_ts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::{aggregate, AggregatorConfig};
    use parallax_types::Timestamp;
    use serde_json::json;

    #[test]
    fn oracle_resolution_dominates_and_collapses_the_band() {
        let src = OracleResolutionSource::new("uma-oo");
        let ev = RawEvent {
            source: "uma".into(),
            kind: AlphaEventKind::Oracle,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: json!({
                "contract": "wx.temp.chicago.gt.869.2026-08-12.nws_official",
                "resolved_yes": true,
            }),
        };
        let oracle_est = src.on_event(&ev).unwrap();

        // A prior, uncertain weather estimate that disagreed with the
        // resolution should be overwhelmed once the oracle fires.
        let prior = ProbabilityEstimate {
            source: "hrrr".into(),
            contract: oracle_est.contract.clone(),
            probability: 0.4,
            std_dev: 0.15,
            as_of: Timestamp::from_nanos(0),
        };

        let fv = aggregate(
            &oracle_est.contract.clone(),
            &[prior, oracle_est],
            &AggregatorConfig::default(),
            Timestamp::from_nanos(0),
        )
        .unwrap();

        assert!(fv.midpoint > 0.95, "midpoint was {}", fv.midpoint);
        assert!(fv.band_width() < 0.05, "band was {}", fv.band_width());
    }
}
