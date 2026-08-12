use async_trait::async_trait;
use parallax_types::{ExecError, OrderAck, OrderId, OrderIntent, VenueCapabilities, VenueId};

/// The plugin boundary every venue implements (design doc §9/§17). New
/// venues are added by implementing this trait — nothing upstream (the
/// strategy core, the risk gate) needs to change.
///
/// `async_trait` is used deliberately instead of native `async fn` in
/// traits: this crate holds adapters as `Vec<Box<dyn VenueAdapter>>`, and
/// native async trait methods are not object-safe on stable Rust without
/// manually boxing every return future. `async_trait` does that boxing
/// for us at the cost of one allocation per call — irrelevant next to a
/// network round trip, and nowhere near the microsecond-budgeted
/// in-process hot path described in design doc §13.
#[async_trait]
pub trait VenueAdapter: Send + Sync {
    fn venue_id(&self) -> VenueId;
    fn capabilities(&self) -> VenueCapabilities;
    async fn submit(&self, order: OrderIntent) -> Result<OrderAck, ExecError>;
    async fn cancel(&self, order_id: OrderId) -> Result<(), ExecError>;
}
