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
/// order book. Requests several candidate markets and tries each in
/// order rather than betting everything on the first one the API
/// happens to return — a real listing can have an empty book between
/// observations even while the market itself is open (same rationale as
/// `fetch_live_polymarket` below).
pub async fn fetch_live_kalshi() -> Result<LiveQuote, String> {
    let adapter = KalshiAdapter::new(
        Arc::new(UnconfiguredKalshiSigner),
        Arc::new(SymbolRegistry::new()),
    );

    let markets = adapter
        .fetch_open_markets_for_series_raw("KXHIGHCHI", 5)
        .await
        .map_err(|e| e.to_string())?;
    let markets_arr = markets
        .get("markets")
        .and_then(|m| m.as_array())
        .filter(|a| !a.is_empty())
        .ok_or("no open KXHIGHCHI markets right now")?;

    let mut last_err = "no open KXHIGHCHI markets had a usable ticker".to_string();

    for market in markets_arr {
        let Some(ticker) = market.get("ticker").and_then(|v| v.as_str()) else {
            continue;
        };
        let title = market
            .get("title")
            .or_else(|| market.get("subtitle"))
            .and_then(|v| v.as_str())
            .unwrap_or(ticker)
            .to_string();
        let close_time = market
            .get("close_time")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let book = match adapter.fetch_orderbook_raw(ticker).await {
            Ok(b) => b,
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        match parse_kalshi_orderbook(&book) {
            Ok((bid, bid_size, ask, ask_size)) => {
                return Ok(LiveQuote {
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
                });
            }
            Err(e) => last_err = e,
        }
    }

    Err(format!(
        "none of the open KXHIGHCHI markets had a two-sided book (last error: {last_err})"
    ))
}

/// Discovers the highest-24h-volume active market on Polymarket and
/// fetches its live order book for the first outcome token. Not a
/// contract-matched pair with the Kalshi quote above — Polymarket
/// doesn't currently list a directly equivalent Chicago-temperature
/// market, so this shows real connectivity to a real, live, independent
/// market rather than forcing a misleading "same contract" comparison.
/// Picking "the first token of the first high-volume market" is fragile:
/// a real, currently-active market can easily have a thin or momentarily
/// one-sided book on one of its two outcome tokens (no resting ask, no
/// resting bid) even while it's genuinely the highest-24h-volume listing.
/// That used to surface as a hard "no ask levels" error on every refresh
/// for whichever market the Gamma API happened to return first. This
/// tries every token of every returned candidate market, in order, and
/// only gives up once none of them yield a genuinely two-sided book.
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

    let mut candidates_tried = 0u32;
    let mut last_err = "no active market with tradeable outcome tokens was returned".to_string();

    for market in markets_arr {
        let Some(token_ids_raw) = market.get("clobTokenIds").and_then(|v| v.as_str()) else {
            continue;
        };
        if token_ids_raw.is_empty() || token_ids_raw == "[]" {
            continue;
        }
        let Ok(token_ids) = serde_json::from_str::<Vec<String>>(token_ids_raw) else {
            last_err = format!("market had unparseable clobTokenIds: {token_ids_raw}");
            continue;
        };
        let question = market
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("Polymarket market")
            .to_string();

        for token_id in &token_ids {
            candidates_tried += 1;
            let book = match adapter.fetch_book_raw(token_id).await {
                Ok(b) => b,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            };
            match parse_polymarket_book(&book) {
                Ok((bid, bid_size, ask, ask_size)) => {
                    return Ok(LiveQuote {
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
                    });
                }
                Err(e) => last_err = e,
            }
        }
    }

    Err(format!(
        "none of {candidates_tried} candidate outcome tokens across the top active markets had a two-sided book (last error: {last_err})"
    ))
}
