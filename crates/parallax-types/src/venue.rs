use serde::{Deserialize, Serialize};

/// Every tradable venue PARALLAX knows about. `Paper` is the in-memory
/// simulated venue used by the sim harness and by shadow mode — it is not
/// a stub to be deleted later, it is a first-class adapter target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    /// Conservative request budget PARALLAX will self-throttle to — always
    /// set below the venue's published limit, never skated against it.
    pub rate_limit_per_sec: u32,
}
