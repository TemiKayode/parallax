//! The consolidated cross-venue order book (design doc §6): one canonical
//! contract maps to many venues' independent quotes, and this crate is
//! where "many venues" collapses into "one view" — both the ordinary
//! consolidated mid used to center market-making quotes, and direct
//! cross-venue arbitrage detection (ask on one venue crossing the bid on
//! another for the *identical* canonical contract, no model required).

#![forbid(unsafe_code)]

use parallax_types::{
    BookDepth, CanonicalContractId, NormalizedTick, Timestamp, VenueId, WalkResult,
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BestQuote {
    pub venue: VenueId,
    pub price: f64,
    pub size: f64,
}

/// A riskless cross-venue mispricing on the *same* canonical contract:
/// buying at `buy_price` on `buy_venue` and selling at `sell_price` on
/// `sell_venue` locks in `edge` per share before fees, independent of any
/// alpha model. This is distinct from — and strictly higher-confidence
/// than — the model-driven stat-arb signal in `parallax-strategy`.
#[derive(Debug, Clone, PartialEq)]
pub struct CrossVenueArb {
    pub contract: CanonicalContractId,
    pub buy_venue: VenueId,
    pub buy_price: f64,
    pub sell_venue: VenueId,
    pub sell_price: f64,
    pub edge: f64,
    /// The most this arb can actually be taken for right now: the smaller
    /// of the two venues' top-of-book size. A placeholder `()` here used
    /// to leave a caller with no way to know how much of the arb was
    /// actually executable versus a one-lot quirk of the touch (design doc
    /// review 3.17).
    pub executable_size: f64,
}

#[derive(Default)]
struct ContractBook {
    by_venue: BTreeMap<VenueId, NormalizedTick>,
    /// Multi-level depth, keyed separately from `by_venue`'s top-of-book
    /// ticks because not every consumer needs it and not every venue
    /// adapter publishes it. Populated only when `update_depth` is
    /// called — top-of-book alone (`update`) works exactly as before.
    depth_by_venue: BTreeMap<VenueId, BookDepth>,
}

/// Holds the latest known quote per (contract, venue), last-write-wins,
/// plus optional multi-level depth alongside it. Top-of-book (`update` /
/// `quotes` / `best_bid` / `best_ask`) is what most of PARALLAX consumes;
/// depth (`update_depth` / `walk_ask` / `walk_bid`) exists specifically
/// for APERTURE's tradable-edge calculator (design doc §5), which needs
/// to know what a given size actually costs to fill, not just the touch.
///
/// Backed by `BTreeMap`, not `HashMap`: `HashMap` iteration order is
/// randomized per process, so anything that scans venues (`detect_arb`,
/// `best_bid`/`best_ask` when prices tie) could return a different result
/// on byte-identical input between runs. A backtest that cannot reproduce
/// itself cannot accept or reject a strategy (design doc review 2.3).
#[derive(Default)]
pub struct ConsolidatedBook {
    contracts: BTreeMap<CanonicalContractId, ContractBook>,
    rejected_ticks: u64,
}

impl ConsolidatedBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rejects a tick that fails `NormalizedTick::validate()` — a NaN or
    /// out-of-range price would otherwise sit in the book comparing
    /// `false` against every later `>`/`<` check that reads it (design doc
    /// review 1.1) — and counts the rejection rather than silently
    /// dropping it. A rising `rejected_ticks()` means the feed shape
    /// changed and needs attention, not a swallowed error.
    pub fn update(&mut self, tick: NormalizedTick) {
        if tick.validate().is_err() {
            self.rejected_ticks += 1;
            return;
        }
        let entry = self.contracts.entry(tick.contract.clone()).or_default();
        entry.by_venue.insert(tick.venue, tick);
    }

    pub fn rejected_ticks(&self) -> u64 {
        self.rejected_ticks
    }

    pub fn update_depth(&mut self, depth: BookDepth) {
        let entry = self.contracts.entry(depth.contract.clone()).or_default();
        entry.depth_by_venue.insert(depth.venue, depth);
    }

    pub fn depth(&self, contract: &CanonicalContractId, venue: VenueId) -> Option<&BookDepth> {
        self.contracts.get(contract)?.depth_by_venue.get(&venue)
    }

    /// Volume-weighted average price to buy `target_size` on `venue`,
    /// walking that venue's resting ask depth rather than trusting the
    /// top-of-book price alone — `ExpectedAvgEntry` in the APERTURE edge
    /// calculator. `None` if there's no depth snapshot for this
    /// venue/contract yet, or the book can't fill any of the target size.
    pub fn walk_ask(
        &self,
        contract: &CanonicalContractId,
        venue: VenueId,
        target_size: f64,
    ) -> Option<WalkResult> {
        self.depth(contract, venue)?.walk_asks(target_size)
    }

    pub fn walk_bid(
        &self,
        contract: &CanonicalContractId,
        venue: VenueId,
        target_size: f64,
    ) -> Option<WalkResult> {
        self.depth(contract, venue)?.walk_bids(target_size)
    }

    pub fn quotes(&self, contract: &CanonicalContractId) -> impl Iterator<Item = &NormalizedTick> {
        self.contracts
            .get(contract)
            .into_iter()
            .flat_map(|b| b.by_venue.values())
    }

    pub fn best_bid(&self, contract: &CanonicalContractId) -> Option<BestQuote> {
        self.quotes(contract)
            .max_by(|a, b| a.bid.total_cmp(&b.bid))
            .map(|t| BestQuote {
                venue: t.venue,
                price: t.bid,
                size: t.bid_size,
            })
    }

    pub fn best_ask(&self, contract: &CanonicalContractId) -> Option<BestQuote> {
        self.quotes(contract)
            .min_by(|a, b| a.ask.total_cmp(&b.ask))
            .map(|t| BestQuote {
                venue: t.venue,
                price: t.ask,
                size: t.ask_size,
            })
    }

    /// Midpoint between the best bid and best ask across *all* venues —
    /// the market-implied component that feeds into `FairValue` alongside
    /// the alpha ensemble (design doc §7).
    pub fn consolidated_mid(&self, contract: &CanonicalContractId) -> Option<f64> {
        let bid = self.best_bid(contract)?;
        let ask = self.best_ask(contract)?;
        Some((bid.price + ask.price) / 2.0)
    }

    /// Scans every venue pair for the same contract and returns the
    /// largest riskless edge, if any venue's ask undercuts another
    /// venue's bid. `None` means the market is internally consistent
    /// (or under-observed).
    pub fn detect_arb(&self, contract: &CanonicalContractId) -> Option<CrossVenueArb> {
        let quotes: Vec<&NormalizedTick> = self.quotes(contract).collect();
        let mut best: Option<CrossVenueArb> = None;
        for buy in &quotes {
            for sell in &quotes {
                if buy.venue == sell.venue {
                    continue;
                }
                let edge = sell.bid - buy.ask;
                if edge > 0.0 && best.as_ref().map(|b| edge > b.edge).unwrap_or(true) {
                    best = Some(CrossVenueArb {
                        contract: contract.clone(),
                        buy_venue: buy.venue,
                        buy_price: buy.ask,
                        sell_venue: sell.venue,
                        sell_price: sell.bid,
                        edge,
                        executable_size: buy.ask_size.min(sell.bid_size),
                    });
                }
            }
        }
        best
    }

    /// Drops venue quotes older than `max_age_ns` relative to `now`, and
    /// removes any contract entry left with no venues and no depth at all
    /// — otherwise a venue listing thousands of short-dated markets a day
    /// leaves an ever-growing set of emptied-out entries behind as each
    /// one expires (design doc review 3.6). The risk engine calls this (or
    /// an equivalent staleness check) before trusting the book — a quote
    /// from a disconnected venue must not silently keep influencing the
    /// consolidated mid.
    pub fn prune_stale(&mut self, now: Timestamp, max_age_ns: i64) {
        self.contracts.retain(|_, book| {
            book.by_venue
                .retain(|_, tick| now.since(tick.receive_ts) <= max_age_ns);
            book.depth_by_venue
                .retain(|_, depth| now.since(depth.receive_ts) <= max_age_ns);
            !book.by_venue.is_empty() || !book.depth_by_venue.is_empty()
        });
    }

    /// Number of distinct contracts currently tracked — exposed mainly so
    /// `prune_stale`'s empty-entry cleanup is externally observable/testable.
    pub fn tracked_contracts(&self) -> usize {
        self.contracts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::CanonicalContractSpec;

    fn contract() -> CanonicalContractId {
        CanonicalContractSpec {
            event_class: parallax_types::EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: 869,
            direction: parallax_types::Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        }
        .to_id()
    }

    fn tick(venue: VenueId, bid: f64, ask: f64) -> NormalizedTick {
        NormalizedTick {
            venue,
            contract: contract(),
            bid,
            bid_size: 100.0,
            ask,
            ask_size: 100.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        }
    }

    #[test]
    fn consolidated_mid_uses_best_of_both_venues() {
        let mut book = ConsolidatedBook::new();
        book.update(tick(VenueId::Polymarket, 0.60, 0.63));
        book.update(tick(VenueId::Kalshi, 0.65, 0.70));
        // best bid is Kalshi's 0.65, best ask is Polymarket's 0.63
        assert_eq!(book.best_bid(&contract()).unwrap().price, 0.65);
        assert_eq!(book.best_ask(&contract()).unwrap().price, 0.63);
        assert_eq!(book.consolidated_mid(&contract()).unwrap(), 0.64);
    }

    #[test]
    fn detects_riskless_cross_venue_arb() {
        let mut book = ConsolidatedBook::new();
        // Polymarket ask 0.60 is cheaper than Kalshi bid 0.65 -> buy Polymarket, sell Kalshi
        book.update(tick(VenueId::Polymarket, 0.55, 0.60));
        book.update(tick(VenueId::Kalshi, 0.65, 0.72));
        let arb = book
            .detect_arb(&contract())
            .expect("arb should be detected");
        assert_eq!(arb.buy_venue, VenueId::Polymarket);
        assert_eq!(arb.sell_venue, VenueId::Kalshi);
        assert_eq!(arb.contract, contract());
        assert_eq!(arb.executable_size, 100.0);
        assert!((arb.edge - 0.05).abs() < 1e-9);
    }

    #[test]
    fn no_arb_when_books_are_consistent() {
        let mut book = ConsolidatedBook::new();
        book.update(tick(VenueId::Polymarket, 0.60, 0.66));
        book.update(tick(VenueId::Kalshi, 0.61, 0.65));
        assert!(book.detect_arb(&contract()).is_none());
    }

    #[test]
    fn walk_ask_uses_the_named_venues_depth_not_top_of_book() {
        use parallax_types::{BookDepth, DepthLevel};
        let mut book = ConsolidatedBook::new();
        book.update_depth(BookDepth {
            venue: VenueId::Polymarket,
            contract: contract(),
            bids: vec![DepthLevel {
                price: 0.58,
                size: 20.0,
            }],
            asks: vec![
                DepthLevel {
                    price: 0.60,
                    size: 40.0,
                },
                DepthLevel {
                    price: 0.63,
                    size: 60.0,
                },
            ],
            receive_ts: Timestamp::from_nanos(0),
        });

        let result = book
            .walk_ask(&contract(), VenueId::Polymarket, 70.0)
            .unwrap();
        assert_eq!(result.filled_size, 70.0);
        let expected = (40.0 * 0.60 + 30.0 * 0.63) / 70.0;
        assert!((result.avg_price - expected).abs() < 1e-9);

        // No depth published for Kalshi -> None, not a fallback to Polymarket's.
        assert!(book.walk_ask(&contract(), VenueId::Kalshi, 10.0).is_none());
    }

    #[test]
    fn prune_stale_also_drops_stale_depth() {
        use parallax_types::{BookDepth, DepthLevel};
        let mut book = ConsolidatedBook::new();
        book.update_depth(BookDepth {
            venue: VenueId::Polymarket,
            contract: contract(),
            bids: vec![],
            asks: vec![DepthLevel {
                price: 0.6,
                size: 10.0,
            }],
            receive_ts: Timestamp::from_nanos(0),
        });
        book.prune_stale(Timestamp::from_nanos(10_000_000_000), 1_000_000_000);
        assert!(book.depth(&contract(), VenueId::Polymarket).is_none());
    }

    #[test]
    fn prune_stale_removes_old_venue_quotes_only() {
        let mut book = ConsolidatedBook::new();
        let mut fresh = tick(VenueId::Polymarket, 0.6, 0.62);
        fresh.receive_ts = Timestamp::from_nanos(1_000_000_000);
        let mut stale = tick(VenueId::Kalshi, 0.6, 0.62);
        stale.receive_ts = Timestamp::from_nanos(0);
        book.update(fresh);
        book.update(stale);

        book.prune_stale(Timestamp::from_nanos(1_000_000_000), 500_000_000);
        let remaining: Vec<_> = book.quotes(&contract()).map(|t| t.venue).collect();
        assert_eq!(remaining, vec![VenueId::Polymarket]);
    }

    #[test]
    fn prune_stale_removes_the_contract_entry_entirely_once_empty() {
        let mut book = ConsolidatedBook::new();
        book.update(tick(VenueId::Polymarket, 0.6, 0.62));
        assert_eq!(book.tracked_contracts(), 1);

        book.prune_stale(Timestamp::from_nanos(10_000_000_000), 1);
        assert_eq!(
            book.tracked_contracts(),
            0,
            "an emptied-out contract entry must not linger forever"
        );
    }

    #[test]
    fn a_nan_tick_is_rejected_and_counted_instead_of_entering_the_book() {
        let mut book = ConsolidatedBook::new();
        let mut bad = tick(VenueId::Polymarket, f64::NAN, 0.60);
        bad.contract = contract();
        book.update(bad);
        assert_eq!(book.rejected_ticks(), 1);
        assert!(book.quotes(&contract()).next().is_none());
    }

    #[test]
    fn a_valid_tick_after_a_rejected_one_is_still_accepted() {
        let mut book = ConsolidatedBook::new();
        book.update(tick(VenueId::Polymarket, f64::NAN, 0.60));
        book.update(tick(VenueId::Polymarket, 0.55, 0.60));
        assert_eq!(book.rejected_ticks(), 1);
        assert_eq!(book.quotes(&contract()).count(), 1);
    }
}
