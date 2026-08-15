use async_trait::async_trait;
use parallax_types::{
    ClientOrderId, ExecError, OrderAck, OrderId, OrderIntent, Position, VenueCapabilities, VenueId,
};

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

    /// Looks up an order's last known status at the venue by the
    /// deterministic id `ClientOrderId::derive` computes for the intent
    /// that produced it. `Ok(None)` means the venue has no record of this
    /// order at all — the specific, narrow condition
    /// `docs/GOING-LIVE.md` Stage 1 requires before a timed-out submit is
    /// safe to resend: "before any retry, query order state by client
    /// ID; only resend if the venue has no record." See
    /// `execution::submit_idempotent`, the only caller that should ever
    /// use this to decide whether to retry.
    async fn find_order_by_client_id(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Result<Option<OrderAck>, ExecError>;

    /// The venue's own view of every open position on this account — the
    /// ground truth `reconcile::reconcile_startup` loads into the risk
    /// gate before it will approve a single order.
    /// `docs/GOING-LIVE.md` Stage 1: "the venue is always right. Never
    /// trade until local state and venue state agree."
    async fn fetch_positions(&self) -> Result<Vec<Position>, ExecError>;

    /// The venue's own view of every currently-working (resting,
    /// unfilled) order on this account. `docs/GOING-LIVE.md` Stage 1 asks
    /// for this at the same three points as `fetch_positions` ("fetch
    /// positions *and* working orders"); Stage 2's out-of-band cancel
    /// path is the other consumer — you cannot cancel-all without first
    /// knowing what's open.
    async fn list_open_orders(&self) -> Result<Vec<OrderId>, ExecError>;
}
