//! The risk engine (design doc §10): a single gate every proposed order
//! from every strategy engine must clear, correlated-cluster netting so
//! logically-linked contracts share one exposure budget, and independent
//! kill switches at the global/venue/contract scope.

mod gate;
mod kill_switch;

pub use gate::{RejectReason, RiskGate, RiskLimits};
pub use kill_switch::KillSwitch;

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_book::ConsolidatedBook;
    use parallax_types::{
        CanonicalContractSpec, Direction, EngineId, EventClass, NormalizedTick, OrderIntent,
        OrderType, Outcome, Side, Timestamp, VenueId,
    };

    fn spec(threshold: i64) -> CanonicalContractSpec {
        CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        }
    }

    fn book_with_tick(
        venue: VenueId,
        contract: parallax_types::CanonicalContractId,
        ts: i64,
    ) -> ConsolidatedBook {
        let mut book = ConsolidatedBook::new();
        book.update(NormalizedTick {
            venue,
            contract,
            bid: 0.60,
            bid_size: 100.0,
            ask: 0.63,
            ask_size: 100.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(ts),
        });
        book
    }

    fn intent(
        venue: VenueId,
        contract: parallax_types::CanonicalContractId,
        side: Side,
        size: f64,
    ) -> OrderIntent {
        OrderIntent {
            venue,
            contract,
            outcome: Outcome::Yes,
            side,
            price: 0.61,
            size,
            order_type: OrderType::Limit,
            engine: EngineId::MarketMaking,
            created_at: Timestamp::from_nanos(0),
        }
    }

    #[test]
    fn order_within_limits_is_accepted() {
        let contract = spec(869).to_id();
        let gate = RiskGate::new(RiskLimits::default());
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        assert!(gate.check(&ord, &book, Timestamp::from_nanos(0)).is_ok());
    }

    #[test]
    fn order_without_market_data_is_rejected() {
        let contract = spec(869).to_id();
        let gate = RiskGate::new(RiskLimits::default());
        let book = ConsolidatedBook::new();
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        assert_eq!(
            gate.check(&ord, &book, Timestamp::from_nanos(0)),
            Err(RejectReason::NoMarketData)
        );
    }

    #[test]
    fn stale_feed_is_rejected() {
        let contract = spec(869).to_id();
        let gate = RiskGate::new(RiskLimits::default());
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        let now = Timestamp::from_nanos(10_000_000_000); // 10s later, limit is 5s
        match gate.check(&ord, &book, now) {
            Err(RejectReason::FeedStale { .. }) => {}
            other => panic!("expected FeedStale, got {other:?}"),
        }
    }

    #[test]
    fn per_contract_limit_is_enforced() {
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_abs_qty_per_contract: 50.0,
            ..RiskLimits::default()
        };
        let gate = RiskGate::new(limits);
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 51.0);
        match gate.check(&ord, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::ContractLimitExceeded { .. }) => {}
            other => panic!("expected ContractLimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn correlated_cluster_is_netted_across_contracts() {
        // Two different thresholds, same city/date -> same cluster.
        let contract_a = spec(869).to_id();
        let contract_b = spec(900).to_id();
        let cluster = spec(869).cluster_key();
        assert_eq!(cluster, spec(900).cluster_key());

        let limits = RiskLimits {
            max_abs_qty_per_contract: 1_000.0, // high enough that only the cluster limit binds
            max_abs_qty_per_cluster: 100.0,
            ..RiskLimits::default()
        };
        let mut gate = RiskGate::new(limits);
        gate.register_contract(contract_a.clone(), cluster.clone());
        gate.register_contract(contract_b.clone(), cluster.clone());

        let mut book = ConsolidatedBook::new();
        for c in [&contract_a, &contract_b] {
            book.update(NormalizedTick {
                venue: VenueId::Kalshi,
                contract: c.clone(),
                bid: 0.6,
                bid_size: 100.0,
                ask: 0.63,
                ask_size: 100.0,
                venue_ts: None,
                receive_ts: Timestamp::from_nanos(0),
            });
        }

        // First order fills 70 of the 100-unit cluster budget on contract A.
        gate.record_fill(VenueId::Kalshi, &contract_a, 70.0, 0.61);

        // A second order on the *different* contract B, well within B's own
        // per-contract limit, should still be rejected because the two
        // together would exceed the shared cluster budget.
        let ord_b = intent(VenueId::Kalshi, contract_b, Side::Buy, 40.0);
        match gate.check(&ord_b, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::ClusterLimitExceeded { .. }) => {}
            other => panic!("expected ClusterLimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn global_kill_switch_rejects_everything() {
        let contract = spec(869).to_id();
        let mut gate = RiskGate::new(RiskLimits::default());
        gate.kill_switch_mut().trip_global("feed dropout");
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 1.0);
        match gate.check(&ord, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::KillSwitch { .. }) => {}
            other => panic!("expected KillSwitch, got {other:?}"),
        }
    }

    #[test]
    fn batch_check_prevents_two_engines_from_jointly_exceeding_a_limit() {
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_abs_qty_per_contract: 100.0,
            ..RiskLimits::default()
        };
        let gate = RiskGate::new(limits);
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);

        // Market making proposes 60, stat-arb independently proposes 60 —
        // each is fine alone against the pre-tick position of 0, but
        // together they would take the contract to 120 against a 100 cap.
        let mm_order = intent(VenueId::Kalshi, contract.clone(), Side::Buy, 60.0);
        let mut statarb_order = intent(VenueId::Kalshi, contract, Side::Buy, 60.0);
        statarb_order.engine = EngineId::StatArb;

        let results = gate.check_batch(&[mm_order, statarb_order], &book, Timestamp::from_nanos(0));
        assert!(
            results[0].is_ok(),
            "first order should clear: {:?}",
            results[0]
        );
        match &results[1] {
            Err(RejectReason::ContractLimitExceeded { .. }) => {}
            other => panic!("expected the second order to be rejected once batched, got {other:?}"),
        }
    }

    #[test]
    fn venue_kill_switch_does_not_affect_other_venues() {
        let contract = spec(869).to_id();
        let mut gate = RiskGate::new(RiskLimits::default());
        gate.kill_switch_mut()
            .trip_venue(VenueId::Polymarket, "error rate spike");

        let book_kalshi = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord_kalshi = intent(VenueId::Kalshi, contract.clone(), Side::Buy, 1.0);
        assert!(gate
            .check(&ord_kalshi, &book_kalshi, Timestamp::from_nanos(0))
            .is_ok());

        let book_poly = book_with_tick(VenueId::Polymarket, contract.clone(), 0);
        let ord_poly = intent(VenueId::Polymarket, contract, Side::Buy, 1.0);
        assert!(gate
            .check(&ord_poly, &book_poly, Timestamp::from_nanos(0))
            .is_err());
    }
}
