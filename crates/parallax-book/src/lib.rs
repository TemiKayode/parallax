//! The consolidated cross-venue order book (design doc §6): one canonical
//! contract maps to many venues' independent quotes, and this crate is
//! where "many venues" collapses into "one view" — both the ordinary
//! consolidated mid used to center market-making quotes, and direct
//! cross-venue arbitrage detection (ask on one venue crossing the bid on
//! another for the *identical* canonical contract, no model required).

use parallax_types::{CanonicalContractId, NormalizedTick, Timestamp, VenueId};
use std::collections::HashMap;

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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrossVenueArb {
    pub contract_hint: (),
    pub buy_venue: VenueId,
    pub buy_price: f64,
    pub sell_venue: VenueId,
    pub sell_price: f64,
    pub edge: f64,
}

#[derive(Default)]
struct ContractBook {
    by_venue: HashMap<VenueId, NormalizedTick>,
}

/// Holds the latest known quote per (contract, venue). Deliberately last-
/// write-wins per venue rather than a full depth book: PARALLAX trades
/// prediction-market top-of-book liquidity, and modeling full depth per
/// venue would add state the strategy core doesn't currently consume.
#[derive(Default)]
pub struct ConsolidatedBook {
    contracts: HashMap<CanonicalContractId, ContractBook>,
}

impl ConsolidatedBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, tick: NormalizedTick) {
        let entry = self.contracts.entry(tick.contract.clone()).or_default();
        entry.by_venue.insert(tick.venue, tick);
    }

    pub fn quotes(&self, contract: &CanonicalContractId) -> impl Iterator<Item = &NormalizedTick> {
        self.contracts
            .get(contract)
            .into_iter()
            .flat_map(|b| b.by_venue.values())
    }

    pub fn best_bid(&self, contract: &CanonicalContractId) -> Option<BestQuote> {
        self.quotes(contract)
            .max_by(|a, b| a.bid.partial_cmp(&b.bid).unwrap())
            .map(|t| BestQuote {
                venue: t.venue,
                price: t.bid,
                size: t.bid_size,
            })
    }

    pub fn best_ask(&self, contract: &CanonicalContractId) -> Option<BestQuote> {
        self.quotes(contract)
            .min_by(|a, b| a.ask.partial_cmp(&b.ask).unwrap())
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
                if edge > 0.0 && best.map(|b| edge > b.edge).unwrap_or(true) {
                    best = Some(CrossVenueArb {
                        contract_hint: (),
                        buy_venue: buy.venue,
                        buy_price: buy.ask,
                        sell_venue: sell.venue,
                        sell_price: sell.bid,
                        edge,
                    });
                }
            }
        }
        best
    }

    /// Drops venue quotes older than `max_age_ns` relative to `now`. The
    /// risk engine calls this (or an equivalent staleness check) before
    /// trusting the book — a quote from a disconnected venue must not
    /// silently keep influencing the consolidated mid.
    pub fn prune_stale(&mut self, now: Timestamp, max_age_ns: i64) {
        for book in self.contracts.values_mut() {
            book.by_venue
                .retain(|_, tick| now.since(tick.receive_ts) <= max_age_ns);
        }
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
}
