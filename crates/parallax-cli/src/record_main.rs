//! `docs/GOING-LIVE.md` Stages 0, 1, and 4 together: this records
//! (Stage 0 — "record now, you cannot backfill it later"), checks clock
//! discipline (Stage 1 — "NTP, monitored, with an alert on drift"), and
//! monitors feed health (Stage 4 — continuous real-feed ingestion at zero
//! financial risk), all off the same live polling loop, since they're
//! complementary, not competing uses of the same connectivity.
//!
//! Polls Kalshi's and Polymarket's real public market-data endpoints on
//! an interval, appends normalized book snapshots to a JSONL file (in
//! the exact format `parallax_sim::load_jsonl` reads back for a backtest
//! replay), feeds each successful tick into a real `ConsolidatedBook`
//! (exercising the same validation a live deployment's book would run),
//! tracks per-venue consecutive-failure streaks
//! (`parallax_cli::FeedHealthMonitor`), and checks this process's clock
//! against each venue's real HTTP `Date` header
//! (`parallax_venues::ClockSkewMonitor`) — both alerting once a streak
//! crosses its threshold rather than on the first transient blip or
//! reading.
//!
//! What this is **not**: a live paper-trading loop in the full Stage 4
//! sense. There is no live alpha source in this repo, and Kalshi's real
//! `KXHIGHCHI` listing and Polymarket's real top-volume market are two
//! different real-world events with no shared canonical contract id —
//! see `feed_health.rs`'s module doc for why running a strategy against
//! this feed isn't something this binary claims to do. Nor is the clock
//! check a substitute for real NTP on the host — it only proves this
//! specific process's clock is (or isn't) sane relative to two venues it
//! actually talks to, which is the half of "NTP, monitored" that's
//! verifiable from inside this repo.
//!
//! Read-only: no order is ever placed, and no credentials are required.
//!
//! Usage: `cargo run -p parallax-cli --bin parallax-record`
//! Configurable via env vars:
//!   PARALLAX_RECORD_OUTPUT             default: recordings/venue_ticks.jsonl
//!   PARALLAX_RECORD_INTERVAL_SECS      default: 10
//!   PARALLAX_RECORD_HALT_AFTER_FAILS   default: 5 (consecutive failures before a health alert)
//!   PARALLAX_RECORD_MAX_SKEW_MS        default: 2000 (tolerance before a reading counts as skew)
//!   PARALLAX_RECORD_SKEW_ALERT_AFTER   default: 3 (consecutive skewed readings before an alert)

#![forbid(unsafe_code)]

use parallax_book::ConsolidatedBook;
use parallax_cli::FeedHealthMonitor;
use parallax_types::{Timestamp, VenueId};
use parallax_venues::ClockSkewMonitor;
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

/// `docs/GOING-LIVE.md` Stage 1: how far local and venue clocks may
/// diverge before it counts as skew rather than the `Date` header's own
/// one-second resolution plus ordinary network jitter. 2s is generous on
/// purpose — this alert exists to catch a clock that has actually drifted
/// (the class of bug that shows up as signed-request auth failures), not
/// to fire on routine latency variance.
fn max_skew_ms() -> i64 {
    std::env::var("PARALLAX_RECORD_MAX_SKEW_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(2_000)
}

/// Consecutive out-of-tolerance readings, against the same venue, before
/// `ClockSkewMonitor` alerts — same streak-not-blip reasoning as
/// `halt_after` above.
fn skew_alert_after() -> u32 {
    std::env::var("PARALLAX_RECORD_SKEW_ALERT_AFTER")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(3)
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
    let mut clock_skew = ClockSkewMonitor::new(max_skew_ms(), skew_alert_after());
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
         see this binary's module doc for exactly what that does and doesn't cover."
    );
    println!(
        "Also checking this process's clock against each venue's HTTP Date header every tick \
         (Stage 1 clock discipline) — alerting past {} consecutive readings more than {}ms off.\n",
        skew_alert_after(),
        max_skew_ms()
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

                match parallax_cli::fetch_kalshi_server_time().await {
                    Ok(remote) => {
                        if let Some(alert) =
                            clock_skew.record(VenueId::Kalshi, Timestamp::now(), remote)
                        {
                            eprintln!(
                                "*** CLOCK SKEW ALERT: kalshi off by {}ms for {} consecutive readings ***",
                                alert.skew_ms, alert.consecutive
                            );
                        }
                    }
                    Err(e) => eprintln!("kalshi clock check failed: {e}"),
                }
                match parallax_cli::fetch_polymarket_server_time().await {
                    Ok(remote) => {
                        if let Some(alert) =
                            clock_skew.record(VenueId::Polymarket, Timestamp::now(), remote)
                        {
                            eprintln!(
                                "*** CLOCK SKEW ALERT: polymarket off by {}ms for {} consecutive readings ***",
                                alert.skew_ms, alert.consecutive
                            );
                        }
                    }
                    Err(e) => eprintln!("polymarket clock check failed: {e}"),
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
