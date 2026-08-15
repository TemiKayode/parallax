//! `docs/GOING-LIVE.md` Stage 0 and Stage 4 together: this both records
//! (Stage 0 — "record now, you cannot backfill it later") and monitors
//! (Stage 4 — continuous real-feed ingestion at zero financial risk) the
//! same live polling loop, since they're complementary, not competing
//! uses of the same data.
//!
//! Polls Kalshi's and Polymarket's real public market-data endpoints on
//! an interval, appends normalized book snapshots to a JSONL file (in
//! the exact format `parallax_sim::load_jsonl` reads back for a backtest
//! replay), feeds each successful tick into a real `ConsolidatedBook`
//! (exercising the same validation a live deployment's book would run),
//! and tracks per-venue consecutive-failure streaks
//! (`parallax_cli::FeedHealthMonitor`), alerting once a streak crosses
//! the threshold rather than on the first transient blip.
//!
//! What this is **not**: a live paper-trading loop in the full Stage 4
//! sense. There is no live alpha source in this repo, and Kalshi's real
//! `KXHIGHCHI` listing and Polymarket's real top-volume market are two
//! different real-world events with no shared canonical contract id —
//! see `feed_health.rs`'s module doc for why running a strategy against
//! this feed isn't something this binary claims to do.
//!
//! Read-only: no order is ever placed, and no credentials are required.
//!
//! Usage: `cargo run -p parallax-cli --bin parallax-record`
//! Configurable via env vars:
//!   PARALLAX_RECORD_OUTPUT             default: recordings/venue_ticks.jsonl
//!   PARALLAX_RECORD_INTERVAL_SECS      default: 10
//!   PARALLAX_RECORD_HALT_AFTER_FAILS   default: 5 (consecutive failures before a health alert)

#![forbid(unsafe_code)]

use parallax_book::ConsolidatedBook;
use parallax_cli::FeedHealthMonitor;
use parallax_types::VenueId;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::Duration;

fn output_path() -> PathBuf {
    std::env::var("PARALLAX_RECORD_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("recordings/venue_ticks.jsonl"))
}

fn interval_secs() -> u64 {
    std::env::var("PARALLAX_RECORD_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(10)
}

fn halt_after() -> u32 {
    std::env::var("PARALLAX_RECORD_HALT_AFTER_FAILS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(5)
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let path = output_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let interval = Duration::from_secs(interval_secs());
    let mut health = FeedHealthMonitor::new(halt_after());
    let mut book = ConsolidatedBook::new();

    println!(
        "Recording real Kalshi/Polymarket order-book snapshots to {}",
        path.display()
    );
    println!(
        "Polling every {}s. Ctrl+C to stop. Read-only — no order is ever placed, no credentials required.",
        interval.as_secs()
    );
    println!(
        "Replay this file later with parallax_sim::load_jsonl (see docs/GOING-LIVE.md, Stage 0)."
    );
    println!(
        "Feeding each real tick into a live ConsolidatedBook and monitoring feed health (Stage 4) — \
         see this binary's module doc for exactly what that does and doesn't cover.\n"
    );

    let mut kalshi_ok = 0u64;
    let mut kalshi_err = 0u64;
    let mut polymarket_ok = 0u64;
    let mut polymarket_err = 0u64;

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut ticker = tokio::time::interval(interval);
    // The first `tick()` on a freshly-created interval fires immediately
    // rather than waiting a full period — exactly what we want here, so
    // the recorder writes its first snapshot right away instead of
    // sitting idle for the first interval.

    loop {
        tokio::select! {
            res = &mut ctrl_c => {
                match res {
                    Ok(()) => println!("\nShutdown signal received, stopping recorder..."),
                    Err(e) => eprintln!(
                        "\nwarning: could not wait on the Ctrl+C handler ({e}); stopping anyway"
                    ),
                }
                break;
            }
            _ = ticker.tick() => {
                let attempt = parallax_cli::record_once(&mut file).await;

                match attempt.kalshi {
                    Ok(tick) => {
                        kalshi_ok += 1;
                        book.update(tick);
                        if let Some(alert) = health.record(VenueId::Kalshi, true) {
                            eprintln!("unexpected: a success produced an alert: {alert:?}");
                        }
                    }
                    Err(e) => {
                        kalshi_err += 1;
                        eprintln!("kalshi fetch failed: {e}");
                        if let Some(alert) = health.record(VenueId::Kalshi, false) {
                            eprintln!(
                                "*** HEALTH ALERT: kalshi has failed {} consecutive fetches ***",
                                alert.consecutive_failures
                            );
                        }
                    }
                }
                match attempt.polymarket {
                    Ok(tick) => {
                        polymarket_ok += 1;
                        book.update(tick);
                        if let Some(alert) = health.record(VenueId::Polymarket, true) {
                            eprintln!("unexpected: a success produced an alert: {alert:?}");
                        }
                    }
                    Err(e) => {
                        polymarket_err += 1;
                        eprintln!("polymarket fetch failed: {e}");
                        if let Some(alert) = health.record(VenueId::Polymarket, false) {
                            eprintln!(
                                "*** HEALTH ALERT: polymarket has failed {} consecutive fetches ***",
                                alert.consecutive_failures
                            );
                        }
                    }
                }

                if let Some(alert) = parallax_sim::check_feed_data_quality(&book) {
                    eprintln!("*** DATA QUALITY ALERT: {alert:?} ***");
                }

                println!(
                    "recorded so far — kalshi: {kalshi_ok} ok / {kalshi_err} failed  ·  polymarket: {polymarket_ok} ok / {polymarket_err} failed"
                );
            }
        }
    }

    println!(
        "\nStopped. kalshi: {kalshi_ok} ok / {kalshi_err} failed. polymarket: {polymarket_ok} ok / {polymarket_err} failed."
    );
    println!("Recording saved to {}", path.display());
    Ok(())
}
