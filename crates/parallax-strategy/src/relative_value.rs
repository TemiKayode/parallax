use parallax_types::CanonicalContractId;
use std::collections::HashMap;

/// Welford's online algorithm for streaming mean/variance — numerically
/// stable and doesn't require keeping the whole gap history around, which
/// matters here since a pair's gap gets a new observation on every tick
/// of either market.
#[derive(Debug, Default, Clone, Copy)]
struct WelfordState {
    count: u64,
    mean: f64,
    m2: f64,
}

impl WelfordState {
    fn update(&mut self, x: f64) {
        self.count += 1;
        let delta = x - self.mean;
        self.mean += delta / self.count as f64;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;
    }

    fn std_dev(&self) -> Option<f64> {
        if self.count < 2 {
            return None;
        }
        Some((self.m2 / (self.count as f64 - 1.0)).sqrt())
    }
}

/// The multi-market relative-value monitor (design doc §6): tracks each
/// related-market pair's *own* rolling gap distribution and scores new
/// observations against it, rather than a single fixed threshold shared
/// across pairs with structurally different typical gaps (a 5m-vs-15m
/// gap behaves nothing like a BTC-vs-ETH gap).
#[derive(Default)]
pub struct RelativeValueMonitor {
    pairs: HashMap<(CanonicalContractId, CanonicalContractId), WelfordState>,
    /// Floor on the historical volatility denominator — a pair that has
    /// only ever shown a near-constant gap shouldn't produce an
    /// exploding z-score the first time it moves at all.
    min_std_dev: f64,
}

impl RelativeValueMonitor {
    pub fn new(min_std_dev: f64) -> Self {
        RelativeValueMonitor {
            pairs: HashMap::new(),
            min_std_dev,
        }
    }

    /// Canonicalizes the pair's storage key by contract id ordering and
    /// flips the gap's sign to match, so callers never have to worry
    /// about passing `(a, b)` vs `(b, a)` consistently — the monitor
    /// keeps its own sign convention stable regardless.
    fn canonicalize(
        a: &CanonicalContractId,
        b: &CanonicalContractId,
        gap: f64,
    ) -> ((CanonicalContractId, CanonicalContractId), f64) {
        if a <= b {
            ((a.clone(), b.clone()), gap)
        } else {
            ((b.clone(), a.clone()), -gap)
        }
    }

    /// `RelativeScore = (CurrentGap − TypicalGap) / HistoricalGapVolatility`
    /// (design doc §6), scored against the pair's history *before*
    /// incorporating this observation — otherwise every point would be
    /// partly scored against itself. Returns `None` until the pair has at
    /// least two prior observations to compute a volatility from.
    pub fn update_and_score(
        &mut self,
        a: &CanonicalContractId,
        b: &CanonicalContractId,
        current_gap: f64,
    ) -> Option<f64> {
        let (key, gap) = Self::canonicalize(a, b, current_gap);
        let state = self.pairs.entry(key).or_default();
        let score = state
            .std_dev()
            .map(|std| (gap - state.mean) / std.max(self.min_std_dev));
        state.update(gap);
        score
    }

    pub fn typical_gap(&self, a: &CanonicalContractId, b: &CanonicalContractId) -> Option<f64> {
        let (key, sign_probe) = Self::canonicalize(a, b, 1.0);
        self.pairs.get(&key).map(|s| s.mean * sign_probe)
    }

    pub fn gap_volatility(&self, a: &CanonicalContractId, b: &CanonicalContractId) -> Option<f64> {
        let (key, _) = Self::canonicalize(a, b, 1.0);
        self.pairs.get(&key).and_then(|s| s.std_dev())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::{CanonicalContractSpec, Direction, EventClass};

    fn market(window: &str) -> CanonicalContractId {
        CanonicalContractSpec {
            event_class: EventClass("crypto.updown".into()),
            location: "btc".into(),
            threshold: 0,
            direction: Direction::GreaterThan,
            resolution_window: window.into(),
            resolution_source: "binance".into(),
        }
        .to_id()
    }

    #[test]
    fn returns_none_until_two_prior_observations_exist() {
        // Sample std_dev (n-1 denominator) is undefined with fewer than
        // two prior points, so the first *two* calls score None — the
        // third has two priors to compute a volatility from.
        let mut monitor = RelativeValueMonitor::new(0.001);
        let (a, b) = (market("5m"), market("15m"));
        assert!(monitor.update_and_score(&a, &b, 0.02).is_none());
        assert!(monitor.update_and_score(&a, &b, 0.021).is_none());
        assert!(monitor.update_and_score(&a, &b, 0.02).is_some());
    }

    #[test]
    fn a_large_deviation_from_the_typical_gap_scores_high() {
        let mut monitor = RelativeValueMonitor::new(0.001);
        let (a, b) = (market("5m"), market("15m"));
        // Establish a stable typical gap around 0.02.
        for g in [0.019, 0.021, 0.020, 0.0195, 0.0205, 0.020] {
            monitor.update_and_score(&a, &b, g);
        }
        // A sudden jump to a 0.08 gap should score far outside normal range.
        let score = monitor.update_and_score(&a, &b, 0.08).unwrap();
        assert!(score.abs() > 3.0, "score was {score}");
    }

    #[test]
    fn ordering_of_arguments_does_not_change_the_pairs_identity() {
        // Two independent monitors with identical history — calling
        // update_and_score itself mutates state, so comparing "forward
        // vs reversed order" on *one* monitor would have the second call
        // see state the first call had just changed. Two monitors with
        // the same setup isolate the thing actually under test: does
        // reversing (a, b) to (b, a) with a negated gap land on an
        // equivalent score.
        let mut forward = RelativeValueMonitor::new(0.001);
        let mut reversed = RelativeValueMonitor::new(0.001);
        let (a, b) = (market("5m"), market("15m"));
        for g in [0.02, 0.021] {
            forward.update_and_score(&a, &b, g);
            reversed.update_and_score(&b, &a, -g);
        }
        let score_forward = forward.update_and_score(&a, &b, 0.05).unwrap();
        let score_reversed = reversed.update_and_score(&b, &a, -0.05).unwrap();
        assert!(
            (score_forward - score_reversed).abs() < 1e-9,
            "forward={score_forward} reversed={score_reversed}"
        );
    }

    #[test]
    fn typical_gap_and_volatility_match_a_naive_sample_calculation() {
        let mut monitor = RelativeValueMonitor::new(0.0);
        let (a, b) = (market("5m"), market("15m"));
        let gaps = [0.01, 0.03, 0.02, 0.025, 0.015];
        for g in gaps {
            monitor.update_and_score(&a, &b, g);
        }
        let naive_mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        let naive_var =
            gaps.iter().map(|g| (g - naive_mean).powi(2)).sum::<f64>() / (gaps.len() as f64 - 1.0);
        assert!((monitor.typical_gap(&a, &b).unwrap() - naive_mean).abs() < 1e-9);
        assert!((monitor.gap_volatility(&a, &b).unwrap() - naive_var.sqrt()).abs() < 1e-9);
    }

    #[test]
    fn different_pairs_are_tracked_independently() {
        let mut monitor = RelativeValueMonitor::new(0.001);
        let (a, b, c) = (market("5m"), market("15m"), market("30m"));
        for g in [0.01, 0.011, 0.0105] {
            monitor.update_and_score(&a, &b, g);
        }
        for g in [0.05, 0.052, 0.051] {
            monitor.update_and_score(&a, &c, g);
        }
        assert!((monitor.typical_gap(&a, &b).unwrap() - 0.0105).abs() < 1e-6);
        assert!((monitor.typical_gap(&a, &c).unwrap() - 0.051).abs() < 1e-6);
    }
}
