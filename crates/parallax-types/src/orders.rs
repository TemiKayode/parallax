use crate::contract::CanonicalContractId;
use crate::time::Timestamp;
use crate::venue::VenueId;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
