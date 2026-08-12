use parallax_types::VenueId;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub struct CalibrationEstimate {
    /// Rolling estimate of P(a resting quote at this venue gets filled
    /// before it needs to be replaced), in `[0, 1]`.
    pub fill_probability: f64,
    /// Rolling estimate of realized slippage (price paid minus price
    /// quoted, in probability-space units) for aggressive fills at this
    /// venue.
    pub expected_slippage: f64,
}

impl Default for CalibrationEstimate {
    /// Priors used before any samples exist: a coin-flip fill rate and
    /// zero assumed slippage — deliberately unopinionated rather than
    /// optimistic, so a brand-new venue doesn't get preferential
    /// treatment until it earns it.
    fn default() -> Self {
        CalibrationEstimate {
            fill_probability: 0.5,
            expected_slippage: 0.0,
        }
    }
}

struct VenueStats {
    fill_rate_ema: f64,
    slippage_ema: f64,
    samples: u64,
}

/// The "lightweight online model" from design doc §11: an exponential
/// moving average per venue, updated on every fill/reject, entirely off
/// the hot path. Deliberately not a retraining loop — heavier model
/// fitting is offline Python work (design doc §12) that ships as a
/// config artifact; this stays cheap enough to update inline in response
/// to every order outcome.
pub struct Calibrator {
    stats: HashMap<VenueId, VenueStats>,
    /// EMA decay — higher weights recent outcomes more heavily.
    alpha: f64,
}

impl Calibrator {
    pub fn new(alpha: f64) -> Self {
        Calibrator {
            stats: HashMap::new(),
            alpha: alpha.clamp(0.0, 1.0),
        }
    }

    pub fn record_outcome(&mut self, venue: VenueId, filled: bool, realized_slippage: Option<f64>) {
        let entry = self.stats.entry(venue).or_insert(VenueStats {
            fill_rate_ema: 0.5,
            slippage_ema: 0.0,
            samples: 0,
        });
        entry.fill_rate_ema =
            entry.fill_rate_ema * (1.0 - self.alpha) + self.alpha * (filled as u8 as f64);
        if let Some(slip) = realized_slippage {
            entry.slippage_ema = entry.slippage_ema * (1.0 - self.alpha) + self.alpha * slip;
        }
        entry.samples += 1;
    }

    pub fn estimate(&self, venue: VenueId) -> CalibrationEstimate {
        match self.stats.get(&venue) {
            Some(s) => CalibrationEstimate {
                fill_probability: s.fill_rate_ema,
                expected_slippage: s.slippage_ema,
            },
            None => CalibrationEstimate::default(),
        }
    }
}

impl Default for Calibrator {
    fn default() -> Self {
        Calibrator::new(0.2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_fills_push_fill_probability_toward_one() {
        let mut cal = Calibrator::new(0.3);
        for _ in 0..20 {
            cal.record_outcome(VenueId::Kalshi, true, Some(0.01));
        }
        assert!(cal.estimate(VenueId::Kalshi).fill_probability > 0.9);
    }

    #[test]
    fn repeated_rejects_push_fill_probability_toward_zero() {
        let mut cal = Calibrator::new(0.3);
        for _ in 0..20 {
            cal.record_outcome(VenueId::Polymarket, false, None);
        }
        assert!(cal.estimate(VenueId::Polymarket).fill_probability < 0.1);
    }

    #[test]
    fn unseen_venue_uses_unopinionated_prior() {
        let cal = Calibrator::new(0.3);
        let est = cal.estimate(VenueId::Paper);
        assert_eq!(est.fill_probability, 0.5);
        assert_eq!(est.expected_slippage, 0.0);
    }
}
