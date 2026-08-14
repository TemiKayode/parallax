use crate::contract::CanonicalContractId;
use crate::time::Timestamp;
use crate::validate::{self, ValidationError};
use crate::venue::VenueId;
use serde::{Deserialize, Serialize};

/// One venue's best-of-book snapshot for one canonical contract, already
/// mapped from that venue's native listing by its adapter. Prices are
/// probabilities in `[0.0, 1.0]` — every venue's native price convention
/// (cents, ticks, percent) is converted at the adapter boundary, never
/// downstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedTick {
    pub venue: VenueId,
    pub contract: CanonicalContractId,
    pub bid: f64,
    pub bid_size: f64,
    pub ask: f64,
    pub ask_size: f64,
    /// Timestamp the venue attached to this update, if it published one.
    pub venue_ts: Option<Timestamp>,
    /// Timestamp PARALLAX's adapter observed the message. Staleness checks
    /// in the risk engine use this, not `venue_ts`, since clock skew
    /// against a venue we don't control is itself a source of risk.
    pub receive_ts: Timestamp,
}

impl NormalizedTick {
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }

    pub fn spread(&self) -> f64 {
        (self.ask - self.bid).max(0.0)
    }

    /// A NaN `bid`/`ask` compares `false` against every `>`/`<` check a
    /// consumer runs, which reads as "inside every band" and "passes every
    /// staleness/price check" — so a malformed venue message must be
    /// rejected here, at the parse boundary, rather than laundered into
    /// the book as data (design doc review 1.1). Sizes are allowed to be
    /// exactly zero (an empty level) but not negative or non-finite.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate::probability("bid", self.bid)?;
        validate::probability("ask", self.ask)?;
        validate::non_negative("bid_size", self.bid_size)?;
        validate::non_negative("ask_size", self.ask_size)?;
        Ok(())
    }
}

/// One resting-order level in a depth snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DepthLevel {
    pub price: f64,
    pub size: f64,
}

/// A multi-level order book snapshot for one venue/contract — what the
/// APERTURE edge calculator needs and `NormalizedTick` deliberately
/// doesn't carry (design doc §5/§13: PARALLAX's `ConsolidatedBook` is
/// top-of-book only by design; a real execution-cost estimate for a
/// given size has to see what's actually resting behind the touch).
/// `bids` must be sorted descending by price, `asks` ascending — callers
/// populate them in that order; nothing here re-sorts for you.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookDepth {
    pub venue: VenueId,
    pub contract: CanonicalContractId,
    pub bids: Vec<DepthLevel>,
    pub asks: Vec<DepthLevel>,
    pub receive_ts: Timestamp,
}

/// The result of walking one side of a depth snapshot for a target size:
/// the volume-weighted average price actually achievable, and how much
/// of the target size the book could actually fill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WalkResult {
    pub avg_price: f64,
    pub filled_size: f64,
}

impl BookDepth {
    /// Walks `levels` (already sorted best-first by the caller) up to
    /// `target_size`, returning the volume-weighted average price for
    /// whatever it could fill. Returns `None` for an empty book or a
    /// non-positive target — there is no such thing as "the average
    /// price of zero shares."
    pub fn walk(levels: &[DepthLevel], target_size: f64) -> Option<WalkResult> {
        if target_size <= 0.0 || levels.is_empty() {
            return None;
        }
        let mut remaining = target_size;
        let mut notional = 0.0;
        let mut filled = 0.0;
        for level in levels {
            if remaining <= 0.0 {
                break;
            }
            let take = level.size.min(remaining);
            notional += take * level.price;
            filled += take;
            remaining -= take;
        }
        if filled <= 0.0 {
            return None;
        }
        Some(WalkResult {
            avg_price: notional / filled,
            filled_size: filled,
        })
    }

    pub fn walk_asks(&self, target_size: f64) -> Option<WalkResult> {
        Self::walk(&self.asks, target_size)
    }

    pub fn walk_bids(&self, target_size: f64) -> Option<WalkResult> {
        Self::walk(&self.bids, target_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(pairs: &[(f64, f64)]) -> Vec<DepthLevel> {
        pairs
            .iter()
            .map(|&(price, size)| DepthLevel { price, size })
            .collect()
    }

    #[test]
    fn walk_averages_across_multiple_levels_when_size_exceeds_top_level() {
        // 40 @ 0.60, 60 @ 0.62 -> walking 70 takes all 40 at .60 and 30 of the 60 at .62
        let asks = levels(&[(0.60, 40.0), (0.62, 60.0)]);
        let result = BookDepth::walk(&asks, 70.0).unwrap();
        assert_eq!(result.filled_size, 70.0);
        let expected_notional = 40.0 * 0.60 + 30.0 * 0.62;
        assert!((result.avg_price - expected_notional / 70.0).abs() < 1e-9);
    }

    #[test]
    fn walk_reports_partial_fill_when_book_is_thinner_than_target() {
        let asks = levels(&[(0.60, 10.0), (0.62, 5.0)]);
        let result = BookDepth::walk(&asks, 100.0).unwrap();
        assert_eq!(result.filled_size, 15.0);
        assert!((result.avg_price - (10.0 * 0.60 + 5.0 * 0.62) / 15.0).abs() < 1e-9);
    }

    #[test]
    fn walk_matches_top_of_book_price_when_target_fits_in_first_level() {
        let asks = levels(&[(0.55, 100.0), (0.60, 100.0)]);
        let result = BookDepth::walk(&asks, 20.0).unwrap();
        assert_eq!(result.filled_size, 20.0);
        assert!((result.avg_price - 0.55).abs() < 1e-9);
    }

    #[test]
    fn walk_returns_none_for_empty_book_or_nonpositive_size() {
        assert!(BookDepth::walk(&[], 10.0).is_none());
        assert!(BookDepth::walk(&levels(&[(0.5, 10.0)]), 0.0).is_none());
        assert!(BookDepth::walk(&levels(&[(0.5, 10.0)]), -5.0).is_none());
    }

    fn tick(bid: f64, ask: f64) -> NormalizedTick {
        NormalizedTick {
            venue: VenueId::Kalshi,
            contract: CanonicalContractId("wx.temp.chicago.gt_869.2026-08-12.nws_official".into()),
            bid,
            bid_size: 10.0,
            ask,
            ask_size: 10.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        }
    }

    #[test]
    fn validate_rejects_a_nan_price_that_would_otherwise_clear_every_comparison() {
        assert!(tick(f64::NAN, 0.60).validate().is_err());
        assert!(tick(0.55, f64::NAN).validate().is_err());
    }

    #[test]
    fn validate_rejects_out_of_range_prices_and_negative_sizes() {
        assert!(tick(-0.1, 0.60).validate().is_err());
        assert!(tick(0.55, 1.1).validate().is_err());
        let mut t = tick(0.55, 0.60);
        t.bid_size = -1.0;
        assert!(t.validate().is_err());
    }

    #[test]
    fn validate_accepts_a_well_formed_tick() {
        assert!(tick(0.55, 0.60).validate().is_ok());
    }
}
