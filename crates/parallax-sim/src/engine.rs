use crate::replay::ReplayEvent;
use crate::report::BacktestReport;
use parallax_alpha::{aggregate, AggregatorConfig, AlphaSource};
use parallax_book::ConsolidatedBook;
use parallax_risk::{RiskGate, RiskLimits};
use parallax_strategy::{Calibrator, StrategyCore, StrategyEngine, StrategyInput};
use parallax_types::{
    AckStatus, CanonicalContractId, ClusterKey, OrderId, OrderIntent, ProbabilityEstimate,
    Timestamp,
};
use parallax_venues::{PaperAdapter, VenueAdapter};
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
    pub fn new(
        risk_limits: RiskLimits,
        alpha_sources: Vec<Box<dyn AlphaSource>>,
        engines: Vec<Box<dyn StrategyEngine + Send + Sync>>,
    ) -> Self {
        Backtest {
            book: ConsolidatedBook::new(),
            risk: RiskGate::new(risk_limits),
            strategy: StrategyCore::new(engines),
            calibrator: Calibrator::default(),
            venue: Arc::new(PaperAdapter::new()),
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

        let fills = self.venue.advance_market(
            contract.clone(),
            tick.bid,
            tick.bid_size,
            tick.ask,
            tick.ask_size,
            now,
        );
        for ack in fills {
            self.apply_ack(&ack);
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
                Err(_reason) => self.report.orders_rejected_by_risk += 1,
                Ok(()) => self.submit_and_track(intent).await,
            }
        }
    }

    async fn submit_and_track(&mut self, intent: OrderIntent) {
        match self.venue.submit(intent.clone()).await {
            Err(_) => self.report.orders_failed_submission += 1,
            Ok(ack) => {
                self.outstanding.insert(ack.order_id.clone(), intent);
                self.apply_ack(&ack);
            }
        }
    }

    fn apply_ack(&mut self, ack: &parallax_types::OrderAck) {
        let Some(intent) = self.outstanding.get(&ack.order_id).cloned() else {
            return;
        };

        match &ack.status {
            AckStatus::Filled { qty, price } => {
                self.record_fill(&intent, *qty, *price);
                self.outstanding.remove(&ack.order_id);
            }
            AckStatus::PartiallyFilled {
                filled_qty, price, ..
            } => {
                self.record_fill(&intent, *filled_qty, *price);
                // still resting at the venue under the same order id
            }
            AckStatus::Rejected { .. } => {
                self.calibrator.record_outcome(intent.venue, false, None);
                self.outstanding.remove(&ack.order_id);
            }
            AckStatus::Canceled => {
                self.outstanding.remove(&ack.order_id);
            }
            AckStatus::Accepted => {}
        }
    }

    fn record_fill(&mut self, intent: &OrderIntent, qty: f64, price: f64) {
        let signed = match intent.side {
            parallax_types::Side::Buy => qty,
            parallax_types::Side::Sell => -qty,
        };
        self.risk
            .record_fill(intent.venue, &intent.contract, signed, price);
        let slippage = (price - intent.price).abs();
        self.calibrator
            .record_outcome(intent.venue, true, Some(slippage));
        self.report.fills += 1;
        self.report.filled_volume += qty;
    }

    fn finalize_report(&mut self) -> BacktestReport {
        let mut report = std::mem::take(&mut self.report);
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

        let net = backtest.risk.position_qty(VenueId::Polymarket, &contract());
        assert!(
            net > 0.0,
            "expected a net long position after buying the cheap ask, got {net}"
        );
    }
}
