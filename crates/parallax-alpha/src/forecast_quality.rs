//! Measures whether the forecasts are actually any good, per source —
//! nothing in the original implementation did (design doc review 3.23).
//! An overall Brier score of 0.15 is compatible with a 95%-confidence
//! bucket that resolves YES 60% of the time, which is exactly the bucket
//! a stat-arb engine sizes into hardest; a single aggregate number hides
//! that, so this reports a full reliability curve per source, not just
//! one score.

use crate::stats::clamp_probability;
use std::collections::HashMap;

const BUCKETS: usize = 10;

#[derive(Debug, Clone, Copy, Default)]
struct SourceStats {
    brier_sum: f64,
    log_loss_sum: f64,
    n: u64,
    bucket_sum_predicted: [f64; BUCKETS],
    bucket_sum_actual: [f64; BUCKETS],
    bucket_count: [u64; BUCKETS],
}

/// One bucket of a reliability curve: among forecasts whose predicted
/// probability fell in this decile, what fraction actually resolved YES?
/// A well-calibrated source has `actual_frequency ≈ predicted_mean` in
/// every bucket with enough `count` to be meaningful.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReliabilityPoint {
    pub predicted_mean: f64,
    pub actual_frequency: f64,
    pub count: u64,
}

/// Accumulates resolved-outcome statistics per alpha source. Feed it every
/// `(predicted_probability, outcome)` pair once a contract resolves;
/// query `skill_score` before enabling any engine on live data, and check
/// `reliability_curve` per source, not just the aggregate — the whole
/// point of tracking this at all.
#[derive(Default)]
pub struct ForecastQualityTracker {
    per_source: HashMap<String, SourceStats>,
    base_rate_sum: f64,
    base_rate_n: u64,
}

impl ForecastQualityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, source: &str, predicted_probability: f64, outcome: bool) {
        let p = clamp_probability(predicted_probability);
        let y = if outcome { 1.0 } else { 0.0 };

        let stats = self.per_source.entry(source.to_string()).or_default();
        stats.brier_sum += (p - y).powi(2);
        stats.log_loss_sum -= y * p.ln() + (1.0 - y) * (1.0 - p).ln();
        stats.n += 1;
        let bucket = ((p * BUCKETS as f64) as usize).min(BUCKETS - 1);
        stats.bucket_sum_predicted[bucket] += p;
        stats.bucket_sum_actual[bucket] += y;
        stats.bucket_count[bucket] += 1;

        self.base_rate_sum += y;
        self.base_rate_n += 1;
    }

    fn base_rate(&self) -> f64 {
        if self.base_rate_n == 0 {
            0.5
        } else {
            self.base_rate_sum / self.base_rate_n as f64
        }
    }

    pub fn brier_score(&self, source: &str) -> Option<f64> {
        let s = self.per_source.get(source)?;
        (s.n > 0).then(|| s.brier_sum / s.n as f64)
    }

    pub fn log_loss(&self, source: &str) -> Option<f64> {
        let s = self.per_source.get(source)?;
        (s.n > 0).then(|| s.log_loss_sum / s.n as f64)
    }

    /// `1 − brier / baseline`, where `baseline` is the Brier score of
    /// always predicting the overall observed base rate. Positive means
    /// the source beats "just guess the base rate"; `None` when there
    /// isn't enough data yet, or the base rate is degenerate (every
    /// resolution has gone the same way so far).
    pub fn skill_score(&self, source: &str) -> Option<f64> {
        let brier = self.brier_score(source)?;
        let base = self.base_rate();
        let baseline_brier = base * (1.0 - base);
        if baseline_brier <= 1e-9 {
            return None;
        }
        Some(1.0 - brier / baseline_brier)
    }

    /// Per-decile-of-predicted-probability calibration, omitting empty
    /// buckets. `Vec::new()` for an unseen source.
    pub fn reliability_curve(&self, source: &str) -> Vec<ReliabilityPoint> {
        let Some(s) = self.per_source.get(source) else {
            return Vec::new();
        };
        (0..BUCKETS)
            .filter(|&i| s.bucket_count[i] > 0)
            .map(|i| ReliabilityPoint {
                predicted_mean: s.bucket_sum_predicted[i] / s.bucket_count[i] as f64,
                actual_frequency: s.bucket_sum_actual[i] / s.bucket_count[i] as f64,
                count: s.bucket_count[i],
            })
            .collect()
    }

    pub fn sample_count(&self, source: &str) -> u64 {
        self.per_source.get(source).map(|s| s.n).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_forecasts_score_zero_brier() {
        let mut tracker = ForecastQualityTracker::new();
        for _ in 0..20 {
            tracker.record("perfect", 1.0, true);
            tracker.record("perfect", 0.0, false);
        }
        let brier = tracker.brier_score("perfect").unwrap();
        assert!(brier < 1e-6, "brier was {brier}");
    }

    #[test]
    fn always_fifty_fifty_scores_a_quarter_brier_against_balanced_outcomes() {
        let mut tracker = ForecastQualityTracker::new();
        for i in 0..100 {
            tracker.record("coinflip", 0.5, i % 2 == 0);
        }
        let brier = tracker.brier_score("coinflip").unwrap();
        assert!((brier - 0.25).abs() < 1e-6, "brier was {brier}");
    }

    #[test]
    fn a_confidently_wrong_source_has_negative_skill() {
        let mut tracker = ForecastQualityTracker::new();
        for _ in 0..30 {
            tracker.record("overconfident", 0.99, false);
            tracker.record("overconfident", 0.01, true);
        }
        let skill = tracker.skill_score("overconfident").unwrap();
        assert!(skill < 0.0, "skill was {skill}");
    }

    #[test]
    fn a_source_beating_the_base_rate_has_positive_skill() {
        let mut tracker = ForecastQualityTracker::new();
        // Base rate ends up ~50%, but this source is well-calibrated and
        // confidently correct most of the time.
        for i in 0..100 {
            let outcome = i % 2 == 0;
            let p = if outcome { 0.9 } else { 0.1 };
            tracker.record("good", p, outcome);
        }
        let skill = tracker.skill_score("good").unwrap();
        assert!(skill > 0.5, "skill was {skill}");
    }

    #[test]
    fn reliability_curve_reports_calibrated_buckets_close_to_the_diagonal() {
        let mut tracker = ForecastQualityTracker::new();
        // Every forecast at ~0.7 resolves YES 70% of the time.
        for i in 0..100 {
            tracker.record("calibrated", 0.7, i % 10 < 7);
        }
        let curve = tracker.reliability_curve("calibrated");
        assert_eq!(curve.len(), 1);
        assert!((curve[0].predicted_mean - 0.7).abs() < 1e-9);
        assert!((curve[0].actual_frequency - 0.7).abs() < 1e-9);
        assert_eq!(curve[0].count, 100);
    }

    #[test]
    fn an_unseen_source_has_no_stats() {
        let tracker = ForecastQualityTracker::new();
        assert!(tracker.brier_score("nobody").is_none());
        assert!(tracker.skill_score("nobody").is_none());
        assert!(tracker.reliability_curve("nobody").is_empty());
    }

    #[test]
    fn sources_are_tracked_independently() {
        let mut tracker = ForecastQualityTracker::new();
        for _ in 0..10 {
            tracker.record("a", 1.0, true);
            tracker.record("b", 0.0, true); // confidently wrong
        }
        assert!(tracker.brier_score("a").unwrap() < tracker.brier_score("b").unwrap());
    }
}
