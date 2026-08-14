use crate::core::{StrategyEngine, StrategyInput};
use parallax_types::{EngineId, OrderIntent, OrderType, Outcome, Side};

#[derive(Debug, Clone, Copy)]
pub struct MarketMakingConfig {
    /// Minimum half-spread even when the confidence band is at its
    /// tightest — never quote a zero-width market.
    pub base_half_spread: f64,
    /// How much of the fair-value confidence band's half-width gets
    /// added to the quoted half-spread. Wider model disagreement ->
    /// wider quotes, directly implementing the grid-spread-capture
    /// principle (polymarket_lp_tool lineage) with the width driven by
    /// the alpha layer instead of a fixed constant.
    pub band_multiplier: f64,
    /// How far the quote center shifts, in price, per unit of signed
    /// inventory — positive inventory skews the center down (encourages
    /// selling down the position), negative skews it up.
    pub inventory_skew_sensitivity: f64,
    pub ladder_levels: usize,
    pub level_spacing: f64,
    pub level_size: f64,
    /// Assumed maker fee rate, applied on both legs of a round trip
    /// (design doc review 2.1): the quoted half-spread is floored at
    /// `maker_fee_rate * price * (1 - price)`, the actual round-trip cost
    /// of a filled quote at that price. A 1-cent half-spread earning 2¢
    /// gross per round trip against ~3.5¢ of real fees is a strategy that
    /// loses money faster the better it works; this is not a live
    /// per-venue lookup (`StrategyInput` deliberately carries no venue
    /// fee data, matching its "an engine sees its own inventory and the
    /// book, nothing more" boundary), so keep it in sync with whichever
    /// venue this engine is actually quoting.
    pub maker_fee_rate: f64,
}

impl Default for MarketMakingConfig {
    fn default() -> Self {
        MarketMakingConfig {
            base_half_spread: 0.01,
            band_multiplier: 0.5,
            inventory_skew_sensitivity: 0.0005,
            ladder_levels: 3,
            level_spacing: 0.01,
            level_size: 10.0,
            // Kalshi's published maker rate as of this review.
            maker_fee_rate: 0.0175,
        }
    }
}

/// Two-sided, laddered quoting around the consolidated fair value
/// (design doc §8 / CloddsBot + polymarket_lp_tool lineage): quotes are
/// centered on the multi-venue fair value rather than any one venue's
/// own mid, skewed by live inventory, and widened by model disagreement.
pub struct MarketMakingEngine {
    config: MarketMakingConfig,
}

impl MarketMakingEngine {
    pub fn new(config: MarketMakingConfig) -> Self {
        MarketMakingEngine { config }
    }
}

impl StrategyEngine for MarketMakingEngine {
    fn id(&self) -> EngineId {
        EngineId::MarketMaking
    }

    fn evaluate(&self, input: &StrategyInput) -> Vec<OrderIntent> {
        // Stand in whichever venue currently has the best live
        // fill-probability estimate, among venues actually quoting this
        // contract — the calibration-driven venue choice from design
        // doc §11.
        let venue = input
            .book
            .quotes(input.contract)
            .map(|t| t.venue)
            .max_by(|a, b| {
                input
                    .calibration
                    .estimate(*a)
                    .fill_probability
                    .total_cmp(&input.calibration.estimate(*b).fill_probability)
            });
        let Some(venue) = venue else {
            return Vec::new();
        };

        let inventory = *input.inventory.get(&venue).unwrap_or(&0.0);
        let skew = self.config.inventory_skew_sensitivity * inventory;
        let center = (input.fair_value.midpoint - skew).clamp(0.0, 1.0);

        let round_trip_fee_floor = self.config.maker_fee_rate * center * (1.0 - center);
        let half_spread = (self.config.base_half_spread
            + self.config.band_multiplier * (input.fair_value.band_width() / 2.0))
            .max(round_trip_fee_floor);

        let mut intents = Vec::new();
        for level in 0..self.config.ladder_levels {
            let offset = half_spread + level as f64 * self.config.level_spacing;
            let bid_price = center - offset;
            let ask_price = center + offset;

            if bid_price > 0.0 {
                intents.push(OrderIntent {
                    venue,
                    contract: input.contract.clone(),
                    outcome: Outcome::Yes,
                    side: Side::Buy,
                    price: bid_price.clamp(0.0, 1.0),
                    size: self.config.level_size,
                    order_type: OrderType::Limit,
                    engine: EngineId::MarketMaking,
                    created_at: input.now,
                });
            }
            if ask_price < 1.0 {
                intents.push(OrderIntent {
                    venue,
                    contract: input.contract.clone(),
                    outcome: Outcome::Yes,
                    side: Side::Sell,
                    price: ask_price.clamp(0.0, 1.0),
                    size: self.config.level_size,
                    order_type: OrderType::Limit,
                    engine: EngineId::MarketMaking,
                    created_at: input.now,
                });
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

    fn fair_value(mid: f64, band_half_width: f64) -> FairValue {
        FairValue {
            contract: contract(),
            midpoint: mid,
            band_low: (mid - band_half_width).clamp(0.0, 1.0),
            band_high: (mid + band_half_width).clamp(0.0, 1.0),
            as_of: Timestamp::from_nanos(0),
            inputs: vec![],
            effective_sample_size: 1.0,
        }
    }

    fn book_quoting(venue: VenueId) -> ConsolidatedBook {
        let mut book = ConsolidatedBook::new();
        book.update(NormalizedTick {
            venue,
            contract: contract(),
            bid: 0.6,
            bid_size: 10.0,
            ask: 0.64,
            ask_size: 10.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        });
        book
    }

    #[test]
    fn quotes_are_centered_on_fair_value_when_flat() {
        let engine = MarketMakingEngine::new(MarketMakingConfig::default());
        let book = book_quoting(VenueId::Kalshi);
        let fv = fair_value(0.66, 0.02);
        let inventory = HashMap::new();
        let calibration = Calibrator::default();
        let input = StrategyInput {
            contract: &contract(),
            fair_value: &fv,
            book: &book,
            inventory: &inventory,
            calibration: &calibration,
            now: Timestamp::from_nanos(0),
        };
        let intents = engine.evaluate(&input);
        assert!(!intents.is_empty());
        let best_bid = intents
            .iter()
            .filter(|i| i.side == Side::Buy)
            .map(|i| i.price)
            .fold(f64::MIN, f64::max);
        let best_ask = intents
            .iter()
            .filter(|i| i.side == Side::Sell)
            .map(|i| i.price)
            .fold(f64::MAX, f64::min);
        assert!((0.66 - best_bid) > 0.0);
        assert!((best_ask - 0.66) > 0.0);
        // roughly symmetric around fair value
        assert!(((0.66 - best_bid) - (best_ask - 0.66)).abs() < 1e-9);
    }

    #[test]
    fn wider_confidence_band_widens_the_quoted_spread() {
        let engine = MarketMakingEngine::new(MarketMakingConfig::default());
        let book = book_quoting(VenueId::Kalshi);
        let inventory = HashMap::new();
        let calibration = Calibrator::default();

        let tight = fair_value(0.66, 0.01);
        let input_tight = StrategyInput {
            contract: &contract(),
            fair_value: &tight,
            book: &book,
            inventory: &inventory,
            calibration: &calibration,
            now: Timestamp::from_nanos(0),
        };
        let wide = fair_value(0.66, 0.10);
        let input_wide = StrategyInput {
            fair_value: &wide,
            ..input_tight_clone(&input_tight)
        };

        let tight_intents = engine.evaluate(&input_tight);
        let wide_intents = engine.evaluate(&input_wide);
        let tight_best_ask = tight_intents
            .iter()
            .filter(|i| i.side == Side::Sell)
            .map(|i| i.price)
            .fold(f64::MAX, f64::min);
        let wide_best_ask = wide_intents
            .iter()
            .filter(|i| i.side == Side::Sell)
            .map(|i| i.price)
            .fold(f64::MAX, f64::min);
        assert!(wide_best_ask > tight_best_ask);
    }

    // StrategyInput borrows several fields; this helper just re-shares
    // the same borrows for a second input value in the band-width test
    // above without fighting the borrow checker over struct update syntax.
    fn input_tight_clone<'a>(input: &StrategyInput<'a>) -> StrategyInput<'a> {
        StrategyInput {
            contract: input.contract,
            fair_value: input.fair_value,
            book: input.book,
            inventory: input.inventory,
            calibration: input.calibration,
            now: input.now,
        }
    }

    #[test]
    fn positive_inventory_skews_quotes_down() {
        let engine = MarketMakingEngine::new(MarketMakingConfig::default());
        let book = book_quoting(VenueId::Kalshi);
        let fv = fair_value(0.66, 0.02);
        let calibration = Calibrator::default();

        let mut flat_inv = HashMap::new();
        flat_inv.insert(VenueId::Kalshi, 0.0);
        let flat_input = StrategyInput {
            contract: &contract(),
            fair_value: &fv,
            book: &book,
            inventory: &flat_inv,
            calibration: &calibration,
            now: Timestamp::from_nanos(0),
        };

        let mut long_inv = HashMap::new();
        long_inv.insert(VenueId::Kalshi, 400.0);
        let long_input = StrategyInput {
            inventory: &long_inv,
            ..input_tight_clone(&flat_input)
        };

        let flat_mid = mid_of(&engine.evaluate(&flat_input));
        let long_mid = mid_of(&engine.evaluate(&long_input));
        assert!(
            long_mid < flat_mid,
            "long inventory should skew quotes down: flat={flat_mid} long={long_mid}"
        );
    }

    fn mid_of(intents: &[OrderIntent]) -> f64 {
        let best_bid = intents
            .iter()
            .filter(|i| i.side == Side::Buy)
            .map(|i| i.price)
            .fold(f64::MIN, f64::max);
        let best_ask = intents
            .iter()
            .filter(|i| i.side == Side::Sell)
            .map(|i| i.price)
            .fold(f64::MAX, f64::min);
        (best_bid + best_ask) / 2.0
    }

    #[test]
    fn half_spread_is_floored_at_the_round_trip_fee_cost() {
        // A near-zero base spread and a tight band would otherwise quote
        // a half-spread far too thin to cover the round-trip fee at a
        // 50-cent contract (design doc review 2.1).
        let config = MarketMakingConfig {
            base_half_spread: 0.0001,
            band_multiplier: 0.0,
            maker_fee_rate: 0.0175,
            ladder_levels: 1,
            ..MarketMakingConfig::default()
        };
        let engine = MarketMakingEngine::new(config);
        let book = book_quoting(VenueId::Kalshi);
        let fv = fair_value(0.50, 0.0001);
        let inventory = HashMap::new();
        let calibration = Calibrator::default();
        let input = StrategyInput {
            contract: &contract(),
            fair_value: &fv,
            book: &book,
            inventory: &inventory,
            calibration: &calibration,
            now: Timestamp::from_nanos(0),
        };
        let intents = engine.evaluate(&input);
        let half_spread =
            (mid_of(&intents) - intents.iter().find(|i| i.side == Side::Buy).unwrap().price).abs();
        let expected_floor = 0.0175 * 0.50 * 0.50;
        assert!(
            half_spread >= expected_floor - 1e-9,
            "half_spread {half_spread} should be at least the fee floor {expected_floor}"
        );
    }
}
