use crate::contract::CanonicalContractId;
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
}

impl Position {
    pub fn flat(venue: VenueId, contract: CanonicalContractId) -> Self {
        Position {
            venue,
            contract,
            qty: 0.0,
            avg_price: 0.0,
        }
    }

    pub fn is_flat(&self) -> bool {
        self.qty.abs() < 1e-9
    }

    /// Mark-to-model unrealized P&L against a given fair-value midpoint.
    pub fn unrealized_pnl(&self, fair_mid: f64) -> f64 {
        self.qty * (fair_mid - self.avg_price)
    }

    pub fn apply_fill(&mut self, fill_qty_signed: f64, fill_price: f64) {
        let new_qty = self.qty + fill_qty_signed;
        if new_qty.abs() < 1e-9 {
            self.qty = 0.0;
            self.avg_price = 0.0;
            return;
        }
        // Only move the average price when the fill adds to (or flips) the
        // position in the same direction; a fill that reduces exposure
        // realizes P&L against the existing average instead.
        let same_direction = self.qty == 0.0 || (self.qty.signum() == fill_qty_signed.signum());
        if same_direction {
            self.avg_price = (self.avg_price * self.qty + fill_price * fill_qty_signed) / new_qty;
        }
        self.qty = new_qty;
    }
}
