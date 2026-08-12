use crate::contract::CanonicalContractId;
use crate::time::Timestamp;
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
}
