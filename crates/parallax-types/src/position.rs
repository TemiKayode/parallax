use crate::contract::CanonicalContractId;
use crate::validate::{self, ValidationError};
use crate::venue::VenueId;
use serde::{Deserialize, Serialize};

/// PARALLAX's own resting exposure in one contract on one venue. `qty` is
/// signed: positive is net long YES, negative is net long NO (i.e. short
/// YES). Prices/costs are in probability space, consistent with
/// `NormalizedTick` and `OrderIntent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub venue: VenueId,
    pub contract: CanonicalContractId,
    pub qty: f64,
    pub avg_price: f64,
    /// Realized P&L accumulated across every fill that reduced or flipped
    /// this position, in the same probability-space units as price. Reset
    /// only by constructing a fresh `Position` — it is a running total,
    /// not a mark.
    pub realized_pnl: f64,
    /// Fees paid on every fill against this position, accumulated the same
    /// way. Tracked separately from `realized_pnl` so a caller can report
    /// gross P&L, fees, and net P&L independently.
    pub fees_paid: f64,
}

/// Fills smaller than this are dust: floating-point residue from a chain
/// of adds/reduces that should sum to exactly flat but lands a few ULPs
/// off zero. Snapping to flat here — rather than dividing by a
/// denominator that can be arbitrarily close to zero — is what keeps
/// `avg_price` from ever becoming `NaN` and silently disabling every
/// downstream comparison that reads it (design doc review 1.1/4.2).
const DUST_EPSILON: f64 = 1e-9;

impl Position {
    pub fn flat(venue: VenueId, contract: CanonicalContractId) -> Self {
        Position {
            venue,
            contract,
            qty: 0.0,
            avg_price: 0.0,
            realized_pnl: 0.0,
            fees_paid: 0.0,
        }
    }

    pub fn is_flat(&self) -> bool {
        self.qty.abs() < DUST_EPSILON
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        validate::finite("qty", self.qty)?;
        validate::finite("avg_price", self.avg_price)?;
        if !self.is_flat() {
            validate::probability("avg_price", self.avg_price)?;
        }
        validate::finite("realized_pnl", self.realized_pnl)?;
        validate::non_negative("fees_paid", self.fees_paid)?;
        Ok(())
    }

    /// Mark-to-model unrealized P&L against a given fair-value midpoint.
    pub fn unrealized_pnl(&self, fair_mid: f64) -> f64 {
        self.qty * (fair_mid - self.avg_price)
    }

    /// Applies one signed fill at `fill_price`, charging `fee` against
    /// `fees_paid`, and returns the P&L this specific fill realized (0.0
    /// if it purely added to or opened a position). Three cases, handled
    /// separately rather than as one formula that happens to work for one
    /// of them (design doc review 2.4):
    ///
    /// - **Add** (flat, or same direction as the existing position):
    ///   blend the average price; nothing is realized yet.
    /// - **Reduce** (opposite direction, doesn't cross through flat):
    ///   realize P&L on the closed quantity against the *existing* average
    ///   price, which stays unchanged on the remainder.
    /// - **Flip** (opposite direction, crosses through flat): realize P&L
    ///   on the entire old position, then open the new one at this fill's
    ///   own price — carrying the stale average forward here is exactly
    ///   the 10-cent-per-share bug this case exists to avoid.
    pub fn apply_fill(&mut self, fill_qty_signed: f64, fill_price: f64, fee: f64) -> f64 {
        let adding = self.qty == 0.0 || self.qty.signum() == fill_qty_signed.signum();
        let new_qty = self.qty + fill_qty_signed;
        let mut realized = 0.0;

        if adding {
            if new_qty.abs() < DUST_EPSILON {
                self.qty = 0.0;
                self.avg_price = 0.0;
            } else {
                self.avg_price =
                    (self.avg_price * self.qty + fill_price * fill_qty_signed) / new_qty;
                self.qty = new_qty;
            }
        } else {
            let closing_qty = fill_qty_signed.abs().min(self.qty.abs());
            realized = closing_qty * (fill_price - self.avg_price) * self.qty.signum();

            if new_qty.abs() < DUST_EPSILON {
                self.qty = 0.0;
                self.avg_price = 0.0;
            } else if new_qty.signum() == self.qty.signum() {
                // Pure reduce: existing average is still the right basis
                // for whatever remains.
                self.qty = new_qty;
            } else {
                // Flip: the old position closed entirely: the remainder
                // is a brand-new position opened at this fill's price.
                self.qty = new_qty;
                self.avg_price = fill_price;
            }
        }

        self.realized_pnl += realized;
        self.fees_paid += fee;
        realized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{CanonicalContractSpec, Direction, EventClass};

    fn contract() -> CanonicalContractId {
        CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: 869,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        }
        .to_id()
    }

    fn pos() -> Position {
        Position::flat(VenueId::Kalshi, contract())
    }

    #[test]
    fn opening_a_position_sets_avg_price_and_realizes_nothing() {
        let mut p = pos();
        let realized = p.apply_fill(10.0, 0.50, 0.0);
        assert_eq!(realized, 0.0);
        assert_eq!(p.qty, 10.0);
        assert_eq!(p.avg_price, 0.50);
    }

    #[test]
    fn adding_in_the_same_direction_blends_the_average_price() {
        let mut p = pos();
        p.apply_fill(10.0, 0.50, 0.0);
        p.apply_fill(5.0, 0.70, 0.0);
        assert_eq!(p.qty, 15.0);
        assert!((p.avg_price - (0.50 * 10.0 + 0.70 * 5.0) / 15.0).abs() < 1e-9);
    }

    #[test]
    fn reducing_realizes_pnl_against_the_existing_average_and_leaves_it_unchanged() {
        let mut p = pos();
        p.apply_fill(10.0, 0.50, 0.0);
        let realized = p.apply_fill(-4.0, 0.60, 0.0);
        assert!((realized - 0.4).abs() < 1e-9, "realized was {realized}");
        assert_eq!(p.qty, 6.0);
        assert_eq!(p.avg_price, 0.50, "average must not move on a pure reduce");
    }

    #[test]
    fn flipping_through_flat_resets_avg_price_to_the_flipping_fills_price() {
        // Regression for review 2.4: a long of 10 @ 0.50 closed by a sell
        // of 15 @ 0.60 must leave the resulting short's avg_price at 0.60
        // (the flip's own price), not the stale 0.50.
        let mut p = pos();
        p.apply_fill(10.0, 0.50, 0.0);
        let realized = p.apply_fill(-15.0, 0.60, 0.0);
        assert!((realized - 1.0).abs() < 1e-9, "realized was {realized}");
        assert_eq!(p.qty, -5.0);
        assert_eq!(p.avg_price, 0.60);
    }

    #[test]
    fn short_covered_at_a_lower_price_realizes_a_profit() {
        let mut p = pos();
        p.apply_fill(-10.0, 0.50, 0.0); // open short
        let realized = p.apply_fill(10.0, 0.40, 0.0); // cover, price dropped
        assert!((realized - 1.0).abs() < 1e-9, "realized was {realized}");
        assert!(p.is_flat());
    }

    #[test]
    fn exact_offsetting_fill_snaps_cleanly_to_flat_without_nan() {
        let mut p = pos();
        p.apply_fill(10.0, 0.50, 0.0);
        p.apply_fill(-10.0, 0.55, 0.0);
        assert!(p.is_flat());
        assert_eq!(p.avg_price, 0.0);
        assert!(!p.avg_price.is_nan());
    }

    #[test]
    fn dust_sized_residual_position_does_not_produce_nan_on_the_next_fill() {
        // A chain of adds/reduces can land a few ULPs off exactly flat;
        // the next fill must not divide by that near-zero denominator.
        let mut p = pos();
        p.qty = 1e-11;
        p.avg_price = 0.50;
        p.apply_fill(-1e-11, 0.50, 0.0);
        assert!(p.qty.is_finite());
        assert!(p.avg_price.is_finite());
        assert!(!p.avg_price.is_nan());
    }

    #[test]
    fn fees_accumulate_independently_of_realized_pnl() {
        let mut p = pos();
        p.apply_fill(10.0, 0.50, 0.05);
        p.apply_fill(-10.0, 0.60, 0.03);
        assert!((p.fees_paid - 0.08).abs() < 1e-9);
        assert!((p.realized_pnl - 1.0).abs() < 1e-9);
    }

    #[test]
    fn validate_rejects_non_finite_fields() {
        let mut p = pos();
        p.qty = f64::NAN;
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_rejects_avg_price_outside_probability_range_when_not_flat() {
        let mut p = pos();
        p.qty = 10.0;
        p.avg_price = 1.5;
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_allows_a_stale_avg_price_once_flat() {
        let mut p = pos();
        p.qty = 0.0;
        p.avg_price = 0.0;
        assert!(p.validate().is_ok());
    }
}
