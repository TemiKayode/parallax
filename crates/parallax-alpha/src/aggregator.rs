use crate::stats::clamp_probability;
use parallax_types::{
    CanonicalContractId, EstimateKind, FairValue, ProbabilityEstimate, StalenessPolicy, Timestamp,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy)]
pub struct AggregatorConfig {
    /// Band half-width in standard deviations. 1.0 ≈ 68% band, 1.65 ≈ 90%.
    pub band_z_score: f64,
    /// Variance floor so a single overconfident source (std_dev ≈ 0)
    /// can't claim near-infinite weight and collapse the band to a point.
    pub min_std_dev: f64,
    /// Estimates older than this many nanoseconds are down-weighted
    /// exponentially — a stale opinion should count for less, not the
    /// same as a fresh one that happens to agree with it. Only applies to
    /// `StalenessPolicy::Decays` estimates; `Permanent` ones (a finalized
    /// oracle resolution) never decay (design doc review 2.7).
    pub staleness_half_life_ns: i64,
    /// Assumed within-group correlation for estimates sharing a
    /// `correlation_group` — five ensemble members sharing initial
    /// conditions, or six outlets reprinting one wire story, are not five
    /// or six independent observations. Their combined weight is
    /// haircut by `k / (1 + (k-1)·ρ)` rather than pooled as `k`
    /// independent samples (design doc review 2.9).
    pub assumed_correlation: f64,
    /// Floor on the reported band's half-width. Without this, a
    /// midpoint and band that both clamp toward the same edge of `[0,1]`
    /// from a malformed or extreme input can collapse the band to a
    /// single point — after which *every* market price reads as "outside
    /// the band," i.e. as a trading signal (design doc review 2.11).
    pub min_band_half_width: f64,
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        AggregatorConfig {
            band_z_score: 1.0,
            min_std_dev: 0.01,
            staleness_half_life_ns: 30_000_000_000, // 30s
            assumed_correlation: 0.6,
            min_band_half_width: 0.005,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateError {
    /// Nothing survived the contract-match filter and/or the input was
    /// empty — no opinion is not the same as a 50% opinion, so this is an
    /// explicit error rather than a silent fallback midpoint (design doc
    /// review 2.12).
    NoUsableEstimates,
}

fn effective_weight(
    e: &ProbabilityEstimate,
    as_of: Timestamp,
    config: &AggregatorConfig,
    group_sizes: &HashMap<&str, usize>,
) -> f64 {
    let effective_std = match e.staleness {
        StalenessPolicy::Permanent => e.std_dev.max(config.min_std_dev),
        StalenessPolicy::Decays => {
            let age_ns = as_of.since(e.as_of).max(0) as f64;
            let decay = 0.5f64.powf(age_ns / config.staleness_half_life_ns as f64);
            (e.std_dev.max(config.min_std_dev)) / decay.max(1e-6)
        }
    };
    let mut weight = 1.0 / (effective_std * effective_std);
    if let Some(group) = e.correlation_group.as_deref() {
        let k = *group_sizes.get(group).unwrap_or(&1) as f64;
        if k > 1.0 {
            weight /= 1.0 + (k - 1.0) * config.assumed_correlation;
        }
    }
    weight
}

/// Combine every source's opinion on one contract into a single fair
/// value with a confidence band that widens with source disagreement,
/// staleness, and low *effective* (correlation-adjusted) sample size —
/// the generalized form of "weather-forecast disagreement as signal"
/// from design doc §7. Returns `None` for an empty input, or once
/// estimates whose `contract` doesn't match `contract` are dropped and
/// nothing usable remains (design doc review 2.12) — use `try_aggregate`
/// for a typed error instead of `None`.
pub fn aggregate(
    contract: &CanonicalContractId,
    estimates: &[ProbabilityEstimate],
    config: &AggregatorConfig,
    as_of: Timestamp,
) -> Option<FairValue> {
    let usable: Vec<&ProbabilityEstimate> = estimates
        .iter()
        .filter(|e| &e.contract == contract)
        .collect();
    if usable.is_empty() {
        return None;
    }

    let mut group_sizes: HashMap<&str, usize> = HashMap::new();
    for e in &usable {
        if let Some(g) = e.correlation_group.as_deref() {
            *group_sizes.entry(g).or_insert(0) += 1;
        }
    }

    let absolutes: Vec<&ProbabilityEstimate> = usable
        .iter()
        .copied()
        .filter(|e| e.kind == EstimateKind::Absolute)
        .collect();
    let shifts: Vec<&ProbabilityEstimate> = usable
        .iter()
        .copied()
        .filter(|e| e.kind == EstimateKind::LogOddsShift)
        .collect();

    // A directional nudge on its own can't establish where the level
    // actually sits — there must be at least one absolute opinion to
    // shift.
    if absolutes.is_empty() {
        return None;
    }

    let mut weights = Vec::with_capacity(absolutes.len());
    let mut weight_sum = 0.0;
    let mut weighted_prob_sum = 0.0;
    for e in &absolutes {
        let w = effective_weight(e, as_of, config, &group_sizes);
        weights.push(w);
        weight_sum += w;
        weighted_prob_sum += w * e.probability;
    }
    let base_midpoint = weighted_prob_sum / weight_sum;

    // Fixed-effects pooled variance: shrinks as more sources agree and
    // confirm. Weighted dispersion: stays large when sources disagree,
    // even if each is individually "confident." Taking the max of the
    // two is what lets genuine cross-model disagreement override an
    // otherwise-tight pool.
    let pooled_variance = 1.0 / weight_sum;
    let weighted_dispersion: f64 = absolutes
        .iter()
        .zip(weights.iter())
        .map(|(e, w)| w * (e.probability - base_midpoint).powi(2))
        .sum::<f64>()
        / weight_sum;
    let combined_variance = pooled_variance.max(weighted_dispersion);

    let mut counted_groups: HashSet<&str> = HashSet::new();
    let mut effective_sample_size = 0.0;
    for e in &absolutes {
        match e.correlation_group.as_deref() {
            None => effective_sample_size += 1.0,
            Some(g) => {
                if counted_groups.insert(g) {
                    let k = *group_sizes.get(g).unwrap_or(&1) as f64;
                    effective_sample_size += k / (1.0 + (k - 1.0) * config.assumed_correlation);
                }
            }
        }
    }

    // Log-odds shifts pool separately, by the same inverse-variance
    // logic, then apply once as an additive nudge on top of the absolute
    // pool's midpoint — never voted into that pool directly (design doc
    // review 2.6).
    let mut shift_weight_sum = 0.0;
    let mut shift_weighted_sum = 0.0;
    for e in &shifts {
        let w = effective_weight(e, as_of, config, &group_sizes);
        shift_weight_sum += w;
        shift_weighted_sum += w * e.probability;
    }
    let combined_shift = if shift_weight_sum > 0.0 {
        shift_weighted_sum / shift_weight_sum
    } else {
        0.0
    };
    let shift_variance = if shift_weight_sum > 0.0 {
        1.0 / shift_weight_sum
    } else {
        0.0
    };

    let base_midpoint_clamped = clamp_probability(base_midpoint.clamp(0.0, 1.0));
    let base_logit = (base_midpoint_clamped / (1.0 - base_midpoint_clamped)).ln();
    let final_logit = base_logit + combined_shift;
    let midpoint = clamp_probability(1.0 / (1.0 + (-final_logit).exp()));

    // Delta method: propagate the shift's own (log-odds-space) variance
    // into probability space at the final midpoint before adding it to
    // the absolute pool's variance.
    let shift_prob_variance = (midpoint * (1.0 - midpoint)).powi(2) * shift_variance;
    let total_variance = combined_variance + shift_prob_variance;
    let half_width = (config.band_z_score * total_variance.sqrt()).max(config.min_band_half_width);

    // The band is built *around the already-clamped midpoint*, not
    // clamped independently of it — clamping `midpoint`, `band_low` and
    // `band_high` each on their own let a malformed midpoint of 1.2 and
    // its band both clamp to 1.0, after which every real price read as
    // "outside the band" (design doc review 2.11). Clamping `band_low`/
    // `band_high` to `[0,1]` here can only ever pull them *toward*
    // `midpoint` (which is already inside `[0,1]`), so `band_low <=
    // midpoint <= band_high` holds by construction.
    let band_low = (midpoint - half_width).clamp(0.0, 1.0);
    let band_high = (midpoint + half_width).clamp(0.0, 1.0);

    Some(FairValue {
        contract: contract.clone(),
        midpoint,
        band_low,
        band_high,
        as_of,
        inputs: usable.into_iter().cloned().collect(),
        effective_sample_size,
    })
}

/// `aggregate`, but a typed error instead of `None` when there was
/// nothing usable to combine.
pub fn try_aggregate(
    contract: &CanonicalContractId,
    estimates: &[ProbabilityEstimate],
    config: &AggregatorConfig,
    as_of: Timestamp,
) -> Result<FairValue, AggregateError> {
    aggregate(contract, estimates, config, as_of).ok_or(AggregateError::NoUsableEstimates)
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
            kind: EstimateKind::Absolute,
            staleness: StalenessPolicy::Decays,
            correlation_group: None,
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

    #[test]
    fn a_permanent_estimate_does_not_decay_with_age() {
        let cfg = AggregatorConfig::default();
        let mut settled = est("oracle", 0.99, 0.01, 0);
        settled.staleness = StalenessPolicy::Permanent;
        // 10 half-lives later, a Decays estimate would be almost entirely
        // down-weighted; Permanent must still dominate.
        let as_of = Timestamp::from_nanos(300_000_000_000);
        let weak_prior = est("weak-prior", 0.5, 0.3, as_of.as_nanos());
        let fv = aggregate(&contract(), &[settled, weak_prior], &cfg, as_of).unwrap();
        assert!(fv.midpoint > 0.9, "midpoint was {}", fv.midpoint);
    }

    #[test]
    fn correlated_sources_do_not_narrow_the_band_as_much_as_independent_ones() {
        let cfg = AggregatorConfig::default();
        let mut correlated = Vec::new();
        for i in 0..5 {
            let mut e = est(&format!("member-{i}"), 0.70, 0.05, 0);
            e.correlation_group = Some("ensemble-a".into());
            correlated.push(e);
        }
        let mut independent = Vec::new();
        for i in 0..5 {
            independent.push(est(&format!("model-{i}"), 0.70, 0.05, 0));
        }
        let correlated_fv =
            aggregate(&contract(), &correlated, &cfg, Timestamp::from_nanos(0)).unwrap();
        let independent_fv =
            aggregate(&contract(), &independent, &cfg, Timestamp::from_nanos(0)).unwrap();
        assert!(
            correlated_fv.band_width() > independent_fv.band_width(),
            "correlated={} independent={}",
            correlated_fv.band_width(),
            independent_fv.band_width()
        );
        assert!(correlated_fv.effective_sample_size < independent_fv.effective_sample_size);
    }

    #[test]
    fn effective_sample_size_of_many_highly_correlated_sources_approaches_one() {
        let cfg = AggregatorConfig {
            assumed_correlation: 0.99,
            ..AggregatorConfig::default()
        };
        let mut estimates = Vec::new();
        for i in 0..500 {
            let mut e = est(&format!("m{i}"), 0.6, 0.05, 0);
            e.correlation_group = Some("g".into());
            estimates.push(e);
        }
        let fv = aggregate(&contract(), &estimates, &cfg, Timestamp::from_nanos(0)).unwrap();
        assert!(
            fv.effective_sample_size < 2.0,
            "500 sources at rho=0.99 should read as ~1 independent sample, got {}",
            fv.effective_sample_size
        );
    }

    #[test]
    fn a_log_odds_shift_moves_a_coin_flip_more_than_a_near_certainty() {
        let cfg = AggregatorConfig::default();
        let coin_flip = vec![
            est("base", 0.50, 0.03, 0),
            ProbabilityEstimate {
                kind: EstimateKind::LogOddsShift,
                probability: 1.0,
                std_dev: 0.3,
                correlation_group: None,
                ..est("headline", 0.0, 0.0, 0)
            },
        ];
        let near_certain = vec![
            est("base", 0.97, 0.01, 0),
            ProbabilityEstimate {
                kind: EstimateKind::LogOddsShift,
                probability: 1.0,
                std_dev: 0.3,
                correlation_group: None,
                ..est("headline", 0.0, 0.0, 0)
            },
        ];
        let fv_flip = aggregate(&contract(), &coin_flip, &cfg, Timestamp::from_nanos(0)).unwrap();
        let fv_certain =
            aggregate(&contract(), &near_certain, &cfg, Timestamp::from_nanos(0)).unwrap();
        let flip_move = fv_flip.midpoint - 0.50;
        let certain_move = fv_certain.midpoint - 0.97;
        assert!(
            flip_move > certain_move,
            "flip_move={flip_move} certain_move={certain_move}"
        );
    }

    #[test]
    fn a_zero_shift_is_a_true_no_op() {
        let cfg = AggregatorConfig::default();
        let estimates = vec![
            est("base", 0.66, 0.03, 0),
            ProbabilityEstimate {
                kind: EstimateKind::LogOddsShift,
                probability: 0.0,
                std_dev: 0.3,
                correlation_group: None,
                ..est("headline", 0.0, 0.0, 0)
            },
        ];
        let without_shift = aggregate(
            &contract(),
            &[est("base", 0.66, 0.03, 0)],
            &cfg,
            Timestamp::from_nanos(0),
        )
        .unwrap();
        let with_shift =
            aggregate(&contract(), &estimates, &cfg, Timestamp::from_nanos(0)).unwrap();
        assert!((with_shift.midpoint - without_shift.midpoint).abs() < 1e-9);
    }

    #[test]
    fn estimates_for_a_different_contract_are_dropped_not_pooled() {
        let cfg = AggregatorConfig::default();
        let other = CanonicalContractId("some.other.contract".into());
        let mismatched = ProbabilityEstimate {
            contract: other,
            ..est("routing-bug", 0.99, 0.01, 0)
        };
        let correct = est("real", 0.40, 0.05, 0);
        let fv = aggregate(
            &contract(),
            &[mismatched, correct],
            &cfg,
            Timestamp::from_nanos(0),
        )
        .unwrap();
        // If the mismatched estimate had been pooled, the midpoint would
        // sit far above 0.40.
        assert!(fv.midpoint < 0.5, "midpoint was {}", fv.midpoint);
    }

    #[test]
    fn try_aggregate_reports_no_usable_estimates_when_nothing_matches() {
        let cfg = AggregatorConfig::default();
        let other = CanonicalContractId("some.other.contract".into());
        let mismatched = ProbabilityEstimate {
            contract: other,
            ..est("routing-bug", 0.99, 0.01, 0)
        };
        assert_eq!(
            try_aggregate(&contract(), &[mismatched], &cfg, Timestamp::from_nanos(0)).unwrap_err(),
            AggregateError::NoUsableEstimates
        );
    }

    #[test]
    fn every_output_satisfies_fair_values_own_validate() {
        let cfg = AggregatorConfig::default();
        let cases = vec![
            vec![est("a", 0.0001, 0.001, 0)],
            vec![est("a", 0.9999, 0.001, 0)],
            vec![est("a", 0.5, 1e-9, 0)],
            vec![est("a", 0.5, 0.5, 0), est("b", 0.5, 0.5, 1_000_000_000_000)],
        ];
        for estimates in cases {
            let fv = aggregate(
                &contract(),
                &estimates,
                &cfg,
                Timestamp::from_nanos(2_000_000_000_000),
            )
            .unwrap();
            assert!(fv.validate().is_ok(), "validate failed for {fv:?}");
        }
    }
}
