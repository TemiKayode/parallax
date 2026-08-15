use crate::calibration::Calibrator;
use parallax_book::ConsolidatedBook;
use parallax_types::{CanonicalContractId, FairValue, Side, VenueId};

#[derive(Debug, Clone, Copy)]
pub struct EdgeConfig {
    /// Multiplier on the fair-value confidence band's width (design doc
    /// §5): a wider band — more model disagreement or staleness — demands
    /// more edge before capital moves, for free, reusing plumbing that
    /// already exists rather than a separate hand-tuned constant.
    pub safety_margin_k: f64,
    pub taker_fee_bps: f64,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        EdgeConfig {
            safety_margin_k: 0.5,
            taker_fee_bps: 200.0,
        }
    }
}

/// One size's worth of the honest answer to "is this actually tradable,"
/// not just "does the model disagree with the touch." `tradable_edge`
/// negative means don't trade this size, full stop.
#[derive(Debug, Clone, Copy)]
pub struct EdgeResult {
    pub side: Side,
    /// How much of the requested size the book could actually fill —
    /// may be less than what was asked for on a thin book.
    pub size: f64,
    pub expected_avg_entry: f64,
    pub execution_costs: f64,
    pub safety_margin: f64,
    pub tradable_edge: f64,
}

/// Everything `compute_tradable_edge` and `largest_profitable_size` need
/// that stays fixed while size varies — bundled so sweeping many
/// candidate sizes doesn't mean repeating the same seven-argument call.
#[derive(Clone, Copy)]
pub struct EdgeContext<'a> {
    pub fair_value: &'a FairValue,
    pub book: &'a ConsolidatedBook,
    pub contract: &'a CanonicalContractId,
    pub venue: VenueId,
    pub side: Side,
    pub calibration: &'a Calibrator,
    pub config: &'a EdgeConfig,
}

/// `TradableEdge(size) = FairValue.midpoint − ExpectedAvgEntry(size) −
/// ExecutionCosts − SafetyMargin` (design doc §5), computed for one
/// specific size against one specific venue's actual depth — never
/// against the top-of-book quote alone. Returns `None` if there's no
/// depth snapshot to walk (design doc §13: this is exactly why
/// `ConsolidatedBook` needed a real depth extension, not a workaround).
pub fn compute_tradable_edge(ctx: &EdgeContext, size: f64) -> Option<EdgeResult> {
    let walk = match ctx.side {
        Side::Buy => ctx.book.walk_ask(ctx.contract, ctx.venue, size)?,
        Side::Sell => ctx.book.walk_bid(ctx.contract, ctx.venue, size)?,
    };

    let calibration_est = ctx.calibration.estimate(ctx.venue);
    let execution_costs = ctx.config.taker_fee_bps / 10_000.0 + calibration_est.expected_slippage;
    let safety_margin = ctx.config.safety_margin_k * ctx.fair_value.band_width();

    let raw_edge = match ctx.side {
        Side::Buy => ctx.fair_value.midpoint - walk.avg_price,
        Side::Sell => walk.avg_price - ctx.fair_value.midpoint,
    };

    Some(EdgeResult {
        side: ctx.side,
        size: walk.filled_size,
        expected_avg_entry: walk.avg_price,
        execution_costs,
        safety_margin,
        tradable_edge: raw_edge - execution_costs - safety_margin,
    })
}

/// Sweeps `candidate_sizes` and returns the result for the *largest* size
/// whose `tradable_edge` is still positive — the size the position-
/// structure selector should actually act on, not just whatever size
/// happened to be checked first. `None` if no candidate size clears the
/// bar (design doc §5: "a 9¢ theoretical edge means nothing if only 50
/// shares can capture it" — this is the function that finds out how many
/// shares actually can).
pub fn largest_profitable_size(ctx: &EdgeContext, candidate_sizes: &[f64]) -> Option<EdgeResult> {
    candidate_sizes
        .iter()
        .filter_map(|&size| compute_tradable_edge(ctx, size))
        .filter(|r| r.tradable_edge > 0.0)
        .max_by(|a, b| a.size.total_cmp(&b.size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::{
        BookDepth, CanonicalContractSpec, DepthLevel, Direction, EventClass, Timestamp,
    };

    fn contract() -> CanonicalContractId {
        CanonicalContractSpec {
            event_class: EventClass("crypto.updown".into()),
            location: "btc".into(),
            threshold: 0,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12t12-05-00z".into(),
            resolution_source: "binance".into(),
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

    fn book_with_ask_levels(levels: &[(f64, f64)]) -> ConsolidatedBook {
        let mut book = ConsolidatedBook::new();
        book.update_depth(BookDepth {
            venue: VenueId::Polymarket,
            contract: contract(),
            bids: vec![DepthLevel {
                price: 0.50,
                size: 1000.0,
            }],
            asks: levels
                .iter()
                .map(|&(price, size)| DepthLevel { price, size })
                .collect(),
            receive_ts: Timestamp::from_nanos(0),
        });
        book
    }

    fn ctx<'a>(
        fair_value: &'a FairValue,
        book: &'a ConsolidatedBook,
        contract: &'a CanonicalContractId,
        calibration: &'a Calibrator,
        config: &'a EdgeConfig,
    ) -> EdgeContext<'a> {
        EdgeContext {
            fair_value,
            book,
            contract,
            venue: VenueId::Polymarket,
            side: Side::Buy,
            calibration,
            config,
        }
    }

    #[test]
    fn returns_none_without_a_depth_snapshot() {
        let book = ConsolidatedBook::new();
        let fv = fair_value(0.70, 0.02);
        let contract = contract();
        let calibration = Calibrator::default();
        let config = EdgeConfig::default();
        let result =
            compute_tradable_edge(&ctx(&fv, &book, &contract, &calibration, &config), 10.0);
        assert!(result.is_none());
    }

    #[test]
    fn small_size_against_a_deep_cheap_book_is_profitable() {
        let book = book_with_ask_levels(&[(0.55, 500.0)]);
        let fv = fair_value(0.70, 0.01);
        let contract = contract();
        let calibration = Calibrator::default();
        let config = EdgeConfig::default();
        let result =
            compute_tradable_edge(&ctx(&fv, &book, &contract, &calibration, &config), 10.0)
                .unwrap();
        assert!((result.expected_avg_entry - 0.55).abs() < 1e-9);
        assert!(
            result.tradable_edge > 0.0,
            "edge was {}",
            result.tradable_edge
        );
    }

    #[test]
    fn walking_deeper_into_a_thin_book_shrinks_the_edge() {
        // 9c naive edge (0.70 fair value vs 0.61 best ask) on only 10
        // shares; the rest of the book is much worse.
        let book = book_with_ask_levels(&[(0.61, 10.0), (0.75, 500.0)]);
        let fv = fair_value(0.70, 0.005);
        let contract = contract();
        let calibration = Calibrator::default();
        let config = EdgeConfig::default();
        let c = ctx(&fv, &book, &contract, &calibration, &config);

        let small = compute_tradable_edge(&c, 10.0).unwrap();
        let large = compute_tradable_edge(&c, 200.0).unwrap();

        assert!(
            small.tradable_edge > 0.0,
            "small size should still be profitable, got {}",
            small.tradable_edge
        );
        assert!(
            large.tradable_edge < small.tradable_edge,
            "large={} small={}",
            large.tradable_edge,
            small.tradable_edge
        );
    }

    #[test]
    fn wider_confidence_band_demands_more_edge() {
        let book = book_with_ask_levels(&[(0.60, 100.0)]);
        let contract = contract();
        let calibration = Calibrator::default();
        let config = EdgeConfig::default();

        let tight_fv = fair_value(0.70, 0.01);
        let tight = compute_tradable_edge(
            &ctx(&tight_fv, &book, &contract, &calibration, &config),
            10.0,
        )
        .unwrap();
        let wide_fv = fair_value(0.70, 0.10);
        let wide = compute_tradable_edge(
            &ctx(&wide_fv, &book, &contract, &calibration, &config),
            10.0,
        )
        .unwrap();

        assert!(wide.tradable_edge < tight.tradable_edge);
    }

    #[test]
    fn largest_profitable_size_picks_the_biggest_size_that_still_clears_the_bar() {
        // Breakeven average price here is 0.70 - (0.005 safety + 0.005 fee) = 0.69.
        // 30 units at 0.60 stays well under that; walking further into the
        // 0.75 level for size 100 or 300 pushes the average past it.
        let book = book_with_ask_levels(&[(0.60, 30.0), (0.75, 200.0)]);
        let fv = fair_value(0.70, 0.005);
        let contract = contract();
        let calibration = Calibrator::default();
        let config = EdgeConfig {
            safety_margin_k: 0.5,
            taker_fee_bps: 50.0,
        };

        let result = largest_profitable_size(
            &ctx(&fv, &book, &contract, &calibration, &config),
            &[10.0, 30.0, 100.0, 300.0],
        )
        .unwrap();
        assert_eq!(result.size, 30.0);
    }

    #[test]
    fn no_candidate_size_profitable_returns_none() {
        let book = book_with_ask_levels(&[(0.72, 500.0)]);
        let fv = fair_value(0.70, 0.005);
        let contract = contract();
        let calibration = Calibrator::default();
        let config = EdgeConfig::default();
        let result = largest_profitable_size(
            &ctx(&fv, &book, &contract, &calibration, &config),
            &[10.0, 50.0],
        );
        assert!(result.is_none());
    }
}
