//! A runnable demonstration of the PARALLAX pipeline end to end, using
//! synthetic data (no live venue connections, no credentials). It exists
//! to make the architecture in the design doc concretely observable:
//! run `cargo run -p parallax-cli` (or the installed `parallax-demo`
//! binary), read what happened. The same scenario, via the same
//! `parallax_cli` library functions, powers `parallax-ui`'s web
//! dashboard — see that crate for a browser-based view of this.

#![forbid(unsafe_code)]

use parallax_cli::{run_demo_backtest, sample_arb};

fn section(title: &str) {
    println!("\n=== {title} ===");
}

#[tokio::main]
async fn main() {
    section("1. Direct cross-venue arbitrage detection (parallax-book)");
    {
        println!(
            "Polymarket: bid 0.55 / ask 0.60 (size 40)      Kalshi: bid 0.66 / ask 0.70 (size 30)"
        );
        match sample_arb(0.55, 0.60, 40.0, 0.66, 0.70, 30.0) {
            Some(arb) => println!(
                "  -> arb found: buy {:?} @ {:.2}, sell {:?} @ {:.2}, edge {:.2} per contract (no model required)",
                arb.buy_venue, arb.buy_price, arb.sell_venue, arb.sell_price, arb.edge
            ),
            None => println!("  -> no riskless arb (books are internally consistent)"),
        }
    }

    section("2. Backtest: weather update -> stale quote -> fill (parallax-sim)");
    {
        println!(
            "t=0    HRRR ensemble update: 5/5 members forecast > 86.9°F for Chicago on 2026-08-12"
        );
        println!("t=1ms  Polymarket still quoting bid 0.50 / ask 0.55 (stale, pre-update price)");
        println!(
            "t=2ms  Kalshi quoting bid 0.90 / ask 0.94 (already repriced toward the ensemble view)"
        );

        let report = run_demo_backtest().await;

        println!("\n--- backtest report ---");
        println!("ticks processed:            {}", report.ticks_processed);
        println!(
            "alpha events processed:     {}",
            report.alpha_events_processed
        );
        println!("orders proposed:            {}", report.orders_proposed);
        println!(
            "orders rejected by risk:    {}",
            report.orders_rejected_by_risk
        );
        println!(
            "orders failed at venue:     {}",
            report.orders_failed_submission
        );
        println!("fills:                      {}", report.fills);
        println!("filled volume:              {:.2}", report.filled_volume);
        println!(
            "unrealized PnL (mark-to-model): {:.4}",
            report.unrealized_pnl
        );
        println!("fees paid:                  {:.4}", report.fees_paid);
        println!("net PnL (after fees):       {:.4}", report.net_pnl());
        println!("max drawdown:               {:.4}", report.max_drawdown);
        if report.open_positions.is_empty() {
            println!("open positions:             none");
        } else {
            println!("open positions:");
            for (venue, contract, qty, avg_price) in &report.open_positions {
                println!(
                    "  {venue:?} {} qty={qty:.2} avg_price={avg_price:.4}",
                    contract.0
                );
            }
        }
    }

    println!("\nDone. This ran entirely against synthetic data and the in-memory PaperAdapter —");
    println!("no live venue connection, no credentials, no real order was submitted anywhere.");
    println!("\nWant a live view of this in a browser? Run: cargo run -p parallax-ui  (or the installed `parallax-ui` binary)");
}
