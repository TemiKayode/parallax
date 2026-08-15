//! Shared vocabulary for every PARALLAX crate: the canonical contract
//! schema, event envelopes, order/position types, and venue metadata.
//! Intentionally free of any I/O, strategy, or venue-specific logic —
//! everything here is plain data so it can cross the hot-path ring buffer
//! by value and be replayed byte-identically in the sim harness.

#![forbid(unsafe_code)]

mod alpha;
mod contract;
mod market;
mod orders;
mod position;
mod time;
mod validate;
mod venue;

pub use alpha::{
    AlphaEventKind, EstimateKind, FairValue, ProbabilityEstimate, RawEvent, StalenessPolicy,
};
pub use contract::{CanonicalContractId, CanonicalContractSpec, ClusterKey, Direction, EventClass};
pub use market::{BookDepth, DepthLevel, NormalizedTick, WalkResult};
pub use orders::{
    AckStatus, ClientOrderId, EngineId, ExecError, OrderAck, OrderId, OrderIntent, OrderType,
    Outcome, Side,
};
pub use position::Position;
pub use time::Timestamp;
pub use validate::{finite, non_negative, positive, probability, ValidationError};
pub use venue::{FeeModel, SettlementModel, VenueCapabilities, VenueId};
