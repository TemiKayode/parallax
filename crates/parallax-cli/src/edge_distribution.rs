//! Stage 0 of `docs/GOING-LIVE.md`: "report the distribution, not a
//! number... run many seeds... judge on the 10th percentile." A single
//! deterministic backtest (`run_demo_backtest`) proves the pipeline wires
//! together; it cannot prove a strategy has edge, because one draw of
//! forecast/quote noise is not evidence about the distribution those
//! numbers are drawn from.
//!
//! This runs the same weather-update → stale-quote → fill scenario many
//! times, perturbing per seed exactly the things a real deployment cannot
//! pin down in advance — the ensemble forecast, both venues' quoted
//! prices, and queue position — and reports the resulting P&L
//! distribution rather than one number. It is still synthetic data, not
//! the recorded venue book data Stage 0 ultimately calls for; see the
//! module-level caveat on `run_edge_distribution`.
//!
//! Deliberately not perturbed: execution latency. `PaperAdapter` only
//! resolves a latency-delayed order on the *next* `advance_market` call
//! (see `activate_pending`), and this scenario has exactly one tick after
//! the one a delayed order would react to — so any nonzero latency here
//! would resolve against the *already-repriced* book and always miss,
//! which isn't modeling latency, it's a scenario-specific artifact. An
//! earlier version of this harness added a second, identical tick to give
//! a delayed order a fair chance to land — and discovered empirically
//! that the strategy pipeline re-proposes and re-fills against a tick
//! that carries no new information (fills went 1 -> 2, orders_proposed
//! 16 -> 24, for the *same* market state broadcast twice). That is a real
//! finding worth its own investigation before latency is safe to model
//! here — see the idempotency discussion in `docs/GOING-LIVE.md` Stage 1,
//! which is about exactly this class of problem, one layer up (order
//! retries rather than tick redelivery). Bolting a workaround onto this
//! harness would hide it, not fix it.

use crate::{demo_contract_id, demo_contract_spec};
use parallax_alpha::WeatherEnsembleSource;
use parallax_risk::RiskLimits;
use parallax_sim::{Backtest, BacktestReport, ReplayEvent};
use parallax_strategy::{
    LiquiditySnipingEngine, MarketMakingConfig, MarketMakingEngine, SnipingConfig, StatArbConfig,
    StatArbEngine,
};
use parallax_types::{AlphaEventKind, FeeModel, NormalizedTick, RawEvent, Timestamp, VenueId};
use parallax_venues::PaperConfig;
use rand::{Rng, SeedableRng};
use rand_pcg::Pcg64;

/// Bounds every perturbation stays within — wide enough to be a real
/// stress on the scenario, narrow enough that the qualitative shape of
/// "confident bullish ensemble, stale cheap Polymarket quote" survives
/// every draw. A jitter that could flip the ensemble's direction or
/// invert a quote's bid/ask would stop testing this strategy and start
/// testing a different, unrelated scenario.
const ENSEMBLE_JITTER_TENTHS: i32 = 15; // +/- 1.5 degF per member
const QUOTE_JITTER: f64 = 0.02; // +/- 2c on each side of each quoted book
const MAX_QUEUE_AHEAD_FRACTION: f64 = 0.5;

struct ScenarioDraw {
    ensemble_forecast_tenths: [i32; 5],
    stale_bid: f64,
    stale_ask: f64,
    kalshi_bid: f64,
    kalshi_ask: f64,
    queue_ahead_fraction: f64,
}

fn jitter_quote(rng: &mut impl Rng, base_bid: f64, base_ask: f64) -> (f64, f64) {
    let bid = (base_bid + rng.gen_range(-QUOTE_JITTER..=QUOTE_JITTER)).clamp(0.01, 0.98);
    let ask = (base_ask + rng.gen_range(-QUOTE_JITTER..=QUOTE_JITTER)).clamp(0.02, 0.99);
    // A crossed book (ask <= bid) isn't a smaller edge, it's not a book —
    // nudging the ask up preserves "some spread exists" without silently
    // discarding the draw (which would bias the sample toward whichever
    // seeds happened not to cross).
    if ask <= bid {
        (bid, (bid + 0.01).min(0.99))
    } else {
        (bid, ask)
    }
}

fn draw_scenario(rng: &mut impl Rng) -> ScenarioDraw {
    let base = [920, 930, 915, 925, 918];
    let mut ensemble_forecast_tenths = [0; 5];
    for (i, member) in base.iter().enumerate() {
        ensemble_forecast_tenths[i] =
            member + rng.gen_range(-ENSEMBLE_JITTER_TENTHS..=ENSEMBLE_JITTER_TENTHS);
    }
    let (stale_bid, stale_ask) = jitter_quote(rng, 0.50, 0.55);
    let (kalshi_bid, kalshi_ask) = jitter_quote(rng, 0.90, 0.94);
    ScenarioDraw {
        ensemble_forecast_tenths,
        stale_bid,
        stale_ask,
        kalshi_bid,
        kalshi_ask,
        queue_ahead_fraction: rng.gen_range(0.0..=MAX_QUEUE_AHEAD_FRACTION),
    }
}

/// One seeded draw of the demo scenario, run end to end through the real
/// pipeline. Deterministic in `seed`: the same seed always produces the
/// same draw and therefore the same report, which is what makes a run
/// reproducible for debugging a specific outlier.
pub async fn run_seeded_scenario(seed: u64) -> BacktestReport {
    let mut rng = Pcg64::seed_from_u64(seed);
    let draw = draw_scenario(&mut rng);

    let contract = demo_contract_id();
    let venue_config = PaperConfig {
        fee_model: FeeModel::polymarket_default(),
        queue_ahead_fraction: draw.queue_ahead_fraction,
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
            "ensemble_forecast_tenths": draw.ensemble_forecast_tenths,
        }),
    };

    let stale_quote = NormalizedTick {
        venue: VenueId::Polymarket,
        contract: contract.clone(),
        bid: draw.stale_bid,
        bid_size: 50.0,
        ask: draw.stale_ask,
        ask_size: 50.0,
        venue_ts: None,
        receive_ts: Timestamp::from_nanos(1_000_000),
    };

    let kalshi_quote = NormalizedTick {
        venue: VenueId::Kalshi,
        contract: contract.clone(),
        bid: draw.kalshi_bid,
        bid_size: 50.0,
        ask: draw.kalshi_ask,
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

/// The result of running `run_seeded_scenario` across many seeds: every
/// seed's net P&L, in seed order, plus which seeds had to be excluded and
/// why.
pub struct EdgeDistributionReport {
    pub net_pnls: Vec<f64>,
    /// Seeds excluded because `bus_integrity_violated` was set — per
    /// `BacktestReport::headline()`, a critical bus topic dropped an
    /// order ack that run, so its P&L is not a number, it's noise wearing
    /// a number's clothes. Averaging it in would silently corrupt every
    /// statistic below with a value nobody should trust.
    pub excluded_seeds: Vec<u64>,
}

impl EdgeDistributionReport {
    pub fn len(&self) -> usize {
        self.net_pnls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.net_pnls.is_empty()
    }

    pub fn mean(&self) -> f64 {
        if self.net_pnls.is_empty() {
            return 0.0;
        }
        self.net_pnls.iter().sum::<f64>() / self.net_pnls.len() as f64
    }

    /// Linear-interpolation percentile (`p` in `[0, 100]`) over the
    /// sorted sample — `percentile(10.0)` is the p10 the design doc's
    /// gate is stated against, `percentile(50.0)` is the median.
    pub fn percentile(&self, p: f64) -> f64 {
        if self.net_pnls.is_empty() {
            return 0.0;
        }
        let mut sorted = self.net_pnls.clone();
        sorted.sort_by(f64::total_cmp);
        if sorted.len() == 1 {
            return sorted[0];
        }
        let rank = (p.clamp(0.0, 100.0) / 100.0) * (sorted.len() - 1) as f64;
        let lo = rank.floor() as usize;
        let hi = rank.ceil() as usize;
        if lo == hi {
            sorted[lo]
        } else {
            let frac = rank - lo as f64;
            sorted[lo] + (sorted[hi] - sorted[lo]) * frac
        }
    }

    pub fn median(&self) -> f64 {
        self.percentile(50.0)
    }

    pub fn profitable_count(&self) -> usize {
        self.net_pnls.iter().filter(|&&pnl| pnl > 0.0).count()
    }
}

/// Runs `run_seeded_scenario` for seeds `0..n_seeds` and summarizes the
/// resulting net-P&L distribution.
///
/// This is still a synthetic scenario with perturbed synthetic inputs,
/// not the recorded real venue book data `docs/GOING-LIVE.md` Stage 0
/// ultimately calls for — it answers "how sensitive is this strategy's
/// P&L to realistic-sized noise in its inputs," which is a real and
/// useful question, but a distribution over synthetic noise is not the
/// same claim as a distribution over what the market actually did. Treat
/// a positive p10 here as "not obviously broken," not as "proven
/// profitable" — the latter needs the recorded-data replay this seeds.
pub async fn run_edge_distribution(n_seeds: u64) -> EdgeDistributionReport {
    let mut net_pnls = Vec::with_capacity(n_seeds as usize);
    let mut excluded_seeds = Vec::new();
    for seed in 0..n_seeds {
        let report = run_seeded_scenario(seed).await;
        if report.bus_integrity_violated {
            excluded_seeds.push(seed);
            continue;
        }
        net_pnls.push(report.net_pnl());
    }
    EdgeDistributionReport {
        net_pnls,
        excluded_seeds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_same_seed_always_produces_the_same_report() {
        let a = run_seeded_scenario(7).await;
        let b = run_seeded_scenario(7).await;
        assert_eq!(a.net_pnl(), b.net_pnl());
        assert_eq!(a.fills, b.fills);
        assert_eq!(a.orders_rejected_by_risk, b.orders_rejected_by_risk);
    }

    #[tokio::test]
    async fn different_seeds_are_not_all_identical() {
        // Not a statistical claim, just a sanity check that perturbation
        // is actually wired in rather than a no-op that always redraws
        // the same base scenario.
        let mut net_pnls = Vec::new();
        for seed in 0..20 {
            net_pnls.push(run_seeded_scenario(seed).await.net_pnl());
        }
        assert!(
            net_pnls.windows(2).any(|w| w[0] != w[1]),
            "expected at least one differing net_pnl across 20 seeds, got {net_pnls:?}"
        );
    }

    #[test]
    fn percentile_matches_a_hand_checked_distribution() {
        let report = EdgeDistributionReport {
            net_pnls: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0],
            excluded_seeds: vec![],
        };
        assert_eq!(report.median(), 5.5);
        // p10 over 10 evenly-spaced points [1..10]: rank = 0.1 * 9 = 0.9,
        // interpolating 90% of the way from sorted[0]=1 to sorted[1]=2.
        assert!((report.percentile(10.0) - 1.9).abs() < 1e-9);
        assert_eq!(report.percentile(0.0), 1.0);
        assert_eq!(report.percentile(100.0), 10.0);
    }

    #[test]
    fn profitable_count_only_counts_strictly_positive_runs() {
        let report = EdgeDistributionReport {
            net_pnls: vec![-1.0, 0.0, 0.5, 3.0],
            excluded_seeds: vec![],
        };
        assert_eq!(report.profitable_count(), 2);
    }

    #[test]
    fn an_empty_distribution_does_not_panic() {
        let report = EdgeDistributionReport {
            net_pnls: vec![],
            excluded_seeds: vec![],
        };
        assert_eq!(report.mean(), 0.0);
        assert_eq!(report.median(), 0.0);
        assert_eq!(report.profitable_count(), 0);
    }
}
