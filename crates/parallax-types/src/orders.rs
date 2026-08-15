use crate::contract::CanonicalContractId;
use crate::time::Timestamp;
use crate::validate::{self, ValidationError};
use crate::venue::VenueId;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Outcome {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    Limit,
    /// Marketable/aggressive limit used for sniping a specific stale quote —
    /// PARALLAX never sends a true unbounded market order against a public
    /// book it does not fully control.
    ImmediateOrCancel,
}

/// Which strategy engine originated an order, carried through for risk
/// arbitration, telemetry, and the calibration layer's per-engine
/// slippage tracking (design doc §8/§11). Not exposed to venues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineId {
    MarketMaking,
    StatArb,
    LiquiditySniping,
}

impl fmt::Display for EngineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            EngineId::MarketMaking => "market_making",
            EngineId::StatArb => "stat_arb",
            EngineId::LiquiditySniping => "liquidity_sniping",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OrderId(pub String);

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A strategy engine's proposed order, before the risk gate has seen it.
/// This is the type referenced in design doc §8's architectural
/// enforcement note: it carries only the engine's own alpha-derived
/// intent, never a reference to another party's order or wallet activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderIntent {
    pub venue: VenueId,
    pub contract: CanonicalContractId,
    pub outcome: Outcome,
    pub side: Side,
    /// Limit price as a probability in `[0.0, 1.0]`.
    pub price: f64,
    pub size: f64,
    pub order_type: OrderType,
    pub engine: EngineId,
    pub created_at: Timestamp,
}

impl OrderIntent {
    /// Every field a risk check or venue adapter is about to compare must
    /// clear this first — a NaN or out-of-range `price`/`size` compares
    /// false against every `>` limit check downstream, which reads as
    /// "approved" (design doc review 1.1).
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate::probability("price", self.price)?;
        validate::positive("size", self.size)?;
        Ok(())
    }

    /// Money genuinely at risk if this order fills and is never closed.
    /// For a buy, the max loss is paying `price` per share if the
    /// contract resolves NO: `size * price`. For a sell (a short YES),
    /// the max loss is the contract resolving YES: `size * (1 - price)`.
    /// Using `size * price` for both sides — as a naive notional
    /// calculation would — charges a cheap short (e.g. sell 500 @ 0.02)
    /// almost nothing for the same maximum loss a symmetric buy would be
    /// charged in full (design doc review 4.4).
    pub fn risk_notional(&self) -> f64 {
        match self.side {
            Side::Buy => self.size * self.price,
            Side::Sell => self.size * (1.0 - self.price),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AckStatus {
    Accepted,
    Rejected {
        reason: String,
    },
    Filled {
        qty: f64,
        price: f64,
    },
    PartiallyFilled {
        filled_qty: f64,
        remaining_qty: f64,
        price: f64,
    },
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderAck {
    pub order_id: OrderId,
    pub venue: VenueId,
    pub status: AckStatus,
    pub ts: Timestamp,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("venue {venue} rejected order: {reason}")]
    Rejected { venue: VenueId, reason: String },
    #[error("rate limited by venue {venue}, retry after {retry_after_ms}ms")]
    RateLimited { venue: VenueId, retry_after_ms: u64 },
    #[error("venue {venue} connection error: {message}")]
    Connection { venue: VenueId, message: String },
    #[error("order {0} not found")]
    NotFound(OrderId),
    /// A request timed out (or the connection dropped) *after* it was
    /// sent: the venue may or may not have received and acted on it. The
    /// standard failure mode — submit, timeout, retry — places the order
    /// twice when the first attempt actually landed, and the duplicate is
    /// invisible to the risk gate because the first was never acknowledged
    /// (design doc review 1.7). This case requires reconciling against the
    /// venue's own order/position list before doing anything else.
    #[error("venue {venue} request outcome is indeterminate — reconcile before retrying")]
    Indeterminate { venue: VenueId },
}

impl ExecError {
    /// `false` means a blind retry is unsafe: the request may have already
    /// reached the venue, and resending the same intent risks submitting
    /// it twice. Only `Indeterminate` carries that ambiguity — every other
    /// variant means the venue either never saw the request or explicitly
    /// told us it didn't take effect.
    pub fn is_safely_retryable(&self) -> bool {
        !matches!(self, ExecError::Indeterminate { .. })
    }
}

/// A deterministic idempotency key derived from an order's own content via
/// FNV-1a, so retrying the *same* `OrderIntent` value produces the same
/// key — letting the venue deduplicate a resend after a timeout — while a
/// genuinely different order (a new `created_at`, a different price/size)
/// always gets a different one (design doc review 1.7).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientOrderId(pub String);

impl fmt::Display for ClientOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

impl ClientOrderId {
    pub fn derive(intent: &OrderIntent) -> Self {
        let content = format!(
            "{}|{}|{:?}|{:?}|{}|{}|{:?}|{:?}|{}",
            intent.venue,
            intent.contract.0,
            intent.outcome,
            intent.side,
            intent.price,
            intent.size,
            intent.order_type,
            intent.engine,
            intent.created_at.as_nanos(),
        );
        ClientOrderId(format!("plx-{:016x}", fnv1a_64(content.as_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(price: f64, size: f64, created_at_ns: i64) -> OrderIntent {
        OrderIntent {
            venue: VenueId::Kalshi,
            contract: CanonicalContractId("wx.temp.chicago.gt_869.2026-08-12.nws_official".into()),
            outcome: Outcome::Yes,
            side: Side::Buy,
            price,
            size,
            order_type: OrderType::Limit,
            engine: EngineId::MarketMaking,
            created_at: Timestamp::from_nanos(created_at_ns),
        }
    }

    #[test]
    fn validate_rejects_nan_price() {
        assert!(intent(f64::NAN, 10.0, 0).validate().is_err());
    }

    #[test]
    fn validate_rejects_nonpositive_size() {
        assert!(intent(0.5, 0.0, 0).validate().is_err());
        assert!(intent(0.5, -1.0, 0).validate().is_err());
    }

    #[test]
    fn validate_accepts_a_well_formed_intent() {
        assert!(intent(0.5, 10.0, 0).validate().is_ok());
    }

    #[test]
    fn risk_notional_charges_a_sell_by_its_actual_max_loss() {
        let mut buy = intent(0.98, 500.0, 0);
        buy.side = Side::Buy;
        let mut sell = intent(0.02, 500.0, 0);
        sell.side = Side::Sell;
        // A buy at 0.98 and a sell at 0.02 are symmetric max-loss bets
        // (both lose ~490 if wrong) and must be charged near-identically.
        assert!((buy.risk_notional() - sell.risk_notional()).abs() < 1e-9);
    }

    #[test]
    fn client_order_id_is_deterministic_for_the_same_intent() {
        let a = intent(0.5, 10.0, 123);
        let b = intent(0.5, 10.0, 123);
        assert_eq!(ClientOrderId::derive(&a), ClientOrderId::derive(&b));
    }

    #[test]
    fn client_order_id_differs_for_a_genuinely_different_intent() {
        let a = intent(0.5, 10.0, 123);
        let b = intent(0.5, 10.0, 456);
        assert_ne!(ClientOrderId::derive(&a), ClientOrderId::derive(&b));
    }

    #[test]
    fn indeterminate_is_the_only_unsafe_retry() {
        assert!(!ExecError::Indeterminate {
            venue: VenueId::Kalshi
        }
        .is_safely_retryable());
        assert!(ExecError::Connection {
            venue: VenueId::Kalshi,
            message: "x".into()
        }
        .is_safely_retryable());
        assert!(ExecError::RateLimited {
            venue: VenueId::Kalshi,
            retry_after_ms: 100
        }
        .is_safely_retryable());
    }
}
