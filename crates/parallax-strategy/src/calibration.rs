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
    /// Fraction of *submission attempts* the venue rejected outright
    /// (malformed order, closed market, missing symbol mapping) rather
    /// than accepting-and-not-filling. Tracked separately from
    /// `fill_probability` because it answers a different question — "is
    /// something wrong with how we're submitting" versus "is this venue
    /// illiquid" — and conflating them makes both answers wrong (design
    /// doc review 2.15).
    pub reject_rate: f64,
}

impl Default for CalibrationEstimate {
    /// Priors used before any samples exist: a coin-flip fill rate, zero
    /// assumed slippage, and zero assumed reject rate — deliberately
    /// unopinionated rather than optimistic, so a brand-new venue doesn't
    /// get preferential treatment until it earns it.
    fn default() -> Self {
        CalibrationEstimate {
            fill_probability: 0.5,
            expected_slippage: 0.0,
            reject_rate: 0.0,
        }
    }
}

/// The three ways a trading attempt at a venue can resolve. Kept as
/// distinct variants — not a single `filled: bool` — because a rejection
/// and an unfilled IOC are different signals: a run of malformed orders
/// (rejections) looks identical to a venue that has gone illiquid
/// (unfilled) under a boolean, and the fill-probability estimate that
/// decides where a market maker stands would move for the wrong reason
/// (design doc review 2.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillOutcome {
    /// The order (or this slice of it) filled.
    Filled,
    /// The venue accepted the order but it found no crossing liquidity —
    /// a genuine illiquidity signal, and the only case that should move
    /// `fill_probability` toward zero.
    Unfilled,
    /// The venue (or PARALLAX's own validation) refused the request
    /// outright before it could ever fill or not — not an illiquidity
    /// signal, and must not be blended into the same estimate.
    Rejected,
}

struct VenueStats {
    fill_rate_ema: f64,
    slippage_ema: f64,
    reject_rate_ema: f64,
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

    pub fn record_outcome(
        &mut self,
        venue: VenueId,
        outcome: FillOutcome,
        realized_slippage: Option<f64>,
    ) {
        let entry = self.stats.entry(venue).or_insert(VenueStats {
            fill_rate_ema: 0.5,
            slippage_ema: 0.0,
            reject_rate_ema: 0.0,
            samples: 0,
        });
        match outcome {
            FillOutcome::Filled => {
                entry.fill_rate_ema = entry.fill_rate_ema * (1.0 - self.alpha) + self.alpha;
                entry.reject_rate_ema *= 1.0 - self.alpha;
                entry.samples += 1;
            }
            FillOutcome::Unfilled => {
                entry.fill_rate_ema *= 1.0 - self.alpha;
                entry.reject_rate_ema *= 1.0 - self.alpha;
                entry.samples += 1;
            }
            FillOutcome::Rejected => {
                // Deliberately does not touch fill_rate_ema: a rejection
                // says nothing about whether the market has liquidity.
                entry.reject_rate_ema = entry.reject_rate_ema * (1.0 - self.alpha) + self.alpha;
                entry.samples += 1;
            }
        }
        if let Some(slip) = realized_slippage {
            entry.slippage_ema = entry.slippage_ema * (1.0 - self.alpha) + self.alpha * slip;
        }
    }

    pub fn estimate(&self, venue: VenueId) -> CalibrationEstimate {
        match self.stats.get(&venue) {
            Some(s) => CalibrationEstimate {
                fill_probability: s.fill_rate_ema,
                expected_slippage: s.slippage_ema,
                reject_rate: s.reject_rate_ema,
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
            cal.record_outcome(VenueId::Kalshi, FillOutcome::Filled, Some(0.01));
        }
        assert!(cal.estimate(VenueId::Kalshi).fill_probability > 0.9);
    }

    #[test]
    fn repeated_unfilled_orders_push_fill_probability_toward_zero() {
        let mut cal = Calibrator::new(0.3);
        for _ in 0..20 {
            cal.record_outcome(VenueId::Polymarket, FillOutcome::Unfilled, None);
        }
        assert!(cal.estimate(VenueId::Polymarket).fill_probability < 0.1);
    }

    #[test]
    fn rejections_do_not_move_fill_probability_at_all() {
        // A run of malformed orders must not look like an illiquid venue
        // (design doc review 2.15).
        let mut cal = Calibrator::new(0.3);
        for _ in 0..20 {
            cal.record_outcome(VenueId::Kalshi, FillOutcome::Rejected, None);
        }
        let est = cal.estimate(VenueId::Kalshi);
        assert_eq!(
            est.fill_probability, 0.5,
            "rejections must not touch fill_probability"
        );
        assert!(est.reject_rate > 0.9);
    }

    #[test]
    fn unseen_venue_uses_unopinionated_prior() {
        let cal = Calibrator::new(0.3);
        let est = cal.estimate(VenueId::Paper);
        assert_eq!(est.fill_probability, 0.5);
        assert_eq!(est.expected_slippage, 0.0);
        assert_eq!(est.reject_rate, 0.0);
    }
}
