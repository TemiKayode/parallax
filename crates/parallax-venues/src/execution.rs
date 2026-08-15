//! `docs/GOING-LIVE.md` Stage 1, the idempotency section:
//!
//! > You send an order. The connection times out. You do not know
//! > whether it arrived. You retry. It had arrived. You are now twice
//! > the size you intended, on a position you sized with Kelly.
//! >
//! > Every order must carry a client-generated order ID, and the retry
//! > path must be idempotent on that ID. Before any retry, query order
//! > state by client ID; only resend if the venue has no record. A
//! > timeout is not a rejection — treat unknown as "possibly filled"
//! > until proven otherwise, and never let the unknown state authorise a
//! > second order.
//!
//! `ClientOrderId::derive` and `ExecError::Indeterminate` already existed
//! as types (`parallax-types`) before this module — the deterministic id
//! was already threaded into `KalshiAdapter`/`PolymarketAdapter`'s
//! request construction, and the distinct "ambiguous outcome" error
//! variant was already carved out. What didn't exist anywhere was the
//! orchestration that actually *uses* them: this is that.

use parallax_types::{ClientOrderId, ExecError, OrderAck, OrderIntent};

use crate::adapter::VenueAdapter;

/// What happened when PARALLAX tried to place an order.
#[derive(Debug, Clone, PartialEq)]
pub enum SubmitOutcome {
    /// The venue's status for this order is known, whether that's a fill,
    /// a rejection, or a resting accept.
    Resolved(OrderAck),
    /// The venue's status for this order could not be established. Per
    /// the rule above, this is *not* a license to resend — the caller
    /// must treat the position as unknown and stay flat until a human (or
    /// a later, successful reconciliation pass) resolves it.
    Unresolved {
        client_order_id: ClientOrderId,
        reason: String,
    },
}

/// Submits `intent`, idempotently. The common cases (a clean
/// accept/reject/fill, or an unambiguous connection/rate-limit error) are
/// resolved or refused in one call, no different from calling
/// `venue.submit` directly. The one case this adds real behavior for is
/// `ExecError::Indeterminate` — a request that may or may not have
/// reached the venue: rather than resend blindly (the bug this whole
/// module exists to prevent) or give up entirely, it queries the venue
/// for this order's deterministic client id and only resends if that
/// query *confirms* the venue never saw it.
///
/// Deliberately out of scope: a retry policy (backoff, max attempts,
/// jitter) for the *safely* retryable errors (`Connection`,
/// `RateLimited`) that aren't `Indeterminate`. Those are safe to retry in
/// the sense that they can't cause a duplicate fill, but *how many times,
/// how fast* is a real design decision on its own — this function treats
/// them as `Unresolved` and leaves retrying them to the caller, rather
/// than picking a policy nobody asked for.
pub async fn submit_idempotent(venue: &dyn VenueAdapter, intent: OrderIntent) -> SubmitOutcome {
    let client_order_id = ClientOrderId::derive(&intent);

    match venue.submit(intent.clone()).await {
        Ok(ack) => SubmitOutcome::Resolved(ack),

        Err(ExecError::Indeterminate { .. }) => {
            match venue.find_order_by_client_id(&client_order_id).await {
                // The venue already has it — the original attempt landed.
                // Report its real status; do not send a second order.
                Ok(Some(ack)) => SubmitOutcome::Resolved(ack),

                // Confirmed: the venue has no record. This is the one
                // condition the design doc licenses a resend under, and
                // it's safe *because* it's confirmed, not assumed.
                Ok(None) => match venue.submit(intent).await {
                    Ok(ack) => SubmitOutcome::Resolved(ack),
                    Err(e) => SubmitOutcome::Unresolved {
                        client_order_id,
                        reason: format!(
                            "confirmed no prior record, but the resend itself failed: {e}"
                        ),
                    },
                },

                // The lookup itself failed — still don't know whether the
                // original order landed. Refusing to guess here is the
                // entire point of this function.
                Err(lookup_err) => SubmitOutcome::Unresolved {
                    client_order_id,
                    reason: format!(
                        "submit outcome was indeterminate, and confirming it failed too: {lookup_err}"
                    ),
                },
            }
        }

        Err(e) => SubmitOutcome::Unresolved {
            client_order_id,
            reason: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parallax_types::{
        AckStatus, CanonicalContractId, EngineId, FeeModel, OrderId, OrderType, Outcome, Position,
        SettlementModel, Side, Timestamp, VenueCapabilities, VenueId,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn intent() -> OrderIntent {
        OrderIntent {
            venue: VenueId::Kalshi,
            contract: CanonicalContractId("wx.temp.chicago.gt_869.test.nws_official".into()),
            outcome: Outcome::Yes,
            side: Side::Buy,
            price: 0.5,
            size: 10.0,
            order_type: OrderType::Limit,
            engine: EngineId::MarketMaking,
            created_at: Timestamp::from_nanos(0),
        }
    }

    fn accepted_ack() -> OrderAck {
        OrderAck {
            order_id: OrderId("fake-1".into()),
            venue: VenueId::Kalshi,
            status: AckStatus::Accepted,
            ts: Timestamp::from_nanos(0),
        }
    }

    /// A controllable double for exercising `submit_idempotent`'s
    /// decision tree without a real venue: each queue is drained in
    /// order, one entry per call, so a test can script an exact sequence
    /// (e.g. "first submit is Indeterminate, then the lookup finds
    /// nothing, then the resend succeeds").
    #[derive(Default)]
    struct FakeAdapter {
        submit_results: Mutex<Vec<Result<OrderAck, ExecError>>>,
        lookup_results: Mutex<Vec<Result<Option<OrderAck>, ExecError>>>,
        submit_calls: AtomicUsize,
        lookup_calls: AtomicUsize,
    }

    #[async_trait]
    impl VenueAdapter for FakeAdapter {
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
            self.submit_calls.fetch_add(1, Ordering::SeqCst);
            self.submit_results.lock().unwrap().remove(0)
        }
        async fn cancel(&self, _order_id: OrderId) -> Result<(), ExecError> {
            Ok(())
        }
        async fn find_order_by_client_id(
            &self,
            _client_order_id: &ClientOrderId,
        ) -> Result<Option<OrderAck>, ExecError> {
            self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            self.lookup_results.lock().unwrap().remove(0)
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExecError> {
            Ok(Vec::new())
        }
        async fn list_open_orders(&self) -> Result<Vec<OrderId>, ExecError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn a_clean_accept_resolves_without_any_lookup() {
        let venue = FakeAdapter {
            submit_results: Mutex::new(vec![Ok(accepted_ack())]),
            ..Default::default()
        };
        let outcome = submit_idempotent(&venue, intent()).await;
        assert_eq!(outcome, SubmitOutcome::Resolved(accepted_ack()));
        assert_eq!(venue.lookup_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn indeterminate_then_confirmed_absent_resends_exactly_once() {
        let venue = FakeAdapter {
            submit_results: Mutex::new(vec![
                Err(ExecError::Indeterminate {
                    venue: VenueId::Kalshi,
                }),
                Ok(accepted_ack()),
            ]),
            lookup_results: Mutex::new(vec![Ok(None)]),
            ..Default::default()
        };
        let outcome = submit_idempotent(&venue, intent()).await;
        assert_eq!(outcome, SubmitOutcome::Resolved(accepted_ack()));
        assert_eq!(venue.submit_calls.load(Ordering::SeqCst), 2);
        assert_eq!(venue.lookup_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn indeterminate_then_found_does_not_resend() {
        let already_filled = OrderAck {
            status: AckStatus::Filled {
                qty: 10.0,
                price: 0.5,
            },
            ..accepted_ack()
        };
        let venue = FakeAdapter {
            submit_results: Mutex::new(vec![Err(ExecError::Indeterminate {
                venue: VenueId::Kalshi,
            })]),
            lookup_results: Mutex::new(vec![Ok(Some(already_filled.clone()))]),
            ..Default::default()
        };
        let outcome = submit_idempotent(&venue, intent()).await;
        assert_eq!(outcome, SubmitOutcome::Resolved(already_filled));
        // Exactly one submit call — the original. A second submit here
        // would be precisely the double-order bug this module exists to
        // prevent.
        assert_eq!(venue.submit_calls.load(Ordering::SeqCst), 1);
        assert_eq!(venue.lookup_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn indeterminate_then_a_failed_lookup_stays_unresolved_and_never_resends() {
        let venue = FakeAdapter {
            submit_results: Mutex::new(vec![Err(ExecError::Indeterminate {
                venue: VenueId::Kalshi,
            })]),
            lookup_results: Mutex::new(vec![Err(ExecError::Connection {
                venue: VenueId::Kalshi,
                message: "network down".into(),
            })]),
            ..Default::default()
        };
        let outcome = submit_idempotent(&venue, intent()).await;
        match outcome {
            SubmitOutcome::Unresolved { .. } => {}
            SubmitOutcome::Resolved(_) => panic!("must not resolve when the lookup itself failed"),
        }
        assert_eq!(venue.submit_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_clean_rejection_is_unresolved_without_any_lookup_or_resend() {
        // Rejected means the venue explicitly said no — there's nothing
        // ambiguous to reconcile, and resending the identical order would
        // just be rejected again.
        let venue = FakeAdapter {
            submit_results: Mutex::new(vec![Err(ExecError::Rejected {
                venue: VenueId::Kalshi,
                reason: "insufficient balance".into(),
            })]),
            ..Default::default()
        };
        let outcome = submit_idempotent(&venue, intent()).await;
        match outcome {
            SubmitOutcome::Unresolved { reason, .. } => {
                assert!(reason.contains("insufficient balance"))
            }
            SubmitOutcome::Resolved(_) => panic!("a rejection is not a resolution"),
        }
        assert_eq!(venue.submit_calls.load(Ordering::SeqCst), 1);
        assert_eq!(venue.lookup_calls.load(Ordering::SeqCst), 0);
    }
}
