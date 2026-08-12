//! Venue adapters (design doc §9): the `VenueAdapter` plugin trait, the
//! in-memory `PaperAdapter` matching engine that backs both backtesting
//! and shadow mode, and the Kalshi/Polymarket adapters. The two live
//! adapters implement real, tested market-data normalization against
//! each venue's public book endpoint; order submission is scaffolded but
//! deliberately refuses to fire until a real request signer is
//! configured — see the reality check in design doc §2 and the module
//! docs on each adapter for what's been verified against current public
//! documentation versus what still needs confirmation before going live.

mod adapter;
mod kalshi;
mod paper;
mod polymarket;

pub use adapter::VenueAdapter;
pub use kalshi::{
    parse_orderbook as parse_kalshi_orderbook, KalshiAdapter, KalshiAuthHeaders,
    KalshiRequestSigner, UnconfiguredKalshiSigner,
};
pub use paper::PaperAdapter;
pub use polymarket::{
    parse_book as parse_polymarket_book, PolymarketAdapter, PolymarketOrderSigner,
    PolymarketSignedOrder, UnconfiguredPolymarketSigner,
};
