use crate::source::AlphaSource;
use crate::stats::{normal_pdf, prob_exceeds};
use parallax_types::{
    AlphaEventKind, CanonicalContractId, EstimateKind, ProbabilityEstimate, RawEvent,
    StalenessPolicy,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct EconPayload {
    contract: String,
    /// Which historical series this release belongs to, e.g. `"cpi_yoy"`
    /// — the key `AlphaConfig::econ.surprise_std_by_series` is keyed on.
    /// Falls back to `contract` when absent, since the two often coincide.
    series: Option<String>,
    /// The contract's threshold, in the same units as `consensus`/`actual`.
    threshold: f64,
    consensus: f64,
    /// Historical standard deviation of (actual - consensus) for this
    /// series — the nowcast's uncertainty before the print lands. Used
    /// only when no offline-fitted override exists for this series in
    /// `AlphaConfig` (design doc review 2.13): a live payload's own
    /// self-reported uncertainty is a fallback, not the source of truth,
    /// once a measured value has actually been calibrated.
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
    correlation_group: Option<String>,
    /// Offline-fitted per-series surprise std, keyed by series id —
    /// overrides whatever a live payload self-reports when present
    /// (design doc review 2.13).
    surprise_std_by_series: std::collections::HashMap<String, f64>,
}

impl EconNowcastSource {
    pub fn new(name: impl Into<String>) -> Self {
        EconNowcastSource {
            name: name.into(),
            kinds: [AlphaEventKind::EconRelease],
            correlation_group: None,
            surprise_std_by_series: std::collections::HashMap::new(),
        }
    }

    /// Builds from the offline-fitted config artifact (design doc review
    /// 2.13): a measured recalibration of a series' surprise std now
    /// actually changes what the hot path uses, rather than sitting in an
    /// exported JSON file nothing reads.
    pub fn from_config(name: impl Into<String>, config: &crate::config::EconConfig) -> Self {
        EconNowcastSource {
            surprise_std_by_series: config.surprise_std_by_series.clone(),
            ..Self::new(name)
        }
    }

    pub fn with_correlation_group(mut self, group: impl Into<String>) -> Self {
        self.correlation_group = Some(group.into());
        self
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
        let series_key = payload.series.as_deref().unwrap_or(&payload.contract);
        let surprise_std = self
            .surprise_std_by_series
            .get(series_key)
            .copied()
            .unwrap_or(payload.surprise_std);

        let (mean, std) = match payload.actual {
            // Pre-release: center on consensus, uncertainty is the
            // series' historical surprise distribution.
            None => (payload.consensus, surprise_std.max(1e-6)),
            // Post-release: center on the printed value, uncertainty
            // collapses to residual revision risk only.
            Some(actual) => (actual, payload.post_release_std.max(1e-6)),
        };

        let probability = prob_exceeds(mean, std, payload.threshold);

        // Delta method: P = Φ(z), z = (mean - threshold) / std, so
        // dP/dz = φ(z) — the probability's sensitivity to one z-scored
        // unit of uncertainty in where the series actually lands. This is
        // scale-invariant by construction (z itself is a ratio, so
        // quoting the same series in different units leaves it
        // unchanged) and, correctly, largest exactly where the link is
        // steepest — consensus sitting right on the threshold — rather
        // than the original `surprise_std / sqrt(|mean|)`, which had no
        // units at all and changed the model's stated confidence just
        // because payrolls were quoted in thousands instead of units
        // (design doc review 2.5).
        let z = (mean - payload.threshold) / std;
        let std_dev = normal_pdf(z).max(1e-4);

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract.clone()),
            probability,
            std_dev,
            as_of: event.receive_ts,
            kind: EstimateKind::Absolute,
            staleness: StalenessPolicy::Decays,
            correlation_group: self.correlation_group.clone(),
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
                "contract": "econ.cpi_yoy.us.gt_30.2026-08-13.bls",
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

    #[test]
    fn reported_confidence_is_unchanged_by_rescaling_the_whole_series() {
        // Regression for design doc review 2.5: quoting the same series
        // 1000x larger (units -> thousands) must not change std_dev.
        let src = EconNowcastSource::new("bls-nowcast");
        let base = RawEvent {
            source: "bls-cpi".into(),
            kind: AlphaEventKind::EconRelease,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: json!({
                "contract": "econ.payrolls.us.gt_200.2026-08-13.bls",
                "threshold": 200.0,
                "consensus": 210.0,
                "surprise_std": 40.0,
                "actual": null,
                "post_release_std": 5.0,
            }),
        };
        let scaled = RawEvent {
            payload: json!({
                "contract": "econ.payrolls.us.gt_200.2026-08-13.bls",
                "threshold": 200_000.0,
                "consensus": 210_000.0,
                "surprise_std": 40_000.0,
                "actual": null,
                "post_release_std": 5_000.0,
            }),
            ..base.clone()
        };

        let base_est = src.on_event(&base).unwrap();
        let scaled_est = src.on_event(&scaled).unwrap();
        assert!(
            (base_est.std_dev - scaled_est.std_dev).abs() < 1e-9,
            "base={} scaled={}",
            base_est.std_dev,
            scaled_est.std_dev
        );
        assert!((base_est.probability - scaled_est.probability).abs() < 1e-9);
    }

    #[test]
    fn confidence_is_highest_when_consensus_sits_on_the_threshold() {
        let src = EconNowcastSource::new("bls-nowcast");
        let on_threshold = src.on_event(&payload_event(3.0, None)).unwrap();
        let far_above = src.on_event(&payload_event(4.0, None)).unwrap();
        assert!(on_threshold.std_dev > far_above.std_dev);
    }

    #[test]
    fn an_offline_fitted_surprise_std_overrides_the_live_payloads_own_value() {
        let mut by_series = std::collections::HashMap::new();
        // Payload says 0.2; the offline-calibrated value says 2.0 — a
        // much wider (less confident) fit.
        by_series.insert("econ.cpi_yoy.us.gt_30.2026-08-13.bls".to_string(), 2.0);
        let config = crate::config::EconConfig {
            surprise_std_by_series: by_series,
        };
        let src = EconNowcastSource::from_config("bls-nowcast", &config);
        let default_src = EconNowcastSource::new("bls-nowcast");

        let overridden = src.on_event(&payload_event(3.1, None)).unwrap();
        let baseline = default_src.on_event(&payload_event(3.1, None)).unwrap();
        assert!(
            overridden.std_dev != baseline.std_dev,
            "an offline override must actually change the emitted estimate"
        );
    }
}
