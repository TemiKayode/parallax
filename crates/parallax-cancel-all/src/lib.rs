//! `docs/GOING-LIVE.md` Stage 2, "an out-of-band cancel path":
//!
//! > A second, tiny, separately-deployed process (or a documented
//! > one-line script) that can `cancel-all` without importing any
//! > strategy code. When the main system is the problem, you cannot use
//! > the main system to fix it.
//!
//! This crate is that isolation, structurally: its `Cargo.toml` depends
//! on `parallax-types` and `parallax-venues` and nothing else — no
//! `parallax-strategy`, `parallax-risk`, `parallax-alpha`, or
//! `parallax-sim` anywhere in its dependency graph. A bug, a panic, or
//! even just a slow compile in any of those crates cannot take this
//! binary down with it, because it was never linked against them in the
//! first place. That's not a coding convention someone could
//! accidentally violate one import at a time — it's enforced by what's
//! literally declared as a dependency.

#![forbid(unsafe_code)]

use parallax_types::{ExecError, OrderId};
use parallax_venues::VenueAdapter;

/// What happened when `cancel_all` tried to cancel every order the venue
/// reports as currently open.
#[derive(Debug)]
pub struct CancelAllReport {
    pub attempted: usize,
    pub canceled: usize,
    /// Every order that failed to cancel, with why. A failure here
    /// doesn't stop the rest — during an actual fault, canceling 9 of 10
    /// resting orders and reporting the 10th loudly is a far better
    /// outcome than aborting after the first failure and leaving all 10
    /// live.
    pub failed: Vec<(OrderId, String)>,
}

impl CancelAllReport {
    pub fn all_succeeded(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Lists every order `venue` reports as open, then attempts to cancel
/// every single one, regardless of whether an earlier one failed.
/// Propagates an error only if the *listing* itself fails — at that
/// point there is nothing known to iterate over, and reporting zero
/// attempts would misleadingly look identical to "nothing was open."
pub async fn cancel_all(venue: &dyn VenueAdapter) -> Result<CancelAllReport, ExecError> {
    let open_orders = venue.list_open_orders().await?;
    let attempted = open_orders.len();

    let mut canceled = 0usize;
    let mut failed = Vec::new();
    for order_id in open_orders {
        match venue.cancel(order_id.clone()).await {
            Ok(()) => canceled += 1,
            Err(e) => failed.push((order_id, e.to_string())),
        }
    }

    Ok(CancelAllReport {
        attempted,
        canceled,
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parallax_types::{
        ClientOrderId, FeeModel, OrderAck, OrderIntent, Position, SettlementModel,
        VenueCapabilities, VenueId,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A controllable double: a fixed set of "open" order ids, and a
    /// scripted cancel outcome per id.
    struct FakeVenue {
        open_orders: Vec<OrderId>,
        cancel_results: Mutex<HashMap<String, Result<(), ExecError>>>,
        list_fails: bool,
    }

    #[async_trait]
    impl VenueAdapter for FakeVenue {
        fn venue_id(&self) -> VenueId {
            VenueId::Kalshi
        }
        fn capabilities(&self) -> VenueCapabilities {
            VenueCapabilities {
                venue: VenueId::Kalshi,
                settlement: SettlementModel::CentralLimitOrderBook,
                min_tick: 0.01,
                min_order_size: 1.0,
                fee_model: FeeModel::kalshi_default(),
                rate_limit_per_sec: 10,
            }
        }
        async fn submit(&self, _order: OrderIntent) -> Result<OrderAck, ExecError> {
            unimplemented!("not exercised by these tests")
        }
        async fn cancel(&self, order_id: OrderId) -> Result<(), ExecError> {
            self.cancel_results
                .lock()
                .unwrap()
                .remove(&order_id.0)
                .unwrap_or(Ok(()))
        }
        async fn find_order_by_client_id(
            &self,
            _client_order_id: &ClientOrderId,
        ) -> Result<Option<OrderAck>, ExecError> {
            unimplemented!("not exercised by these tests")
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExecError> {
            unimplemented!("not exercised by these tests")
        }
        async fn list_open_orders(&self) -> Result<Vec<OrderId>, ExecError> {
            if self.list_fails {
                Err(ExecError::Connection {
                    venue: VenueId::Kalshi,
                    message: "listing failed".into(),
                })
            } else {
                Ok(self.open_orders.clone())
            }
        }
    }

    #[tokio::test]
    async fn cancels_every_open_order_when_all_succeed() {
        let venue = FakeVenue {
            open_orders: vec![
                OrderId("a".into()),
                OrderId("b".into()),
                OrderId("c".into()),
            ],
            cancel_results: Mutex::new(HashMap::new()),
            list_fails: false,
        };
        let report = cancel_all(&venue).await.unwrap();
        assert_eq!(report.attempted, 3);
        assert_eq!(report.canceled, 3);
        assert!(report.all_succeeded());
    }

    #[tokio::test]
    async fn a_failed_cancel_does_not_stop_the_rest() {
        let mut cancel_results = HashMap::new();
        cancel_results.insert(
            "b".to_string(),
            Err(ExecError::Connection {
                venue: VenueId::Kalshi,
                message: "network blip".into(),
            }),
        );
        let venue = FakeVenue {
            open_orders: vec![
                OrderId("a".into()),
                OrderId("b".into()),
                OrderId("c".into()),
            ],
            cancel_results: Mutex::new(cancel_results),
            list_fails: false,
        };
        let report = cancel_all(&venue).await.unwrap();
        // a and c still get canceled even though b failed.
        assert_eq!(report.attempted, 3);
        assert_eq!(report.canceled, 2);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, OrderId("b".into()));
        assert!(!report.all_succeeded());
    }

    #[tokio::test]
    async fn no_open_orders_is_a_trivially_successful_no_op() {
        let venue = FakeVenue {
            open_orders: Vec::new(),
            cancel_results: Mutex::new(HashMap::new()),
            list_fails: false,
        };
        let report = cancel_all(&venue).await.unwrap();
        assert_eq!(report.attempted, 0);
        assert!(report.all_succeeded());
    }

    #[tokio::test]
    async fn a_failed_listing_propagates_rather_than_reporting_a_false_zero() {
        let venue = FakeVenue {
            open_orders: Vec::new(),
            cancel_results: Mutex::new(HashMap::new()),
            list_fails: true,
        };
        assert!(cancel_all(&venue).await.is_err());
    }
}
