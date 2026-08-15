//! Stage 0 of `docs/GOING-LIVE.md`: "report the distribution, not a
//! number... run many seeds and many time periods... judge on the 10th
//! percentile." A single deterministic backtest (`run_demo_backtest`)
//! proves the pipeline wires together; it cannot prove a strategy has
//! edge, because one draw of forecast/quote noise, in one market regime,
//! is not evidence about the distribution those numbers are drawn from.
//!
//! This runs the same weather-update → stale-quote → fill scenario many
//! times, perturbing per seed exactly the things a real deployment
//! cannot pin down in advance — the ensemble forecast, both venues'
//! quoted prices, and queue position — and reports the resulting P&L
//! distribution rather than one number. It is still synthetic data, not
//! the recorded venue book data Stage 0 ultimately calls for; see the
//! module-level caveat on `run_edge_distribution`.
//!
//! [`Regime`] is the "many time periods" half of that sentence: the
//! doc's own gate is explicit that a positive p10 in one market
//! condition isn't enough — "across periods that include at least one
//! volatility shock and one quiet week." `run_multi_regime_distribution`
//! runs the identical seeded-perturbation methodology against three
//! distinct base scenarios (a confident baseline, a wider/faster
//! volatility-shock variant, and a low-signal quiet-period variant) and
//! reports whether p10 clears zero in *every* one of them — the gate,
//! checked, not just described.
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

/// A distinct market condition the same seeded-perturbation methodology
/// is run against — the doc's "many time periods," made concrete instead
/// of left as one scenario perturbed by noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Regime {
    /// The original demo scenario: a confident, unanimous bullish
    /// ensemble against a Polymarket quote that hasn't caught up yet.
    Baseline,
    /// A bigger, faster, more chaotic move than baseline: wider ensemble
    /// *disagreement* even as the mean forecast moves further past the
    /// threshold, and wider, further-repriced quotes on both venues.
    /// What this regime is actually for is stress-testing the risk
    /// gate's collar and notional limits under a bigger shock — not just
    /// checking the strategy stays profitable when the edge is even more
    /// obvious.
    VolatilityShock,
    /// A low-signal day: the ensemble sits close to the threshold (a
    /// real but weak, uncertain direction) and both venues already agree
    /// closely with each other. Little to no riskless mispricing exists
    /// to take — this is the realistic common case a strategy has to not
    /// lose money on, not the dramatic day baseline models.
    QuietPeriod,
}

impl Regime {
    fn base_ensemble_tenths(self) -> [i32; 5] {
        match self {
            Regime::Baseline => [920, 930, 915, 925, 918],
            Regime::VolatilityShock => [960, 890, 945, 875, 930],
            Regime::QuietPeriod => [875, 880, 870, 878, 872],
        }
    }

    fn base_stale_quote(self) -> (f64, f64) {
        match self {
            Regime::Baseline => (0.50, 0.55),
            Regime::VolatilityShock => (0.40, 0.62),
            Regime::QuietPeriod => (0.56, 0.59),
        }
    }

    fn base_kalshi_quote(self) -> (f64, f64) {
        match self {
            Regime::Baseline => (0.90, 0.94),
            Regime::VolatilityShock => (0.93, 0.99),
            Regime::QuietPeriod => (0.57, 0.60),
        }
    }

    /// +/- tenths of a degree F applied independently to each ensemble
    /// member — wider in a volatility-shock regime (more day-to-day
    /// forecast noise in a chaotic period), tighter but still real in a
    /// quiet one.
    fn ensemble_jitter_tenths(self) -> i32 {
        match self {
            Regime::Baseline => 15,
            Regime::VolatilityShock => 30,
            Regime::QuietPeriod => 8,
        }
    }

    /// +/- price jitter applied independently to each quoted bid/ask.
    fn quote_jitter(self) -> f64 {
        match self {
            Regime::Baseline => 0.02,
            Regime::VolatilityShock => 0.05,
            Regime::QuietPeriod => 0.01,
        }
    }
}

const MAX_QUEUE_AHEAD_FRACTION: f64 = 0.5;

struct ScenarioDraw {
    ensemble_forecast_tenths: [i32; 5],
    stale_bid: f64,
    stale_ask: f64,
    kalshi_bid: f64,
    kalshi_ask: f64,
    queue_ahead_fraction: f64,
}

fn jitter_quote(rng: &mut impl Rng, jitter: f64, base_bid: f64, base_ask: f64) -> (f64, f64) {
    let bid = (base_bid + rng.gen_range(-jitter..=jitter)).clamp(0.01, 0.98);
    let ask = (base_ask + rng.gen_range(-jitter..=jitter)).clamp(0.02, 0.99);
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

fn draw_scenario(regime: Regime, rng: &mut impl Rng) -> ScenarioDraw {
    let base = regime.base_ensemble_tenths();
    let ensemble_jitter = regime.ensemble_jitter_tenths();
    let mut ensemble_forecast_tenths = [0; 5];
    for (i, member) in base.iter().enumerate() {
        ensemble_forecast_tenths[i] = member + rng.gen_range(-ensemble_jitter..=ensemble_jitter);
    }
    let quote_jitter = regime.quote_jitter();
    let (stale_base_bid, stale_base_ask) = regime.base_stale_quote();
    let (kalshi_base_bid, kalshi_base_ask) = regime.base_kalshi_quote();
    let (stale_bid, stale_ask) = jitter_quote(rng, quote_jitter, stale_base_bid, stale_base_ask);
    let (kalshi_bid, kalshi_ask) =
        jitter_quote(rng, quote_jitter, kalshi_base_bid, kalshi_base_ask);
    ScenarioDraw {
        ensemble_forecast_tenths,
        stale_bid,
        stale_ask,
        kalshi_bid,
        kalshi_ask,
        queue_ahead_fraction: rng.gen_range(0.0..=MAX_QUEUE_AHEAD_FRACTION),
    }
}

/// One seeded draw of `regime`, run end to end through the real pipeline.
/// Deterministic in `(regime, seed)`: the same pair always produces the
/// same draw and therefore the same report, which is what makes a run
/// reproducible for debugging a specific outlier.
pub async fn run_seeded_scenario(regime: Regime, seed: u64) -> BacktestReport {
    let mut rng = Pcg64::seed_from_u64(seed);
    let draw = draw_scenario(regime, &mut rng);

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

/// Runs `run_seeded_scenario` for seeds `0..n_seeds` under `regime` and
/// summarizes the resulting net-P&L distribution.
///
/// This is still a synthetic scenario with perturbed synthetic inputs,
/// not the recorded real venue book data `docs/GOING-LIVE.md` Stage 0
/// ultimately calls for — it answers "how sensitive is this strategy's
/// P&L to realistic-sized noise in its inputs, across a few distinct
/// market conditions," which is a real and useful question, but a
/// distribution over synthetic noise is not the same claim as a
/// distribution over what the market actually did. Treat a positive p10
/// here as "not obviously broken," not as "proven profitable" — the
/// latter needs the recorded-data replay this seeds.
pub async fn run_edge_distribution(regime: Regime, n_seeds: u64) -> EdgeDistributionReport {
    let mut net_pnls = Vec::with_capacity(n_seeds as usize);
    let mut excluded_seeds = Vec::new();
    for seed in 0..n_seeds {
        let report = run_seeded_scenario(regime, seed).await;
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

/// The result of running the full edge-distribution methodology
/// independently against all three `Regime`s.
pub struct MultiRegimeReport {
    pub baseline: EdgeDistributionReport,
    pub volatility_shock: EdgeDistributionReport,
    pub quiet_period: EdgeDistributionReport,
}

impl MultiRegimeReport {
    /// `docs/GOING-LIVE.md` Stage 0's gate, checked rather than just
    /// described: "out-of-sample p10 is positive after fees, across
    /// periods that include at least one volatility shock and one quiet
    /// week." `true` only if every regime's p10 clears zero — one good
    /// regime and one bad one is a fail, not an average.
    pub fn gate_passes(&self) -> bool {
        self.baseline.percentile(10.0) > 0.0
            && self.volatility_shock.percentile(10.0) > 0.0
            && self.quiet_period.percentile(10.0) > 0.0
    }
}

pub async fn run_multi_regime_distribution(n_seeds: u64) -> MultiRegimeReport {
    MultiRegimeReport {
        baseline: run_edge_distribution(Regime::Baseline, n_seeds).await,
        volatility_shock: run_edge_distribution(Regime::VolatilityShock, n_seeds).await,
        quiet_period: run_edge_distribution(Regime::QuietPeriod, n_seeds).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_same_regime_and_seed_always_produces_the_same_report() {
        let a = run_seeded_scenario(Regime::Baseline, 7).await;
        let b = run_seeded_scenario(Regime::Baseline, 7).await;
        assert_eq!(a.net_pnl(), b.net_pnl());
        assert_eq!(a.fills, b.fills);
        assert_eq!(a.orders_rejected_by_risk, b.orders_rejected_by_risk);
    }

    #[tokio::test]
    async fn the_same_seed_under_different_regimes_can_differ() {
        let baseline = run_seeded_scenario(Regime::Baseline, 0).await;
        let shock = run_seeded_scenario(Regime::VolatilityShock, 0).await;
        let quiet = run_seeded_scenario(Regime::QuietPeriod, 0).await;
        // Not a claim about which is bigger — just that regime actually
        // changes the scenario rather than being a no-op label.
        assert!(
            baseline.net_pnl() != shock.net_pnl() || baseline.net_pnl() != quiet.net_pnl(),
            "expected at least one regime to differ from baseline at seed 0"
        );
    }

    #[tokio::test]
    async fn different_seeds_are_not_all_identical() {
        // Not a statistical claim, just a sanity check that perturbation
        // is actually wired in rather than a no-op that always redraws
        // the same base scenario.
        let mut net_pnls = Vec::new();
        for seed in 0..20 {
            net_pnls.push(run_seeded_scenario(Regime::Baseline, seed).await.net_pnl());
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

    #[test]
    fn gate_passes_only_when_every_regime_clears_a_positive_p10() {
        let positive = EdgeDistributionReport {
            net_pnls: vec![1.0; 10],
            excluded_seeds: vec![],
        };
        let negative = EdgeDistributionReport {
            net_pnls: vec![-1.0; 10],
            excluded_seeds: vec![],
        };
        let all_positive = MultiRegimeReport {
            baseline: EdgeDistributionReport {
                net_pnls: positive.net_pnls.clone(),
                excluded_seeds: vec![],
            },
            volatility_shock: EdgeDistributionReport {
                net_pnls: positive.net_pnls.clone(),
                excluded_seeds: vec![],
            },
            quiet_period: EdgeDistributionReport {
                net_pnls: positive.net_pnls.clone(),
                excluded_seeds: vec![],
            },
        };
        assert!(all_positive.gate_passes());

        let one_negative = MultiRegimeReport {
            baseline: EdgeDistributionReport {
                net_pnls: positive.net_pnls.clone(),
                excluded_seeds: vec![],
            },
            volatility_shock: EdgeDistributionReport {
                net_pnls: negative.net_pnls.clone(),
                excluded_seeds: vec![],
            },
            quiet_period: EdgeDistributionReport {
                net_pnls: positive.net_pnls,
                excluded_seeds: vec![],
            },
        };
        assert!(
            !one_negative.gate_passes(),
            "one failing regime must fail the whole gate, not average out"
        );
    }
}
