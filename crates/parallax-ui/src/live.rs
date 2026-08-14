//! Real, live market data — read-only. Every function here makes an
//! actual HTTP call to Kalshi's or Polymarket's public, unauthenticated
//! market-data endpoints and normalizes the response with the same
//! parsers `parallax-venues` ships and tests against fixtures
//! (`parse_kalshi_orderbook`, `parse_polymarket_book`). Nothing in this
//! module can place an order — both adapters are constructed with the
//! `Unconfigured*Signer` that refuses every signing request, and neither
//! function here ever calls `submit`.

use parallax_venues::{
    parse_kalshi_orderbook, parse_polymarket_book, KalshiAdapter, PolymarketAdapter,
    SymbolRegistry, UnconfiguredKalshiSigner, UnconfiguredPolymarketSigner,
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
pub struct LiveQuote {
    pub venue: String,
    pub label: String,
    pub detail: String,
    pub bid: f64,
    pub bid_size: f64,
    pub ask: f64,
    pub ask_size: f64,
    pub source_url: String,
}

/// Discovers a currently-open market in Kalshi's real `KXHIGHCHI` series
/// ("Highest temperature in Chicago" — the same underlying event this
/// repo's synthetic demo scenario is modeled on) and fetches its live
/// order book.
pub async fn fetch_live_kalshi() -> Result<LiveQuote, String> {
    let adapter = KalshiAdapter::new(
        Arc::new(UnconfiguredKalshiSigner),
        Arc::new(SymbolRegistry::new()),
    );

    let markets = adapter
        .fetch_open_markets_for_series_raw("KXHIGHCHI", 1)
        .await
        .map_err(|e| e.to_string())?;
    let market = markets
        .get("markets")
        .and_then(|m| m.as_array())
        .and_then(|a| a.first())
        .ok_or("no open KXHIGHCHI markets right now")?;

    let ticker = market
        .get("ticker")
        .and_then(|v| v.as_str())
        .ok_or("market missing a ticker")?
        .to_string();
    let title = market
        .get("title")
        .or_else(|| market.get("subtitle"))
        .and_then(|v| v.as_str())
        .unwrap_or(&ticker)
        .to_string();
    let close_time = market
        .get("close_time")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let book = adapter
        .fetch_orderbook_raw(&ticker)
        .await
        .map_err(|e| e.to_string())?;
    let (bid, bid_size, ask, ask_size) = parse_kalshi_orderbook(&book)?;

    Ok(LiveQuote {
        venue: "kalshi".into(),
        label: title,
        detail: format!("{ticker} · closes {close_time}"),
        bid,
        bid_size,
        ask,
        ask_size,
        source_url: format!(
            "https://external-api.kalshi.com/trade-api/v2/markets/{ticker}/orderbook"
        ),
    })
}

/// Discovers the highest-24h-volume active market on Polymarket and
/// fetches its live order book for the first outcome token. Not a
/// contract-matched pair with the Kalshi quote above — Polymarket
/// doesn't currently list a directly equivalent Chicago-temperature
/// market, so this shows real connectivity to a real, live, independent
/// market rather than forcing a misleading "same contract" comparison.
pub async fn fetch_live_polymarket() -> Result<LiveQuote, String> {
    let adapter = PolymarketAdapter::new(
        Arc::new(UnconfiguredPolymarketSigner),
        Arc::new(SymbolRegistry::new()),
    );

    let markets = adapter
        .fetch_active_markets_raw(10)
        .await
        .map_err(|e| e.to_string())?;
    let markets_arr = markets
        .as_array()
        .ok_or("expected an array of markets from the Gamma API")?;

    let market = markets_arr
        .iter()
        .find(|m| {
            m.get("clobTokenIds")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty() && s != "[]")
        })
        .ok_or("no active market with tradeable outcome tokens found")?;

    let question = market
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("Polymarket market")
        .to_string();
    let token_ids_raw = market
        .get("clobTokenIds")
        .and_then(|v| v.as_str())
        .ok_or("market missing clobTokenIds")?;
    let token_ids: Vec<String> = serde_json::from_str(token_ids_raw)
        .map_err(|e| format!("could not parse clobTokenIds: {e}"))?;
    let token_id = token_ids.first().ok_or("clobTokenIds was empty")?;

    let book = adapter
        .fetch_book_raw(token_id)
        .await
        .map_err(|e| e.to_string())?;
    let (bid, bid_size, ask, ask_size) = parse_polymarket_book(&book)?;

    Ok(LiveQuote {
        venue: "polymarket".into(),
        label: question,
        detail: format!(
            "token {}…{}",
            &token_id[..6.min(token_id.len())],
            &token_id[token_id.len().saturating_sub(4)..]
        ),
        bid,
        bid_size,
        ask,
        ask_size,
        source_url: format!("https://clob.polymarket.com/book?token_id={token_id}"),
    })
}
