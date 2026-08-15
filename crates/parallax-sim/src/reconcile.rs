//! `docs/GOING-LIVE.md` Stage 1, "reconciliation that actually
//! reconciles":
//!
//! > The venue is always right. Never trade until local state and venue
//! > state agree. On disagreement, adopt the venue's view, log the delta
//! > loudly, and stay flat until a human has looked at it.
//! >
//! > Fetch positions *and* working orders at startup, then on a
//! > schedule, then after every disconnect.
//!
//! `RiskGate::set_position`/`mark_reconciled` already existed and already
//! enforced their half of this (`check` refuses every order until
//! `mark_reconciled` is called) — what didn't exist was the code that
//! actually calls them with real data. `reconcile_startup` here is that
//! code, for the first of the three triggers the doc lists ("at
//! startup"); the scheduled and post-disconnect triggers are the same
//! call, made again later — see the caveat on `reconcile_startup` for
//! what's specifically *not* handled yet (detecting drift against an
//! already-populated gate, rather than populating an empty one).

use parallax_risk::RiskGate;
use parallax_types::ClientOrderId;
use parallax_venues::VenueAdapter;
use std::path::Path;

/// What `reconcile_startup` found and did. A caller logs this (or
/// surfaces it to an operator) to know not just *whether* trading is
/// allowed now, but exactly what was resolved and what wasn't.
#[derive(Debug)]
pub struct ReconciliationReport {
    /// Positions loaded into the risk gate from the venue's own report.
    pub positions_loaded: usize,
    /// Currently-working orders the venue reports as open right now —
    /// visibility for an operator (or an out-of-band cancel-all) into
    /// what's actually live, independent of anything this process's own
    /// memory or journal believes.
    pub open_orders: Vec<parallax_types::OrderId>,
    /// Journaled orders whose true state the venue confirmed — either it
    /// has the order (already reflected in the position fetch below) or
    /// it confirmed no record of it (safely dropped, nothing to do).
    pub orders_resolved: usize,
    /// Journaled orders the venue could not give a confirmed answer for.
    /// Non-empty here is why `gate_reconciled` is `false`: an order this
    /// module can't confirm as filled, rejected, or never-received is
    /// exactly the "unknown position" state `docs/GOING-LIVE.md` says
    /// must never be traded through.
    pub orders_unresolved: Vec<(ClientOrderId, String)>,
    /// `true` only if every journaled order resolved *and* the position
    /// fetch succeeded — the two conditions `RiskGate::mark_reconciled`
    /// was called under. `false` means the gate is still refusing every
    /// order (`RejectReason::NotReconciled`), by design.
    pub gate_reconciled: bool,
}

/// Reconciles `gate` against `venue`'s own reported truth before any
/// trading is allowed to start: recovers any order left unresolved in
/// `journal_path` by a prior crash and confirms its real outcome with the
/// venue, then loads every position the venue reports and marks the gate
/// ready.
///
/// Only ever *loosens* nothing: if a single journaled order can't be
/// confirmed, or the position fetch itself fails, the gate is left
/// unreconciled and every order stays refused — reconciliation either
/// fully succeeds or changes nothing, never partially trusts a fetch that
/// only half-completed.
///
/// This handles the *first* of the three triggers `docs/GOING-LIVE.md`
/// lists ("at startup"); calling it again is the same mechanism for "on a
/// schedule" and "after every disconnect", but doing that well needs one
/// more piece this doesn't have yet: detecting *drift* in an
/// already-populated gate (a position that changed since the last
/// reconciliation, not just an empty map being filled for the first
/// time) and logging the delta loudly, per the doc's own wording. Calling
/// this on a non-empty gate today just overwrites each contract's entry
/// silently — correct, but not yet the loud discrepancy log Stage 1
/// asks for.
pub async fn reconcile_startup(
    venue: &dyn VenueAdapter,
    gate: &mut RiskGate,
    journal_path: &Path,
) -> std::io::Result<ReconciliationReport> {
    let unresolved_intents = parallax_venues::recover_unresolved(journal_path)?;

    let mut orders_unresolved = Vec::new();
    let mut orders_resolved = 0usize;
    for (client_order_id, _intent) in &unresolved_intents {
        match venue.find_order_by_client_id(client_order_id).await {
            // Either the venue has it (its effect is already in the
            // position fetch below) or it confirmed no record at all
            // (never happened) — both are a settled, known outcome.
            Ok(Some(_)) | Ok(None) => orders_resolved += 1,
            Err(e) => orders_unresolved.push((client_order_id.clone(), e.to_string())),
        }
    }

    if !orders_unresolved.is_empty() {
        return Ok(ReconciliationReport {
            positions_loaded: 0,
            open_orders: Vec::new(),
            orders_resolved,
            orders_unresolved,
            gate_reconciled: false,
        });
    }

    let positions = match venue.fetch_positions().await {
        Ok(positions) => positions,
        Err(e) => {
            return Ok(ReconciliationReport {
                positions_loaded: 0,
                open_orders: Vec::new(),
                orders_resolved,
                orders_unresolved: vec![(ClientOrderId("(position fetch)".into()), e.to_string())],
                gate_reconciled: false,
            });
        }
    };

    let open_orders = match venue.list_open_orders().await {
        Ok(open_orders) => open_orders,
        Err(e) => {
            return Ok(ReconciliationReport {
                positions_loaded: 0,
                open_orders: Vec::new(),
                orders_resolved,
                orders_unresolved: vec![(
                    ClientOrderId("(open-order listing)".into()),
                    e.to_string(),
                )],
                gate_reconciled: false,
            });
        }
    };

    let mut positions_loaded = 0usize;
    for position in positions {
        // A position failing validate() is dropped, not trusted anyway —
        // set_position already refuses to apply it (design doc review
        // 4.2); this just doesn't count it as loaded.
        if gate.set_position(position).is_ok() {
            positions_loaded += 1;
        }
    }

    gate.mark_reconciled();

    Ok(ReconciliationReport {
        positions_loaded,
        open_orders,
        orders_resolved,
        orders_unresolved: Vec::new(),
        gate_reconciled: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parallax_risk::RiskLimits;
    use parallax_types::{
        CanonicalContractId, EngineId, ExecError, FeeModel, OrderAck, OrderId, OrderIntent,
        OrderType, Outcome, Position, SettlementModel, Side, Timestamp, VenueCapabilities, VenueId,
    };
    use parallax_venues::OrderJournal;
    use std::sync::Mutex;

    fn contract() -> CanonicalContractId {
        CanonicalContractId("wx.temp.chicago.gt_869.test.nws_official".into())
    }

    fn intent() -> OrderIntent {
        OrderIntent {
            venue: VenueId::Kalshi,
            contract: contract(),
            outcome: Outcome::Yes,
            side: Side::Buy,
            price: 0.5,
            size: 10.0,
            order_type: OrderType::Limit,
            engine: EngineId::MarketMaking,
            created_at: Timestamp::from_nanos(0),
        }
    }

    fn temp_journal_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "parallax_reconcile_test_{}_{name}",
            std::process::id()
        ));
        p
    }

    /// A controllable double: scripted lookup/position/open-order
    /// results, no real submit path needed for these tests.
    #[derive(Default)]
    struct FakeVenue {
        lookup_results: Mutex<Vec<Result<Option<OrderAck>, ExecError>>>,
        positions: Mutex<Option<Result<Vec<Position>, ExecError>>>,
        open_orders: Mutex<Option<Result<Vec<OrderId>, ExecError>>>,
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
        async fn cancel(&self, _order_id: OrderId) -> Result<(), ExecError> {
            unimplemented!("not exercised by these tests")
        }
        async fn find_order_by_client_id(
            &self,
            _client_order_id: &ClientOrderId,
        ) -> Result<Option<OrderAck>, ExecError> {
            self.lookup_results.lock().unwrap().remove(0)
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExecError> {
            self.positions
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Ok(Vec::new()))
        }
        async fn list_open_orders(&self) -> Result<Vec<OrderId>, ExecError> {
            self.open_orders
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Ok(Vec::new()))
        }
    }

    #[tokio::test]
    async fn an_empty_journal_reconciles_purely_from_the_position_fetch() {
        let path = temp_journal_path("empty.jsonl");
        std::fs::remove_file(&path).ok();
        let venue = FakeVenue {
            positions: Mutex::new(Some(Ok(vec![Position {
                venue: VenueId::Kalshi,
                contract: contract(),
                qty: 5.0,
                avg_price: 0.6,
                realized_pnl: 0.0,
                fees_paid: 0.0,
            }]))),
            ..Default::default()
        };
        let mut gate = RiskGate::new(RiskLimits::default());
        assert!(!gate.is_reconciled());

        let report = reconcile_startup(&venue, &mut gate, &path).await.unwrap();
        std::fs::remove_file(&path).ok();

        assert!(report.gate_reconciled);
        assert_eq!(report.positions_loaded, 1);
        assert!(gate.is_reconciled());
    }

    #[tokio::test]
    async fn a_journaled_order_the_venue_confirms_it_never_saw_still_reconciles() {
        let path = temp_journal_path("confirmed_absent.jsonl");
        std::fs::remove_file(&path).ok();
        let id = ClientOrderId::derive(&intent());
        {
            let mut journal = OrderJournal::open(&path).unwrap();
            journal.log_intent(&id, &intent()).unwrap();
        }
        let venue = FakeVenue {
            lookup_results: Mutex::new(vec![Ok(None)]),
            positions: Mutex::new(Some(Ok(Vec::new()))),
            ..Default::default()
        };
        let mut gate = RiskGate::new(RiskLimits::default());

        let report = reconcile_startup(&venue, &mut gate, &path).await.unwrap();
        std::fs::remove_file(&path).ok();

        assert!(report.gate_reconciled);
        assert_eq!(report.orders_resolved, 1);
        assert!(report.orders_unresolved.is_empty());
    }

    #[tokio::test]
    async fn a_journaled_order_the_venue_cannot_confirm_blocks_reconciliation() {
        let path = temp_journal_path("blocked.jsonl");
        std::fs::remove_file(&path).ok();
        let id = ClientOrderId::derive(&intent());
        {
            let mut journal = OrderJournal::open(&path).unwrap();
            journal.log_intent(&id, &intent()).unwrap();
        }
        let venue = FakeVenue {
            lookup_results: Mutex::new(vec![Err(ExecError::Connection {
                venue: VenueId::Kalshi,
                message: "timed out".into(),
            })]),
            positions: Mutex::new(Some(Ok(Vec::new()))),
            ..Default::default()
        };
        let mut gate = RiskGate::new(RiskLimits::default());

        let report = reconcile_startup(&venue, &mut gate, &path).await.unwrap();
        std::fs::remove_file(&path).ok();

        assert!(!report.gate_reconciled);
        assert_eq!(report.orders_unresolved.len(), 1);
        assert!(!gate.is_reconciled());
        // Never trade until local state and venue state agree — the
        // gate must still refuse everything after a blocked
        // reconciliation, exactly as if reconciliation had never run.
        let book = parallax_book::ConsolidatedBook::new();
        let check_intent = intent();
        assert!(gate
            .check(&check_intent, &book, Timestamp::from_nanos(0))
            .is_err());
    }

    #[tokio::test]
    async fn a_failed_position_fetch_also_blocks_reconciliation() {
        let path = temp_journal_path("fetch_failed.jsonl");
        std::fs::remove_file(&path).ok();
        let venue = FakeVenue {
            positions: Mutex::new(Some(Err(ExecError::Connection {
                venue: VenueId::Kalshi,
                message: "500".into(),
            }))),
            ..Default::default()
        };
        let mut gate = RiskGate::new(RiskLimits::default());

        let report = reconcile_startup(&venue, &mut gate, &path).await.unwrap();
        std::fs::remove_file(&path).ok();

        assert!(!report.gate_reconciled);
        assert!(!gate.is_reconciled());
    }

    #[tokio::test]
    async fn a_failed_open_order_listing_also_blocks_reconciliation() {
        let path = temp_journal_path("open_orders_failed.jsonl");
        std::fs::remove_file(&path).ok();
        let venue = FakeVenue {
            positions: Mutex::new(Some(Ok(Vec::new()))),
            open_orders: Mutex::new(Some(Err(ExecError::Connection {
                venue: VenueId::Kalshi,
                message: "500".into(),
            }))),
            ..Default::default()
        };
        let mut gate = RiskGate::new(RiskLimits::default());

        let report = reconcile_startup(&venue, &mut gate, &path).await.unwrap();
        std::fs::remove_file(&path).ok();

        assert!(!report.gate_reconciled);
        assert!(!gate.is_reconciled());
    }

    #[tokio::test]
    async fn a_successful_reconciliation_surfaces_the_venues_open_orders() {
        let path = temp_journal_path("open_orders_ok.jsonl");
        std::fs::remove_file(&path).ok();
        let venue = FakeVenue {
            positions: Mutex::new(Some(Ok(Vec::new()))),
            open_orders: Mutex::new(Some(Ok(vec![OrderId("resting-1".into())]))),
            ..Default::default()
        };
        let mut gate = RiskGate::new(RiskLimits::default());

        let report = reconcile_startup(&venue, &mut gate, &path).await.unwrap();
        std::fs::remove_file(&path).ok();

        assert!(report.gate_reconciled);
        assert_eq!(report.open_orders, vec![OrderId("resting-1".into())]);
    }
}
