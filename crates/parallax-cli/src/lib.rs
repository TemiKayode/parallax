//! The synthetic demo scenario, factored out of the `parallax-demo`
//! binary so `parallax-ui` can drive the exact same scenario through the
//! real engine via its web dashboard instead of duplicating this logic.
//! No live venue connection, no credentials — everything here runs
//! against `ConsolidatedBook` and the in-memory `PaperAdapter` only.

#![forbid(unsafe_code)]

use parallax_alpha::WeatherEnsembleSource;
use parallax_book::{ConsolidatedBook, CrossVenueArb};
use parallax_risk::RiskLimits;
use parallax_sim::{Backtest, BacktestReport, ReplayEvent};
use parallax_strategy::{
    LiquiditySnipingEngine, MarketMakingConfig, MarketMakingEngine, SnipingConfig, StatArbConfig,
    StatArbEngine,
};
use parallax_types::{
    AlphaEventKind, CanonicalContractId, CanonicalContractSpec, Direction, EventClass, FeeModel,
    NormalizedTick, RawEvent, Timestamp, VenueId,
};
use parallax_venues::PaperConfig;

pub fn demo_contract_spec() -> CanonicalContractSpec {
    CanonicalContractSpec {
        event_class: EventClass("wx.temp".into()),
        location: "chicago".into(),
        threshold: 869, // 86.9°F, tenths
        direction: Direction::GreaterThan,
        resolution_window: "2026-08-12".into(),
        resolution_source: "nws_official".into(),
    }
}

pub fn demo_contract_id() -> CanonicalContractId {
    demo_contract_spec().to_id()
}

/// Builds a two-venue consolidated book from caller-supplied quotes and
/// runs the real arb detector (`parallax-book::detect_arb`) against it —
/// no model, just the same-contract cross-venue mispricing check from
/// design doc §6. `polymarket_size`/`kalshi_size` apply symmetrically to
/// both sides of each venue's book, so `executable_size` on the result
/// genuinely reflects the depth the caller passed in rather than a fixed
/// backend constant.
pub fn sample_arb(
    polymarket_bid: f64,
    polymarket_ask: f64,
    polymarket_size: f64,
    kalshi_bid: f64,
    kalshi_ask: f64,
    kalshi_size: f64,
) -> Option<CrossVenueArb> {
    let contract = demo_contract_id();
    let mut book = ConsolidatedBook::new();
    book.update(NormalizedTick {
        venue: VenueId::Polymarket,
        contract: contract.clone(),
        bid: polymarket_bid,
        bid_size: polymarket_size,
        ask: polymarket_ask,
        ask_size: polymarket_size,
        venue_ts: None,
        receive_ts: Timestamp::from_nanos(0),
    });
    book.update(NormalizedTick {
        venue: VenueId::Kalshi,
        contract: contract.clone(),
        bid: kalshi_bid,
        bid_size: kalshi_size,
        ask: kalshi_ask,
        ask_size: kalshi_size,
        venue_ts: None,
        receive_ts: Timestamp::from_nanos(0),
    });
    book.detect_arb(&contract)
}

/// Runs the full pipeline end to end against synthetic data: a bullish
/// weather ensemble update, a stale cheap Polymarket quote, and a Kalshi
/// quote that's already repriced — through the exact same
/// `parallax-book` / `parallax-alpha` / `parallax-risk` / `parallax-strategy`
/// types a live deployment would use, per design doc §15.
pub async fn run_demo_backtest() -> BacktestReport {
    let contract = demo_contract_id();
    // The scenario's one fill takes the stale Polymarket ask (see
    // `stale_quote` below) via IOC, so Polymarket's real published taker
    // schedule is the honest fee to charge it — a zero-fee `PaperConfig`
    // default would report an idealization instead of a result (see
    // `docs/GOING-LIVE.md`, Stage 0). Latency and queue-ahead-fraction stay
    // at their frictionless defaults here: this scenario's event
    // timestamps are hand-tuned to nanosecond precision to land the snipe
    // exactly once the fair value postdates the stale quote, and enabling
    // execution latency changes that timing — a fee is pure post-fill
    // arithmetic and doesn't.
    let venue_config = PaperConfig {
        fee_model: FeeModel::polymarket_default(),
        ..PaperConfig::default()
    };
    let mut backtest = Backtest::new(
        RiskLimits::default(),
        venue_config,
        vec![Box::new(WeatherEnsembleSource::new("hrrr-ensemble"))],
        vec![
            Box::new(LiquiditySnipingEngine::new(SnipingConfig::default())),
            Box::new(StatArbEngine::new(StatArbConfig::default())),
            Box::new(MarketMakingEngine::new(MarketMakingConfig::default())),
        ],
    );
    backtest.register_contract(contract.clone(), demo_contract_spec().cluster_key());

    let weather_update = RawEvent {
        source: "hrrr-ensemble".into(),
        kind: AlphaEventKind::Weather,
        publish_ts: None,
        receive_ts: Timestamp::from_nanos(0),
        payload: serde_json::json!({
            "contract": contract.0,
            "threshold_tenths": 869,
            "ensemble_forecast_tenths": [920, 930, 915, 925, 918],
        }),
    };

    let stale_quote = NormalizedTick {
        venue: VenueId::Polymarket,
        contract: contract.clone(),
        bid: 0.50,
        bid_size: 50.0,
        ask: 0.55,
        ask_size: 50.0,
        venue_ts: None,
        receive_ts: Timestamp::from_nanos(1_000_000),
    };

    let kalshi_quote = NormalizedTick {
        venue: VenueId::Kalshi,
        contract: contract.clone(),
        bid: 0.90,
        bid_size: 50.0,
        ask: 0.94,
        ask_size: 50.0,
        venue_ts: None,
        receive_ts: Timestamp::from_nanos(2_000_000),
    };

    backtest
        .run(vec![
            ReplayEvent::Alpha(weather_update),
            ReplayEvent::Tick(stale_quote),
            ReplayEvent::Tick(kalshi_quote),
        ])
        .await
}
