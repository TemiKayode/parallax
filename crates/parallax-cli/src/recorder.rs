//! `docs/GOING-LIVE.md` Stage 0: "Record the websocket feed now, in
//! parallel with everything else — it costs nothing and you cannot
//! backfill it later." This repo has no websocket client, so this
//! records real book snapshots over the same public REST endpoints
//! `parallax-ui`'s live-quotes panel already reads from — lower
//! frequency and lower fidelity than a real streaming feed, but real
//! data with real timestamps, not synthetic noise. Every line it writes
//! is a `{"tick": {...}}` JSONL record `parallax_sim::replay::load_jsonl`
//! already knows how to read back for a future backtest.
//!
//! Read-only: builds `KalshiAdapter`/`PolymarketAdapter` with the
//! `Unconfigured*Signer`s that refuse every signing request, and never
//! calls `submit`. No credentials required — both endpoints are public.

use parallax_types::{CanonicalContractId, NormalizedTick, Timestamp, VenueId};
use parallax_venues::{
    parse_kalshi_orderbook, parse_polymarket_book, KalshiAdapter, PolymarketAdapter,
    SymbolRegistry, UnconfiguredKalshiSigner, UnconfiguredPolymarketSigner,
};
use std::fs::File;
use std::io::Write;
use std::sync::Arc;

/// Fetches one real book snapshot from Kalshi's `KXHIGHCHI` series — the
/// same discovery-with-fallback approach as `parallax-ui`'s live-quotes
/// panel (`live.rs`): several candidate markets are open at once, and a
/// momentarily one-sided book on any single one of them is routine, not
/// an error to surface on the first try.
pub async fn fetch_kalshi_tick() -> Result<NormalizedTick, String> {
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
        let book = match adapter.fetch_orderbook_raw(ticker).await {
            Ok(b) => b,
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        match parse_kalshi_orderbook(&book) {
            Ok((bid, bid_size, ask, ask_size)) => {
                return Ok(NormalizedTick {
                    venue: VenueId::Kalshi,
                    // A raw, venue-native identifier, not a canonically
                    // normalized cross-venue contract id — normalizing a
                    // recorded corpus is a separate, later concern from
                    // getting honest real data with real timestamps onto
                    // disk in the first place.
                    contract: CanonicalContractId(format!("raw.kalshi.{ticker}")),
                    bid,
                    bid_size,
                    ask,
                    ask_size,
                    venue_ts: None,
                    receive_ts: Timestamp::now(),
                });
            }
            Err(e) => last_err = e,
        }
    }
    Err(format!(
        "none of the open KXHIGHCHI markets had a two-sided book (last error: {last_err})"
    ))
}

/// Fetches one real book snapshot from the highest-24h-volume active
/// Polymarket market's first outcome token — same approach and same
/// caveat as `parallax-ui`'s live-quotes panel: this is not a contract
/// matched to the Kalshi snapshot above, it is real, independent
/// connectivity to whatever is actually trading right now.
pub async fn fetch_polymarket_tick() -> Result<NormalizedTick, String> {
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

        for token_id in &token_ids {
            let book = match adapter.fetch_book_raw(token_id).await {
                Ok(b) => b,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            };
            match parse_polymarket_book(&book) {
                Ok((bid, bid_size, ask, ask_size)) => {
                    return Ok(NormalizedTick {
                        venue: VenueId::Polymarket,
                        contract: CanonicalContractId(format!("raw.polymarket.{token_id}")),
                        bid,
                        bid_size,
                        ask,
                        ask_size,
                        venue_ts: None,
                        receive_ts: Timestamp::now(),
                    });
                }
                Err(e) => last_err = e,
            }
        }
    }
    Err(format!(
        "no active Polymarket outcome token had a two-sided book (last error: {last_err})"
    ))
}

/// Appends one tick to a JSONL recording, in the exact envelope
/// `parallax_sim::replay::load_jsonl` expects (`{"tick": {...}}`,
/// snake_case, externally tagged).
pub fn append_tick_jsonl(file: &mut File, tick: &NormalizedTick) -> std::io::Result<()> {
    let line = serde_json::json!({ "tick": tick });
    writeln!(file, "{line}")
}

/// One polling pass: fetch both venues, append whichever succeed. A
/// failed fetch is reported, not fatal — over a recording session run
/// for hours or days, a transient rate limit or a momentarily empty book
/// on one venue must not stop the other venue's recording, or the whole
/// session. Returns the actual tick on success, not just `()` — a
/// caller feeding these into a live `ConsolidatedBook` (`docs/GOING-LIVE.md`
/// Stage 4) needs it, not just a record that *something* was appended.
pub struct RecordAttempt {
    pub kalshi: Result<NormalizedTick, String>,
    pub polymarket: Result<NormalizedTick, String>,
}

pub async fn record_once(file: &mut File) -> RecordAttempt {
    let kalshi = match fetch_kalshi_tick().await {
        Ok(tick) => match append_tick_jsonl(file, &tick) {
            Ok(()) => Ok(tick),
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e),
    };
    let polymarket = match fetch_polymarket_tick().await {
        Ok(tick) => match append_tick_jsonl(file, &tick) {
            Ok(()) => Ok(tick),
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e),
    };
    RecordAttempt { kalshi, polymarket }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_sim::{parse_jsonl, ReplayEvent};

    #[test]
    fn an_appended_tick_round_trips_through_load_jsonl() {
        let tick = NormalizedTick {
            venue: VenueId::Kalshi,
            contract: CanonicalContractId("raw.kalshi.KXHIGHCHI-TEST".into()),
            bid: 0.61,
            bid_size: 120.0,
            ask: 0.64,
            ask_size: 80.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(123),
        };
        let mut path = std::env::temp_dir();
        path.push(format!(
            "parallax_recorder_test_{}.jsonl",
            std::process::id()
        ));
        {
            let mut file = File::create(&path).unwrap();
            append_tick_jsonl(&mut file, &tick).unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let events = parse_jsonl(&content).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ReplayEvent::Tick(parsed) => assert_eq!(parsed, &tick),
            ReplayEvent::Alpha(_) => panic!("expected a Tick event"),
        }
    }

    #[test]
    fn appended_lines_are_valid_single_line_jsonl() {
        let tick = NormalizedTick {
            venue: VenueId::Polymarket,
            contract: CanonicalContractId("raw.polymarket.abc123".into()),
            bid: 0.5,
            bid_size: 10.0,
            ask: 0.52,
            ask_size: 10.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        };
        let mut path = std::env::temp_dir();
        path.push(format!(
            "parallax_recorder_test2_{}.jsonl",
            std::process::id()
        ));
        {
            let mut file = File::create(&path).unwrap();
            append_tick_jsonl(&mut file, &tick).unwrap();
            append_tick_jsonl(&mut file, &tick).unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(content.lines().count(), 2);
        for line in content.lines() {
            assert!(serde_json::from_str::<serde_json::Value>(line).is_ok());
        }
    }
}
