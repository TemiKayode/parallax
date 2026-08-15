//! `docs/GOING-LIVE.md` Stage 0: "record now, in parallel with
//! everything else — it costs nothing and you cannot backfill it
//! later." Polls Kalshi's and Polymarket's real public market-data
//! endpoints on an interval and appends normalized book snapshots to a
//! JSONL file, in the exact format `parallax_sim::load_jsonl` reads back
//! for a backtest replay. Read-only: no order is ever placed, and no
//! credentials are required.
//!
//! Usage: `cargo run -p parallax-cli --bin parallax-record`
//! Configurable via env vars:
//!   PARALLAX_RECORD_OUTPUT          default: recordings/venue_ticks.jsonl
//!   PARALLAX_RECORD_INTERVAL_SECS   default: 10

#![forbid(unsafe_code)]

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

    println!(
        "Recording real Kalshi/Polymarket order-book snapshots to {}",
        path.display()
    );
    println!(
        "Polling every {}s. Ctrl+C to stop. Read-only — no order is ever placed, no credentials required.",
        interval.as_secs()
    );
    println!(
        "Replay this file later with parallax_sim::load_jsonl (see docs/GOING-LIVE.md, Stage 0).\n"
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
                    Ok(()) => kalshi_ok += 1,
                    Err(e) => {
                        kalshi_err += 1;
                        eprintln!("kalshi fetch failed: {e}");
                    }
                }
                match attempt.polymarket {
                    Ok(()) => polymarket_ok += 1,
                    Err(e) => {
                        polymarket_err += 1;
                        eprintln!("polymarket fetch failed: {e}");
                    }
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
