use crate::core::{StrategyEngine, StrategyInput};
use parallax_types::{EngineId, OrderIntent, OrderType, Outcome, Side};

#[derive(Debug, Clone, Copy)]
pub struct StatArbConfig {
    /// Fraction of full Kelly actually taken — quarter-Kelly (0.25) is a
    /// conventional conservative default, trading long-run growth rate
    /// for a much smaller drawdown than full Kelly risks on a
    /// mis-specified edge.
    pub kelly_fraction_cap: f64,
    pub max_order_size: f64,
}

impl Default for StatArbConfig {
    fn default() -> Self {
        StatArbConfig {
            kelly_fraction_cap: 0.25,
            max_order_size: 200.0,
        }
    }
}

/// Fires only when a venue's price sits fully outside the fair-value
/// confidence band (design doc §8) — not merely away from the midpoint.
/// Sized by fractional Kelly for a binary contract — `f* = (q−p)/(1−p)`
/// buying, `f* = (p−q)/p` selling, where `p` is the venue's own price and
/// `q` is the fair-value midpoint — rather than a formula that happened
/// to be named for Kelly without deriving from it (design doc review
/// 2.14). This correctly sizes small near either extreme (being wrong at
/// 2¢ or 98¢ costs almost nothing *or* almost everything, depending on
/// which side) and is capped by the venue's own displayed size — asking
/// for 200 contracts against 5 on offer just rests the other 195 at a
/// price the model has already called wrong.
pub struct StatArbEngine {
    config: StatArbConfig,
}

impl StatArbEngine {
    pub fn new(config: StatArbConfig) -> Self {
        StatArbEngine { config }
    }

    /// `market_price` is the venue's own price being transacted against;
    /// `fair_prob` is the model's fair-value midpoint. Returns a size in
    /// `[0, max_order_size]`, additionally capped by `displayed_size`.
    fn kelly_size(
        &self,
        side: Side,
        market_price: f64,
        fair_prob: f64,
        displayed_size: f64,
    ) -> f64 {
        let p = market_price.clamp(1e-6, 1.0 - 1e-6);
        let q = fair_prob.clamp(0.0, 1.0);
        let raw_fraction = match side {
            Side::Buy => (q - p) / (1.0 - p),
            Side::Sell => (p - q) / p,
        };
        let fraction = (raw_fraction * self.config.kelly_fraction_cap).clamp(0.0, 1.0);
        (self.config.max_order_size * fraction)
            .min(displayed_size)
            .max(0.0)
    }
}

impl StrategyEngine for StatArbEngine {
    fn id(&self) -> EngineId {
        EngineId::StatArb
    }

    fn evaluate(&self, input: &StrategyInput) -> Vec<OrderIntent> {
        let mut intents = Vec::new();

        for tick in input.book.quotes(input.contract) {
            // Cheap: this venue's ask sits below the band -> buy it.
            if tick.ask < input.fair_value.band_low {
                let size = self.kelly_size(
                    Side::Buy,
                    tick.ask,
                    input.fair_value.midpoint,
                    tick.ask_size,
                );
                if size > 0.0 {
                    intents.push(OrderIntent {
                        venue: tick.venue,
                        contract: input.contract.clone(),
                        outcome: Outcome::Yes,
                        side: Side::Buy,
                        price: tick.ask,
                        size,
                        order_type: OrderType::Limit,
                        engine: EngineId::StatArb,
                        created_at: input.now,
                    });
                }
            }
            // Rich: this venue's bid sits above the band -> sell into it.
            if tick.bid > input.fair_value.band_high {
                let size = self.kelly_size(
                    Side::Sell,
                    tick.bid,
                    input.fair_value.midpoint,
                    tick.bid_size,
                );
                if size > 0.0 {
                    intents.push(OrderIntent {
                        venue: tick.venue,
                        contract: input.contract.clone(),
                        outcome: Outcome::Yes,
                        side: Side::Sell,
                        price: tick.bid,
                        size,
                        order_type: OrderType::Limit,
                        engine: EngineId::StatArb,
                        created_at: input.now,
                    });
                }
            }
        }
        intents
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::Calibrator;
    use parallax_book::ConsolidatedBook;
    use parallax_types::{
        CanonicalContractSpec, Direction, EventClass, FairValue, NormalizedTick, Timestamp, VenueId,
    };
    use std::collections::HashMap;

    fn contract() -> parallax_types::CanonicalContractId {
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

    fn fair_value_66_pm3() -> FairValue {
        FairValue {
            contract: contract(),
            midpoint: 0.66,
            band_low: 0.63,
            band_high: 0.69,
            as_of: Timestamp::from_nanos(0),
            inputs: vec![],
            effective_sample_size: 1.0,
        }
    }

    fn tick(venue: VenueId, bid: f64, ask: f64) -> NormalizedTick {
        NormalizedTick {
            venue,
            contract: contract(),
            bid,
            bid_size: 50.0,
            ask,
            ask_size: 50.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        }
    }

    fn run(book: ConsolidatedBook, fv: &FairValue) -> Vec<OrderIntent> {
        let inventory = HashMap::new();
        let calibration = Calibrator::default();
        let engine = StatArbEngine::new(StatArbConfig::default());
        let input = StrategyInput {
            contract: &contract(),
            fair_value: fv,
            book: &book,
            inventory: &inventory,
            calibration: &calibration,
            now: Timestamp::from_nanos(0),
        };
        engine.evaluate(&input)
    }

    #[test]
    fn cheap_venue_triggers_a_buy() {
        let mut book = ConsolidatedBook::new();
        book.update(tick(VenueId::Polymarket, 0.58, 0.60)); // ask 0.60 < band_low 0.63
        let intents = run(book, &fair_value_66_pm3());
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].side, Side::Buy);
        assert_eq!(intents[0].venue, VenueId::Polymarket);
        assert!(intents[0].size > 0.0);
    }

    #[test]
    fn rich_venue_triggers_a_sell() {
        let mut book = ConsolidatedBook::new();
        book.update(tick(VenueId::Kalshi, 0.72, 0.75)); // bid 0.72 > band_high 0.69
        let intents = run(book, &fair_value_66_pm3());
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].side, Side::Sell);
        assert_eq!(intents[0].venue, VenueId::Kalshi);
    }

    #[test]
    fn price_inside_band_triggers_nothing() {
        let mut book = ConsolidatedBook::new();
        book.update(tick(VenueId::Kalshi, 0.65, 0.67)); // both inside [0.63, 0.69]
        let intents = run(book, &fair_value_66_pm3());
        assert!(intents.is_empty());
    }

    #[test]
    fn larger_edge_sizes_larger() {
        let mut small_edge_book = ConsolidatedBook::new();
        small_edge_book.update(tick(VenueId::Polymarket, 0.58, 0.62)); // just below band_low 0.63
        let small = run(small_edge_book, &fair_value_66_pm3());

        let mut large_edge_book = ConsolidatedBook::new();
        large_edge_book.update(tick(VenueId::Polymarket, 0.30, 0.35)); // far below band_low
        let large = run(large_edge_book, &fair_value_66_pm3());

        assert!(large[0].size > small[0].size);
    }
}
