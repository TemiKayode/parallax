use crate::validate::{self, ValidationError};
use serde::{Deserialize, Serialize};

/// Every tradable venue PARALLAX knows about. `Paper` is the in-memory
/// simulated venue used by the sim harness and by shadow mode — it is not
/// a stub to be deleted later, it is a first-class adapter target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VenueId {
    Polymarket,
    Kalshi,
    Paper,
}

impl std::fmt::Display for VenueId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            VenueId::Polymarket => "polymarket",
            VenueId::Kalshi => "kalshi",
            VenueId::Paper => "paper",
        };
        write!(f, "{s}")
    }
}

/// How a venue actually clears trades. This changes what "confirmed" means:
/// an off-chain CLOB match is not the same event as on-chain settlement
/// finality, and the risk engine and calibration layer must not conflate
/// the two when they measure latency or exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettlementModel {
    /// Off-chain order matching, batched on-chain settlement (Polymarket's CLOB).
    OffChainMatchOnChainSettle,
    /// Traditional central limit order book with immediate exchange-side finality (Kalshi).
    CentralLimitOrderBook,
    /// In-memory simulated venue, immediate synthetic finality.
    Simulated,
}

/// Both Kalshi and Polymarket price fees on `contracts * price * (1 -
/// price)`, not on notional (`contracts * price`) — fees peak at a
/// 50-cent contract, exactly where most trading happens, so a flat
/// basis-point-of-notional approximation gets the *shape* of the cost
/// wrong as well as its level (design doc review 2.1). A 2-cent crossing
/// at p≈0.5 costs real fees on both venues even though it looks like
/// "riskless" gross edge before this is netted out.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct FeeModel {
    pub maker_rate: f64,
    pub taker_rate: f64,
    /// Fees round up to the nearest multiple of this unit (Kalshi rounds
    /// up to the nearest centicent); `0.0` disables rounding.
    pub round_up_to: f64,
}

impl FeeModel {
    /// `0.07` taker / `0.0175` maker, rounded up to the centicent — the
    /// published schedule as of this writing. Re-verify before trading;
    /// Kalshi has changed this schedule before and a stale fee model
    /// silently turns a profitable strategy unprofitable with no error
    /// anywhere.
    pub fn kalshi_default() -> Self {
        FeeModel {
            maker_rate: 0.0175,
            taker_rate: 0.07,
            round_up_to: 0.0001,
        }
    }

    /// `0.0625` taker, no maker fee, on fee-enabled markets — the
    /// published schedule as of this review.
    pub fn polymarket_default() -> Self {
        FeeModel {
            maker_rate: 0.0,
            taker_rate: 0.0625,
            round_up_to: 0.0,
        }
    }

    /// Fee for `contracts` at `price`, in the same probability-space units
    /// as price (i.e. dollars per $1-notional contract).
    pub fn fee(&self, is_maker: bool, contracts: f64, price: f64) -> f64 {
        let rate = if is_maker {
            self.maker_rate
        } else {
            self.taker_rate
        };
        let raw = rate * contracts * price * (1.0 - price);
        if self.round_up_to > 0.0 {
            (raw / self.round_up_to).ceil() * self.round_up_to
        } else {
            raw
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        validate::non_negative("maker_rate", self.maker_rate)?;
        validate::non_negative("taker_rate", self.taker_rate)?;
        validate::non_negative("round_up_to", self.round_up_to)?;
        Ok(())
    }
}

/// Static, per-venue facts the rest of the system treats as configuration,
/// not something to hardcode inline at each call site. Populated once per
/// adapter via `VenueAdapter::capabilities`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueCapabilities {
    pub venue: VenueId,
    pub settlement: SettlementModel,
    /// Smallest price increment, expressed in probability space (0.0..=1.0).
    pub min_tick: f64,
    pub min_order_size: f64,
    #[serde(skip, default = "FeeModel::kalshi_default")]
    pub fee_model: FeeModel,
    /// Conservative request budget PARALLAX will self-throttle to — always
    /// set below the venue's published limit, never skated against it.
    pub rate_limit_per_sec: u32,
}

impl VenueCapabilities {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate::positive("min_tick", self.min_tick)?;
        validate::positive("min_order_size", self.min_order_size)?;
        self.fee_model.validate()?;
        if self.rate_limit_per_sec == 0 {
            return Err(ValidationError {
                field: "rate_limit_per_sec",
                message: "must be nonzero".into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_peaks_at_a_fifty_cent_contract() {
        let model = FeeModel {
            maker_rate: 0.0,
            taker_rate: 0.07,
            round_up_to: 0.0,
        };
        let at_50c = model.fee(false, 100.0, 0.50);
        let at_10c = model.fee(false, 100.0, 0.10);
        let at_90c = model.fee(false, 100.0, 0.90);
        assert!(at_50c > at_10c);
        assert!(at_50c > at_90c);
    }

    #[test]
    fn fee_rounds_up_to_the_configured_unit() {
        let model = FeeModel {
            maker_rate: 0.0,
            taker_rate: 0.07,
            round_up_to: 0.01,
        };
        let fee = model.fee(false, 1.0, 0.50);
        assert!((fee - 0.02).abs() < 1e-9, "fee was {fee}");
    }

    #[test]
    fn maker_fee_uses_the_maker_rate() {
        let model = FeeModel::kalshi_default();
        let maker = model.fee(true, 100.0, 0.50);
        let taker = model.fee(false, 100.0, 0.50);
        assert!(maker < taker);
    }

    fn caps(min_tick: f64, min_order_size: f64, rate_limit: u32) -> VenueCapabilities {
        VenueCapabilities {
            venue: VenueId::Kalshi,
            settlement: SettlementModel::CentralLimitOrderBook,
            min_tick,
            min_order_size,
            fee_model: FeeModel::kalshi_default(),
            rate_limit_per_sec: rate_limit,
        }
    }

    #[test]
    fn validate_rejects_zero_tick_or_zero_rate_limit() {
        assert!(caps(0.0, 1.0, 10).validate().is_err());
        assert!(caps(0.01, 1.0, 0).validate().is_err());
    }

    #[test]
    fn validate_accepts_reasonable_capabilities() {
        assert!(caps(0.01, 1.0, 10).validate().is_ok());
    }
}
