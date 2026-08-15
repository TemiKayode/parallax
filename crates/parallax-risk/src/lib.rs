//! The risk engine (design doc §10): a single gate every proposed order
//! from every strategy engine must clear, correlated-cluster netting so
//! logically-linked contracts share one exposure budget, and independent
//! kill switches at the global/venue/contract scope.

#![forbid(unsafe_code)]

mod gate;
mod kill_switch;

pub use gate::{RejectReason, RiskGate, RiskLimits};
pub use kill_switch::{KillSwitch, Trip, TripScope};

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
        let gate = RiskGate::new_presumed_flat(RiskLimits::default());
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        assert!(gate.check(&ord, &book, Timestamp::from_nanos(0)).is_ok());
    }

    #[test]
    fn an_unreconciled_gate_refuses_every_order() {
        let contract = spec(869).to_id();
        let gate = RiskGate::new(RiskLimits::default());
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        assert_eq!(
            gate.check(&ord, &book, Timestamp::from_nanos(0)),
            Err(RejectReason::NotReconciled)
        );
    }

    #[test]
    fn mark_reconciled_lets_a_fresh_gate_start_trading() {
        let contract = spec(869).to_id();
        let mut gate = RiskGate::new(RiskLimits::default());
        gate.mark_reconciled();
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        assert!(gate.check(&ord, &book, Timestamp::from_nanos(0)).is_ok());
    }

    #[test]
    fn order_without_market_data_is_rejected() {
        let contract = spec(869).to_id();
        let gate = RiskGate::new_presumed_flat(RiskLimits::default());
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
        let gate = RiskGate::new_presumed_flat(RiskLimits::default());
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        let now = Timestamp::from_nanos(10_000_000_000); // 10s later, limit is 5s
        match gate.check(&ord, &book, now) {
            Err(RejectReason::FeedStale { .. }) => {}
            other => panic!("expected FeedStale, got {other:?}"),
        }
    }

    #[test]
    fn a_feed_timestamp_from_the_future_is_clock_skew_not_freshness() {
        let contract = spec(869).to_id();
        let gate = RiskGate::new_presumed_flat(RiskLimits::default());
        // Tick is stamped 10s ahead of `now` -> age is negative.
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 10_000_000_000);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        match gate.check(&ord, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::ClockSkew { skew_ns }) => assert_eq!(skew_ns, 10_000_000_000),
            other => panic!("expected ClockSkew, got {other:?}"),
        }
    }

    #[test]
    fn per_contract_limit_is_enforced() {
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_abs_qty_per_contract: 50.0,
            ..RiskLimits::default()
        };
        let gate = RiskGate::new_presumed_flat(limits);
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 51.0);
        match gate.check(&ord, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::ContractLimitExceeded { .. }) => {}
            other => panic!("expected ContractLimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn a_two_sided_ladder_is_charged_the_worst_case_not_the_signed_sum() {
        // Quoting 40 up and 40 down nets to a signed sum of zero, but in a
        // fast market only one side fills — the realizable worst case is
        // 40, not 0 (design doc review 1.3). If the gate were (incorrectly)
        // tracking the signed sum, a third order nudging 10 past a 45 cap
        // would look like it was starting from 0 exposure and sail
        // through; tracked correctly, it starts from a worst case of 40
        // and gets rejected.
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_abs_qty_per_contract: 45.0,
            ..RiskLimits::default()
        };
        let mut gate = RiskGate::new_presumed_flat(limits);
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);

        let buy = intent(VenueId::Kalshi, contract.clone(), Side::Buy, 40.0);
        assert!(gate.check(&buy, &book, Timestamp::from_nanos(0)).is_ok());
        gate.reserve(&buy);

        let sell = intent(VenueId::Kalshi, contract.clone(), Side::Sell, 40.0);
        assert!(gate.check(&sell, &book, Timestamp::from_nanos(0)).is_ok());
        gate.reserve(&sell);

        let extra_buy = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        match gate.check(&extra_buy, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::ContractLimitExceeded { projected, .. }) => {
                assert_eq!(
                    projected, 50.0,
                    "the two-sided ladder's worst case (40) plus 10 more must be 50, not 10"
                )
            }
            other => panic!("expected ContractLimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn working_orders_are_visible_to_the_gate_before_any_fill() {
        // Two consecutive ticks each proposing 60 against a 100 cap must
        // not both clear once the first is reserved (design doc review 1.2).
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_abs_qty_per_contract: 100.0,
            ..RiskLimits::default()
        };
        let mut gate = RiskGate::new_presumed_flat(limits);
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);

        let first = intent(VenueId::Kalshi, contract.clone(), Side::Buy, 60.0);
        assert!(gate.check(&first, &book, Timestamp::from_nanos(0)).is_ok());
        gate.reserve(&first);

        let second = intent(VenueId::Kalshi, contract, Side::Buy, 60.0);
        match gate.check(&second, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::ContractLimitExceeded { .. }) => {}
            other => panic!("second order must see the first's reservation, got {other:?}"),
        }
    }

    #[test]
    fn releasing_a_reservation_frees_its_budget_back_up() {
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_abs_qty_per_contract: 60.0,
            ..RiskLimits::default()
        };
        let mut gate = RiskGate::new_presumed_flat(limits);
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);

        let first = intent(VenueId::Kalshi, contract.clone(), Side::Buy, 60.0);
        gate.reserve(&first);
        let second = intent(VenueId::Kalshi, contract.clone(), Side::Buy, 60.0);
        assert!(gate
            .check(&second, &book, Timestamp::from_nanos(0))
            .is_err());

        gate.release(&first);
        assert!(gate.check(&second, &book, Timestamp::from_nanos(0)).is_ok());
    }

    #[test]
    fn an_order_that_reduces_an_already_over_limit_position_is_still_allowed() {
        // Limits exist to bound exposure, not trap it (design doc review 4.5).
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_abs_qty_per_contract: 50.0,
            ..RiskLimits::default()
        };
        let mut gate = RiskGate::new_presumed_flat(limits);
        // Inherited position already over the (lowered) limit.
        gate.record_fill(VenueId::Kalshi, &contract, 80.0, 0.5, 0.0);
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);

        let reduce = intent(VenueId::Kalshi, contract, Side::Sell, 10.0);
        assert!(
            gate.check(&reduce, &book, Timestamp::from_nanos(0)).is_ok(),
            "an order that reduces exposure must not be trapped by the limit that exposure already breached"
        );
    }

    #[test]
    fn an_order_that_would_increase_an_already_over_limit_position_is_still_rejected() {
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_abs_qty_per_contract: 50.0,
            ..RiskLimits::default()
        };
        let mut gate = RiskGate::new_presumed_flat(limits);
        gate.record_fill(VenueId::Kalshi, &contract, 80.0, 0.5, 0.0);
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);

        let increase = intent(VenueId::Kalshi, contract, Side::Buy, 10.0);
        match gate.check(&increase, &book, Timestamp::from_nanos(0)) {
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
        let mut gate = RiskGate::new_presumed_flat(limits);
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
        gate.record_fill(VenueId::Kalshi, &contract_a, 70.0, 0.61, 0.0);

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
    fn opposing_directions_in_one_cluster_net_against_each_other() {
        // A long "temp > 869" and a long "temp < 869" are opposing bets on
        // the same underlying (design doc review 3.24) — they should net,
        // not compound as if they were the same exposure.
        let mut gt = spec(869);
        gt.direction = Direction::GreaterThan;
        let mut lt = spec(869);
        lt.direction = Direction::LessThan;
        let cluster = gt.cluster_key();
        assert_eq!(cluster, lt.cluster_key());

        let limits = RiskLimits {
            max_abs_qty_per_contract: 1_000.0,
            max_abs_qty_per_cluster: 60.0,
            ..RiskLimits::default()
        };
        let mut gate = RiskGate::new_presumed_flat(limits);
        gate.register_contract(gt.to_id(), cluster.clone());
        gate.register_contract(lt.to_id(), cluster.clone());
        gate.record_fill(VenueId::Kalshi, &gt.to_id(), 50.0, 0.5, 0.0);

        let mut book = ConsolidatedBook::new();
        for c in [gt.to_id(), lt.to_id()] {
            book.update(NormalizedTick {
                venue: VenueId::Kalshi,
                contract: c,
                bid: 0.6,
                bid_size: 100.0,
                ask: 0.63,
                ask_size: 100.0,
                venue_ts: None,
                receive_ts: Timestamp::from_nanos(0),
            });
        }

        // Buying 50 of the *opposing* direction should net toward flat at
        // the cluster level and clear easily, even though 50 + 50 = 100
        // would blow through a 60-unit cap if summed without direction.
        let hedge = intent(VenueId::Kalshi, lt.to_id(), Side::Buy, 50.0);
        assert!(
            gate.check(&hedge, &book, Timestamp::from_nanos(0)).is_ok(),
            "opposing-direction exposure should net, not compound"
        );
    }

    #[test]
    fn cluster_worst_case_is_the_range_of_the_sum_not_the_sum_of_worst_cases() {
        // Regression for design doc review 4.1: cluster cap 800, filled
        // -750 in contract B, then 300 up / 299 down quoted in contract A.
        // Both legs clear individually; the realizable worst case is 1049.
        let a = spec(1);
        let mut b = spec(2);
        b.direction = Direction::GreaterThan; // same direction as A, same cluster
        let cluster = a.cluster_key();
        // Force the same cluster explicitly regardless of threshold-derived key.
        let limits = RiskLimits {
            max_abs_qty_per_contract: 10_000.0,
            max_abs_qty_per_cluster: 800.0,
            ..RiskLimits::default()
        };
        let mut gate = RiskGate::new_presumed_flat(limits);
        gate.register_contract(a.to_id(), cluster.clone());
        gate.register_contract(b.to_id(), cluster.clone());
        gate.record_fill(VenueId::Kalshi, &b.to_id(), -750.0, 0.5, 0.0);

        let mut book = ConsolidatedBook::new();
        for c in [a.to_id(), b.to_id()] {
            book.update(NormalizedTick {
                venue: VenueId::Kalshi,
                contract: c,
                bid: 0.6,
                bid_size: 1000.0,
                ask: 0.63,
                ask_size: 1000.0,
                venue_ts: None,
                receive_ts: Timestamp::from_nanos(0),
            });
        }

        let up = intent(VenueId::Kalshi, a.to_id(), Side::Buy, 300.0);
        assert!(gate.check(&up, &book, Timestamp::from_nanos(0)).is_ok());
        gate.reserve(&up);

        let down = intent(VenueId::Kalshi, a.to_id(), Side::Sell, 299.0);
        // Naively this looks safe (contract A's own worst case is only
        // 300), but the cluster's realizable worst case once B's -750
        // fill and A's 300-up leg both land the wrong way is 300 + 750 =
        // 1050 (or symmetrically via the down leg), which must be caught.
        match gate.check(&down, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::ClusterLimitExceeded { .. }) => {}
            other => panic!("expected ClusterLimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn a_price_far_through_the_touch_is_rejected_by_the_collar() {
        let contract = spec(869).to_id();
        let gate = RiskGate::new_presumed_flat(RiskLimits::default());
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0); // ask 0.63
        let mut runaway = intent(VenueId::Kalshi, contract, Side::Buy, 1.0);
        runaway.price = 0.95; // far above the 0.63 ask
        match gate.check(&runaway, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::PriceThroughBook { .. }) => {}
            other => panic!("expected PriceThroughBook, got {other:?}"),
        }
    }

    #[test]
    fn per_order_notional_limit_is_enforced() {
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_notional_per_order: 10.0,
            ..RiskLimits::default()
        };
        let gate = RiskGate::new_presumed_flat(limits);
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        // size 100 @ 0.61 = 61 notional, over the 10 limit.
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 100.0);
        match gate.check(&ord, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::NotionalPerOrderExceeded { .. }) => {}
            other => panic!("expected NotionalPerOrderExceeded, got {other:?}"),
        }
    }

    #[test]
    fn a_cheap_short_is_charged_the_same_notional_as_a_symmetric_buy() {
        // The same 49x error the notional limits exist to prevent,
        // mirrored onto the short side (design doc review 4.4).
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_notional_per_order: 20.0,
            ..RiskLimits::default()
        };
        let gate = RiskGate::new_presumed_flat(limits);
        let mut book = ConsolidatedBook::new();
        book.update(NormalizedTick {
            venue: VenueId::Kalshi,
            contract: contract.clone(),
            bid: 0.02,
            bid_size: 1000.0,
            ask: 0.03,
            ask_size: 1000.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        });
        let mut cheap_short = intent(VenueId::Kalshi, contract, Side::Sell, 500.0);
        cheap_short.price = 0.02; // 500 * (1 - 0.02) = 490 at risk, not 10
        match gate.check(&cheap_short, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::NotionalPerOrderExceeded { .. }) => {}
            other => panic!("expected NotionalPerOrderExceeded, got {other:?}"),
        }
    }

    #[test]
    fn global_kill_switch_rejects_everything() {
        let contract = spec(869).to_id();
        let mut gate = RiskGate::new_presumed_flat(RiskLimits::default());
        gate.trip_global("feed dropout");
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 1.0);
        match gate.check(&ord, &book, Timestamp::from_nanos(0)) {
            Err(RejectReason::KillSwitch { .. }) => {}
            other => panic!("expected KillSwitch, got {other:?}"),
        }
    }

    #[test]
    fn mark_to_market_trips_the_kill_switch_past_the_session_loss_budget() {
        let limits = RiskLimits {
            max_session_loss: 100.0,
            ..RiskLimits::default()
        };
        let mut gate = RiskGate::new_presumed_flat(limits);
        gate.mark_to_market(1_000.0); // establishes the starting equity
        assert!(!gate.kill_switch().is_global_tripped());
        gate.mark_to_market(850.0); // 150 loss, over the 100 budget
        assert!(gate.kill_switch().is_global_tripped());
    }

    #[test]
    fn mark_to_market_trips_the_kill_switch_on_non_finite_equity() {
        let mut gate = RiskGate::new_presumed_flat(RiskLimits::default());
        gate.mark_to_market(1_000.0);
        gate.mark_to_market(f64::NAN);
        assert!(gate.kill_switch().is_global_tripped());
    }

    #[test]
    fn batch_check_prevents_two_engines_from_jointly_exceeding_a_limit() {
        let contract = spec(869).to_id();
        let limits = RiskLimits {
            max_abs_qty_per_contract: 100.0,
            ..RiskLimits::default()
        };
        let gate = RiskGate::new_presumed_flat(limits);
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
        let mut gate = RiskGate::new_presumed_flat(RiskLimits::default());
        gate.trip_venue(VenueId::Polymarket, "error rate spike");

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

    #[test]
    fn operator_reset_clears_a_trip_that_check_would_otherwise_still_see() {
        let contract = spec(869).to_id();
        let mut gate = RiskGate::new_presumed_flat(RiskLimits::default());
        let book = book_with_tick(VenueId::Kalshi, contract.clone(), 0);
        let ord = intent(VenueId::Kalshi, contract, Side::Buy, 1.0);

        gate.trip_global("test fault");
        assert!(gate.check(&ord, &book, Timestamp::from_nanos(0)).is_err());

        gate.operator_reset_kill_switches();
        assert!(gate.check(&ord, &book, Timestamp::from_nanos(0)).is_ok());
    }
}
