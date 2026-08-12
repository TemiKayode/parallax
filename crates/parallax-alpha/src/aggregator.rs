use parallax_types::{CanonicalContractId, FairValue, ProbabilityEstimate, Timestamp};

#[derive(Debug, Clone, Copy)]
pub struct AggregatorConfig {
    /// Band half-width in standard deviations. 1.0 ≈ 68% band, 1.65 ≈ 90%.
    pub band_z_score: f64,
    /// Variance floor so a single overconfident source (std_dev ≈ 0)
    /// can't claim near-infinite weight and collapse the band to a point.
    pub min_std_dev: f64,
    /// Estimates older than this many nanoseconds are down-weighted
    /// exponentially — a stale opinion should count for less, not the
    /// same as a fresh one that happens to agree with it.
    pub staleness_half_life_ns: i64,
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        AggregatorConfig {
            band_z_score: 1.0,
            min_std_dev: 0.01,
            staleness_half_life_ns: 30_000_000_000, // 30s
        }
    }
}

/// Combine every source's opinion on one contract into a single fair
/// value with a confidence band that widens with source disagreement,
/// staleness, and (transitively, via each source's own std_dev) low
/// sample size — the generalized form of "weather-forecast disagreement
/// as signal" from design doc §7. Returns `None` for an empty input:
/// no opinion is not the same as a 50% opinion.
pub fn aggregate(
    contract: &CanonicalContractId,
    estimates: &[ProbabilityEstimate],
    config: &AggregatorConfig,
    as_of: Timestamp,
) -> Option<FairValue> {
    if estimates.is_empty() {
        return None;
    }

    let mut weights = Vec::with_capacity(estimates.len());
    let mut weight_sum = 0.0;
    let mut weighted_prob_sum = 0.0;

    for e in estimates {
        let age_ns = as_of.since(e.as_of).max(0) as f64;
        let decay = 0.5f64.powf(age_ns / config.staleness_half_life_ns as f64);
        let effective_std = (e.std_dev.max(config.min_std_dev)) / decay.max(1e-6);
        let variance = effective_std * effective_std;
        let weight = 1.0 / variance;
        weights.push(weight);
        weight_sum += weight;
        weighted_prob_sum += weight * e.probability;
    }

    let midpoint = weighted_prob_sum / weight_sum;

    // Fixed-effects pooled variance: shrinks as more sources agree and confirm.
    let pooled_variance = 1.0 / weight_sum;

    // Weighted dispersion of the individual estimates around the pooled
    // mean: stays large when sources disagree, even if each is
    // individually "confident". Taking the max of the two is what lets
    // genuine cross-model disagreement override an otherwise-tight pool.
    let weighted_dispersion: f64 = estimates
        .iter()
        .zip(weights.iter())
        .map(|(e, w)| w * (e.probability - midpoint).powi(2))
        .sum::<f64>()
        / weight_sum;

    let combined_variance = pooled_variance.max(weighted_dispersion);
    let half_width = config.band_z_score * combined_variance.sqrt();

    Some(FairValue {
        contract: contract.clone(),
        midpoint: midpoint.clamp(0.0, 1.0),
        band_low: (midpoint - half_width).clamp(0.0, 1.0),
        band_high: (midpoint + half_width).clamp(0.0, 1.0),
        as_of,
        inputs: estimates.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::CanonicalContractSpec;

    fn contract() -> CanonicalContractId {
        CanonicalContractSpec {
            event_class: parallax_types::EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: 869,
            direction: parallax_types::Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        }
        .to_id()
    }

    fn est(source: &str, prob: f64, std: f64, ts: i64) -> ProbabilityEstimate {
        ProbabilityEstimate {
            source: source.into(),
            contract: contract(),
            probability: prob,
            std_dev: std,
            as_of: Timestamp::from_nanos(ts),
        }
    }

    #[test]
    fn agreeing_sources_narrow_the_band() {
        let cfg = AggregatorConfig::default();
        let estimates = vec![est("wx", 0.66, 0.03, 0), est("news", 0.65, 0.05, 0)];
        let fv = aggregate(&contract(), &estimates, &cfg, Timestamp::from_nanos(0)).unwrap();
        assert!(fv.band_width() < 0.10, "band was {}", fv.band_width());
    }

    #[test]
    fn disagreeing_sources_widen_the_band() {
        let cfg = AggregatorConfig::default();
        let estimates = vec![est("wx", 0.30, 0.03, 0), est("news", 0.80, 0.03, 0)];
        let fv = aggregate(&contract(), &estimates, &cfg, Timestamp::from_nanos(0)).unwrap();
        assert!(fv.band_width() > 0.30, "band was {}", fv.band_width());
    }

    #[test]
    fn stale_estimate_is_down_weighted() {
        let cfg = AggregatorConfig::default(); // 30s half-life
                                               // Fresh estimate says 0.5, an estimate four half-lives (120s) old
                                               // says 0.9 with equal stated confidence — the aggregate should
                                               // land much closer to the fresh 0.5 once staleness decay is applied.
        let as_of = Timestamp::from_nanos(120_000_000_000);
        let fresh = est("fresh", 0.50, 0.02, 120_000_000_000);
        let stale = est("stale", 0.90, 0.02, 0);
        let fv = aggregate(&contract(), &[fresh, stale], &cfg, as_of).unwrap();
        assert!(fv.midpoint < 0.65, "midpoint was {}", fv.midpoint);
    }

    #[test]
    fn empty_estimates_yield_no_opinion() {
        let cfg = AggregatorConfig::default();
        assert!(aggregate(&contract(), &[], &cfg, Timestamp::from_nanos(0)).is_none());
    }
}
