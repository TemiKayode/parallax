use crate::replay::ReplayEvent;
use crate::report::BacktestReport;
use parallax_alpha::{aggregate, AggregatorConfig, AlphaSource};
use parallax_book::ConsolidatedBook;
use parallax_risk::{RejectReason, RiskGate, RiskLimits};
use parallax_strategy::{Calibrator, FillOutcome, StrategyCore, StrategyEngine, StrategyInput};
use parallax_types::{
    AckStatus, CanonicalContractId, ClusterKey, OrderId, OrderIntent, ProbabilityEstimate,
    Timestamp,
};
use parallax_venues::{PaperAdapter, PaperConfig, VenueAdapter};
use std::collections::HashMap;
use std::sync::Arc;

/// Drives the full pipeline in design doc §4's architecture diagram
/// against replayed data, using the exact same `parallax-book`,
/// `parallax-risk`, and `parallax-strategy` types the live binary would
/// use — only the venue adapter (`PaperAdapter`) and the data source
/// (a replay file instead of a live feed) differ, which is what makes
/// backtest/live parity (design doc §15) more than an aspiration.
pub struct Backtest {
    book: ConsolidatedBook,
    risk: RiskGate,
    strategy: StrategyCore,
    calibrator: Calibrator,
    venue: Arc<PaperAdapter>,
    alpha_sources: Vec<Box<dyn AlphaSource>>,
    aggregator_config: AggregatorConfig,
    /// Latest estimate per (contract, source name) — the working set the
    /// aggregator recombines every time any one of them changes.
    estimates: HashMap<CanonicalContractId, HashMap<String, ProbabilityEstimate>>,
    /// Orders currently live at the venue, keyed by the id the venue
    /// assigned, so a later fill against a resting order can be traced
    /// back to which engine/side/intended-price it came from.
    outstanding: HashMap<OrderId, OrderIntent>,
    report: BacktestReport,
}

impl Backtest {
    /// `venue_config` is not optional because it is not safe to default:
    /// `PaperConfig::default()` is zero-fee/zero-latency/front-of-queue —
    /// the most flattering matching engine a backtest can run against, and
    /// exactly right for a unit test asserting exact fill mechanics, but
    /// silently wrong for anyone reading the resulting P&L as evidence a
    /// strategy has edge. Pass `PaperConfig { fee_model:
    /// FeeModel::kalshi_default()` (or `polymarket_default()`), .. }`
    /// explicitly for anything meant to measure a strategy rather than
    /// exercise the pipeline (see `docs/GOING-LIVE.md`, Stage 0).
    pub fn new(
        risk_limits: RiskLimits,
        venue_config: PaperConfig,
        alpha_sources: Vec<Box<dyn AlphaSource>>,
        engines: Vec<Box<dyn StrategyEngine + Send + Sync>>,
    ) -> Self {
        Backtest {
            book: ConsolidatedBook::new(),
            // Flat really is the starting truth for a backtest — there is
            // no real venue position to reconcile against (design doc
            // review 1.8).
            risk: RiskGate::new_presumed_flat(risk_limits),
            strategy: StrategyCore::new(engines),
            calibrator: Calibrator::default(),
            venue: Arc::new(PaperAdapter::with_config(venue_config)),
            alpha_sources,
            aggregator_config: AggregatorConfig::default(),
            estimates: HashMap::new(),
            outstanding: HashMap::new(),
            report: BacktestReport::default(),
        }
    }

    /// Registers a contract's correlated-risk cluster with the risk gate
    /// (design doc §10) before replay starts.
    pub fn register_contract(&mut self, contract: CanonicalContractId, cluster: ClusterKey) {
        self.risk.register_contract(contract, cluster);
    }

    pub async fn run(&mut self, events: Vec<ReplayEvent>) -> BacktestReport {
        for event in events {
            match event {
                ReplayEvent::Tick(tick) => self.handle_tick(tick).await,
                ReplayEvent::Alpha(raw) => self.handle_alpha(raw).await,
            }
        }
        self.finalize_report()
    }

    async fn handle_tick(&mut self, tick: parallax_types::NormalizedTick) {
        self.report.ticks_processed += 1;
        let contract = tick.contract.clone();
        let now = tick.receive_ts;
        self.book.update(tick.clone());
        // A rising `rejected_ticks()` means the feed's shape changed —
        // malformed data that used to parse, or a source that started
        // sending invalid records (design doc review 1.1). This used to
        // be an empty `if` with a comment claiming the count was
        // "surfaced via the report" when nothing did; it's actually
        // surfaced now, by `alerting::check_feed_data_quality(&self.book)`
        // — docs/GOING-LIVE.md Stage 3 — which a caller runs against the
        // book after a replay/backtest completes.

        let fills = self.venue.advance_market(
            contract.clone(),
            tick.bid,
            tick.bid_size,
            tick.ask,
            tick.ask_size,
            now,
        );
        for ack in fills {
            // Every fill reaching here came from a *resting* order being
            // hit by this tick — a maker fill, by construction.
            self.apply_ack(&ack, true);
        }

        self.recompute_and_trade(&contract, now).await;
    }

    async fn handle_alpha(&mut self, event: parallax_types::RawEvent) {
        self.report.alpha_events_processed += 1;
        let now = event.receive_ts;

        // Collect which contracts got a fresh estimate this event so we
        // only re-run strategy evaluation for contracts that actually
        // changed, not the entire universe.
        let mut touched: Vec<CanonicalContractId> = Vec::new();

        for source in &self.alpha_sources {
            if !source.event_kinds().contains(&event.kind) {
                continue;
            }
            if let Some(estimate) = source.on_event(&event) {
                if estimate.validate().is_err() {
                    continue;
                }
                let contract = estimate.contract.clone();
                self.estimates
                    .entry(contract.clone())
                    .or_default()
                    .insert(source.name().to_string(), estimate);
                touched.push(contract);
            }
        }

        for contract in touched {
            self.recompute_and_trade(&contract, now).await;
        }
    }

    async fn recompute_and_trade(&mut self, contract: &CanonicalContractId, now: Timestamp) {
        let Some(source_estimates) = self.estimates.get(contract) else {
            return;
        };
        let estimates: Vec<ProbabilityEstimate> = source_estimates.values().cloned().collect();
        let Some(fair_value) = aggregate(contract, &estimates, &self.aggregator_config, now) else {
            return;
        };
        if fair_value.validate().is_err() {
            return;
        }

        let inventory = self.risk.inventory_for(contract);
        let proposals = {
            let input = StrategyInput {
                contract,
                fair_value: &fair_value,
                book: &self.book,
                inventory: &inventory,
                calibration: &self.calibrator,
                now,
            };
            self.strategy.evaluate_all(&input)
        };
        self.report.orders_proposed += proposals.len() as u64;

        let results = self.risk.check_batch(&proposals, &self.book, now);
        for (intent, result) in proposals.into_iter().zip(results) {
            match result {
                Err(reason) => {
                    // docs/GOING-LIVE.md Stage 3: "Rule 5 fired because
                    // the pair cost 96.2 cents is a debuggable sentence
                    // at 3am; a fill with no recorded reason is an
                    // argument." `reason`'s Debug output carries every
                    // field the specific rule rejected on (the touch
                    // price, the projected notional, whichever limit —
                    // see RejectReason), not just its variant name.
                    tracing::info!(
                        engine = ?intent.engine,
                        venue = ?intent.venue,
                        contract = %intent.contract.0,
                        side = ?intent.side,
                        price = intent.price,
                        size = intent.size,
                        reason = ?reason,
                        "order rejected by risk gate"
                    );
                    self.report.record_rejection(reject_reason_label(&reason));
                }
                Ok(()) => {
                    tracing::debug!(
                        engine = ?intent.engine,
                        venue = ?intent.venue,
                        contract = %intent.contract.0,
                        side = ?intent.side,
                        price = intent.price,
                        size = intent.size,
                        "order accepted by risk gate"
                    );
                    // Reserved immediately, before submission — a working
                    // order is real exposure the instant it's live, and
                    // the next proposal in this same batch must see it
                    // (design doc review 1.2).
                    self.risk.reserve(&intent);
                    self.submit_and_track(intent).await;
                }
            }
        }
    }

    async fn submit_and_track(&mut self, intent: OrderIntent) {
        match self.venue.submit(intent.clone()).await {
            Err(_) => {
                self.report.orders_failed_submission += 1;
                self.calibrator
                    .record_outcome(intent.venue, FillOutcome::Rejected, None);
                // Submission itself failed: nothing is working at the
                // venue, so the reservation taken above must come back off.
                self.risk.release(&intent);
            }
            Ok(ack) => {
                self.outstanding.insert(ack.order_id.clone(), intent);
                // The ack from `submit` itself, if it fills anything,
                // reflects an aggressive/immediate cross — a taker fill.
                self.apply_ack(&ack, false);
            }
        }
    }

    fn apply_ack(&mut self, ack: &parallax_types::OrderAck, is_maker: bool) {
        let Some(intent) = self.outstanding.get(&ack.order_id).cloned() else {
            return;
        };

        match &ack.status {
            AckStatus::Filled { qty, price } => {
                self.record_fill(&intent, *qty, *price, is_maker);
                self.risk.reduce_reservation(&intent, *qty);
                self.outstanding.remove(&ack.order_id);
            }
            AckStatus::PartiallyFilled {
                filled_qty, price, ..
            } => {
                self.record_fill(&intent, *filled_qty, *price, is_maker);
                self.risk.reduce_reservation(&intent, *filled_qty);
                // still resting at the venue under the same order id
            }
            AckStatus::Rejected { .. } => {
                // An accepted order that found no crossing liquidity — a
                // genuine illiquidity signal, kept distinct from a
                // submission-level rejection (design doc review 2.15).
                self.calibrator
                    .record_outcome(intent.venue, FillOutcome::Unfilled, None);
                self.risk.release(&intent);
                self.outstanding.remove(&ack.order_id);
            }
            AckStatus::Canceled => {
                self.risk.release(&intent);
                self.outstanding.remove(&ack.order_id);
            }
            AckStatus::Accepted => {}
        }
    }

    fn record_fill(&mut self, intent: &OrderIntent, qty: f64, price: f64, is_maker: bool) {
        if qty <= 0.0 {
            // A cancellation or a rejection must never be counted as a
            // trade (design doc review 4.7).
            return;
        }
        let signed = match intent.side {
            parallax_types::Side::Buy => qty,
            parallax_types::Side::Sell => -qty,
        };
        let fee = self
            .venue
            .capabilities()
            .fee_model
            .fee(is_maker, qty, price);
        let realized = self
            .risk
            .record_fill(intent.venue, &intent.contract, signed, price, fee);

        // Signed adverse slippage: positive means the fill cost us
        // relative to what we quoted, on either side — an unsigned
        // `.abs()` made a venue that consistently improves our price look
        // exactly as costly as one that consistently worsens it (design
        // doc review 2.15).
        let slippage = match intent.side {
            parallax_types::Side::Buy => price - intent.price,
            parallax_types::Side::Sell => intent.price - price,
        };
        self.calibrator
            .record_outcome(intent.venue, FillOutcome::Filled, Some(slippage));

        self.report.fills += 1;
        self.report.filled_volume += qty;
        self.report.gross_notional_traded += qty * price;
        self.report.realized_pnl += realized;
        self.report.fees_paid += fee;

        let equity = self.current_equity();
        self.risk.mark_to_market(equity);
        self.report.record_equity(equity);
    }

    /// Realized P&L plus mark-to-model unrealized P&L on every open
    /// position, minus fees paid so far — the number `mark_to_market`
    /// and the equity curve are built from.
    fn current_equity(&self) -> f64 {
        let mut unrealized = 0.0;
        for (_, contract, position) in self.risk.positions_snapshot() {
            if position.is_flat() {
                continue;
            }
            if let Some(mid) = self.book.consolidated_mid(&contract) {
                unrealized += position.unrealized_pnl(mid);
            }
        }
        self.report.realized_pnl - self.report.fees_paid + unrealized
    }

    fn finalize_report(&mut self) -> BacktestReport {
        let mut report = std::mem::take(&mut self.report);
        report.unrealized_pnl = 0.0;
        for (venue, contract, position) in self.risk.positions_snapshot() {
            if position.is_flat() {
                continue;
            }
            if let Some(mid) = self.book.consolidated_mid(&contract) {
                report.unrealized_pnl += position.unrealized_pnl(mid);
            }
            report
                .open_positions
                .push((venue, contract, position.qty, position.avg_price));
        }
        report
    }
}

fn reject_reason_label(reason: &RejectReason) -> String {
    // A stable, coarse-grained label per variant — enough for the
    // rejection histogram to answer "why did trading stop" without
    // needing every numeric field to match for aggregation to work.
    match reason {
        RejectReason::NotReconciled => "NotReconciled".to_string(),
        RejectReason::KillSwitch { .. } => "KillSwitch".to_string(),
        RejectReason::NoMarketData => "NoMarketData".to_string(),
        RejectReason::FeedStale { .. } => "FeedStale".to_string(),
        RejectReason::ClockSkew { .. } => "ClockSkew".to_string(),
        RejectReason::PriceThroughBook { .. } => "PriceThroughBook".to_string(),
        RejectReason::Invalid(_) => "Invalid".to_string(),
        RejectReason::ContractLimitExceeded { .. } => "ContractLimitExceeded".to_string(),
        RejectReason::ClusterLimitExceeded { .. } => "ClusterLimitExceeded".to_string(),
        RejectReason::VenueLimitExceeded { .. } => "VenueLimitExceeded".to_string(),
        RejectReason::GrossLimitExceeded { .. } => "GrossLimitExceeded".to_string(),
        RejectReason::NotionalPerOrderExceeded { .. } => "NotionalPerOrderExceeded".to_string(),
        RejectReason::NotionalVenueExceeded { .. } => "NotionalVenueExceeded".to_string(),
        RejectReason::NotionalTotalExceeded { .. } => "NotionalTotalExceeded".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::ReplayEvent;
    use parallax_alpha::WeatherEnsembleSource;
    use parallax_strategy::{LiquiditySnipingEngine, SnipingConfig, StatArbConfig, StatArbEngine};
    use parallax_types::{
        AlphaEventKind, CanonicalContractSpec, Direction, EventClass, NormalizedTick, RawEvent,
        VenueId,
    };

    fn contract() -> CanonicalContractId {
        CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: 869,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        }
        .to_id()
    }

    /// End-to-end: a weather ensemble strongly favors YES, then a venue
    /// quotes a stale, far-too-cheap ask for that same canonical
    /// contract. The strategy core should propose a buy, the risk gate
    /// should accept it, the paper venue should fill it, and the
    /// resulting position/PnL should show up in the final report — the
    /// full pipeline from design doc §4 exercised end to end against the
    /// exact production types, only the venue and data source swapped.
    #[tokio::test]
    async fn stale_cheap_quote_after_bullish_weather_update_gets_bought_and_reported() {
        let mut backtest = Backtest::new(
            RiskLimits::default(),
            PaperConfig::default(),
            vec![Box::new(WeatherEnsembleSource::new("hrrr"))],
            vec![
                Box::new(LiquiditySnipingEngine::new(SnipingConfig::default())),
                Box::new(StatArbEngine::new(StatArbConfig::default())),
            ],
        );
        backtest.register_contract(
            contract(),
            CanonicalContractSpec {
                event_class: EventClass("wx.temp".into()),
                location: "chicago".into(),
                threshold: 869,
                direction: Direction::GreaterThan,
                resolution_window: "2026-08-12".into(),
                resolution_source: "nws_official".into(),
            }
            .cluster_key(),
        );

        let weather_update = RawEvent {
            source: "hrrr".into(),
            kind: AlphaEventKind::Weather,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: serde_json::json!({
                "contract": contract().0,
                "threshold_tenths": 869,
                "direction": "gt",
                "ensemble_forecast_tenths": [920, 930, 915, 925, 918],
            }),
        };

        let stale_quote = NormalizedTick {
            venue: VenueId::Polymarket,
            contract: contract(),
            bid: 0.50,
            bid_size: 50.0,
            ask: 0.55,
            ask_size: 50.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(1),
        };

        let events = vec![
            ReplayEvent::Alpha(weather_update),
            ReplayEvent::Tick(stale_quote),
        ];
        let report = backtest.run(events).await;

        assert_eq!(report.ticks_processed, 1);
        assert_eq!(report.alpha_events_processed, 1);
        assert!(
            report.orders_proposed > 0,
            "expected at least one order proposal"
        );
        assert!(report.fills > 0, "expected the mispriced ask to get bought");
        assert!(report.gross_notional_traded > 0.0);

        let net = backtest.risk.position_qty(VenueId::Polymarket, &contract());
        assert!(
            net > 0.0,
            "expected a net long position after buying the cheap ask, got {net}"
        );
    }
}
