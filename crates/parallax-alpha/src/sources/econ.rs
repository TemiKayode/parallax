use crate::source::AlphaSource;
use crate::stats::prob_exceeds;
use parallax_types::{AlphaEventKind, CanonicalContractId, ProbabilityEstimate, RawEvent};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct EconPayload {
    contract: String,
    /// The contract's threshold, in the same units as `consensus`/`actual`.
    threshold: f64,
    consensus: f64,
    /// Historical standard deviation of (actual - consensus) for this
    /// series — the nowcast's uncertainty before the print lands.
    surprise_std: f64,
    /// Present once the release has actually printed; `None` pre-release.
    actual: Option<f64>,
    /// Residual measurement/revision uncertainty once `actual` is known
    /// (initial prints do get revised). Small but nonzero.
    post_release_std: f64,
}

/// Prices a scheduled economic release against its own historical
/// surprise distribution — the same "domain-specific nowcast" principle
/// as the weather source, applied to a series with a scheduled print
/// instead of a continuous forecast (design doc §5, §7).
pub struct EconNowcastSource {
    name: String,
    kinds: [AlphaEventKind; 1],
}

impl EconNowcastSource {
    pub fn new(name: impl Into<String>) -> Self {
        EconNowcastSource {
            name: name.into(),
            kinds: [AlphaEventKind::EconRelease],
        }
    }
}

impl AlphaSource for EconNowcastSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn event_kinds(&self) -> &[AlphaEventKind] {
        &self.kinds
    }

    fn on_event(&self, event: &RawEvent) -> Option<ProbabilityEstimate> {
        if event.kind != AlphaEventKind::EconRelease {
            return None;
        }
        let payload: EconPayload = serde_json::from_value(event.payload.clone()).ok()?;

        let (mean, std) = match payload.actual {
            // Pre-release: center on consensus, uncertainty is the
            // series' historical surprise distribution.
            None => (payload.consensus, payload.surprise_std.max(1e-6)),
            // Post-release: center on the printed value, uncertainty
            // collapses to residual revision risk only.
            Some(actual) => (actual, payload.post_release_std.max(1e-6)),
        };

        let probability = prob_exceeds(mean, std, payload.threshold);

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability,
            std_dev: std / mean.abs().max(1.0).sqrt(), // normalize into probability-space scale
            as_of: event.receive_ts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::Timestamp;
    use serde_json::json;

    fn payload_event(consensus: f64, actual: Option<f64>) -> RawEvent {
        RawEvent {
            source: "bls-cpi".into(),
            kind: AlphaEventKind::EconRelease,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: json!({
                "contract": "econ.cpi_yoy.us.gt.30.2026-08-13.bls",
                "threshold": 3.0,
                "consensus": consensus,
                "surprise_std": 0.2,
                "actual": actual,
                "post_release_std": 0.02,
            }),
        }
    }

    #[test]
    fn consensus_well_above_threshold_implies_high_probability() {
        let src = EconNowcastSource::new("bls-nowcast");
        let est = src.on_event(&payload_event(3.5, None)).unwrap();
        assert!(est.probability > 0.9, "probability was {}", est.probability);
    }

    #[test]
    fn actual_release_sharply_tightens_uncertainty() {
        let src = EconNowcastSource::new("bls-nowcast");
        let pre = src.on_event(&payload_event(3.1, None)).unwrap();
        let post = src.on_event(&payload_event(3.1, Some(3.4))).unwrap();
        assert!(post.std_dev < pre.std_dev);
    }
}
