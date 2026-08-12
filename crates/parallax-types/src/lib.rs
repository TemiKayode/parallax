//! Shared vocabulary for every PARALLAX crate: the canonical contract
//! schema, event envelopes, order/position types, and venue metadata.
//! Intentionally free of any I/O, strategy, or venue-specific logic —
//! everything here is plain data so it can cross the hot-path ring buffer
//! by value and be replayed byte-identically in the sim harness.

mod alpha;
mod contract;
mod market;
mod orders;
mod position;
mod time;
mod venue;

pub use alpha::{AlphaEventKind, FairValue, ProbabilityEstimate, RawEvent};
pub use contract::{CanonicalContractId, CanonicalContractSpec, ClusterKey, Direction, EventClass};
pub use market::NormalizedTick;
pub use orders::{
    AckStatus, EngineId, ExecError, OrderAck, OrderId, OrderIntent, OrderType, Outcome, Side,
};
pub use position::Position;
pub use time::Timestamp;
pub use venue::{SettlementModel, VenueCapabilities, VenueId};
