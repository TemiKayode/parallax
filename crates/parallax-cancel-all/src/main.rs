//! The out-of-band cancel path itself (docs/GOING-LIVE.md Stage 2). Run
//! this — `cargo run -p parallax-cancel-all` — when the main system is
//! the problem and you need every resting order gone regardless of
//! whether the strategy process is even still running. Its isolation
//! from that process is structural, not just operational: see the crate
//! doc comment in `lib.rs` for exactly what that means.
//!
//! No live venue credentials are wired into this build yet — `submit`,
//! `cancel`, and the query methods this tool depends on
//! (`list_open_orders`) are still refused for `KalshiAdapter`/
//! `PolymarketAdapter` pending live verification (see their module docs).
//! Until then, this demonstrates the mechanism end to end against a
//! self-contained `PaperAdapter`: it places a couple of resting orders
//! itself, then cancels everything open, so the cancel-all path is
//! proven correct and ready the moment a real signer exists to point it
//! at a real venue.

#![forbid(unsafe_code)]

use parallax_cancel_all::cancel_all;
use parallax_types::{
    CanonicalContractId, EngineId, OrderIntent, OrderType, Outcome, Side, Timestamp, VenueId,
};
use parallax_venues::{PaperAdapter, VenueAdapter};

fn demo_order(contract: &CanonicalContractId, price: f64, size: f64) -> OrderIntent {
    OrderIntent {
        venue: VenueId::Paper,
        contract: contract.clone(),
        outcome: Outcome::Yes,
        side: Side::Buy,
        price,
        size,
        order_type: OrderType::Limit,
        engine: EngineId::MarketMaking,
        created_at: Timestamp::from_nanos(0),
    }
}

#[tokio::main]
async fn main() {
    println!("parallax-cancel-all");
    println!("A minimal, isolated cancel-all tool (docs/GOING-LIVE.md Stage 2).");
    println!(
        "This binary's dependency graph is parallax-types + parallax-venues only — no \
         parallax-strategy, parallax-risk, parallax-alpha, or parallax-sim. A bug in any \
         of those can never take this down.\n"
    );

    println!(
        "No live venue credentials are configured, so this demonstrates the mechanism \
         against a self-contained PaperAdapter: placing a couple of resting orders, then \
         canceling everything open.\n"
    );

    let venue = PaperAdapter::new();
    let contract = CanonicalContractId("wx.temp.chicago.gt_869.demo.nws_official".into());
    // A quote for the orders below to rest against without crossing.
    venue.advance_market(
        contract.clone(),
        0.40,
        100.0,
        0.70,
        100.0,
        Timestamp::from_nanos(0),
    );

    for (price, size) in [(0.45, 10.0), (0.50, 15.0), (0.55, 20.0)] {
        match venue.submit(demo_order(&contract, price, size)).await {
            Ok(ack) => println!("placed resting order {} @ {price:.2}", ack.order_id),
            Err(e) => eprintln!("failed to place demo order: {e}"),
        }
    }

    let before = venue
        .list_open_orders()
        .await
        .expect("PaperAdapter's own query never fails");
    println!("\n{} order(s) open before cancel-all.", before.len());

    match cancel_all(&venue).await {
        Ok(report) => {
            println!("\n--- cancel-all report ---");
            println!("attempted: {}", report.attempted);
            println!("canceled:  {}", report.canceled);
            if report.all_succeeded() {
                println!("every open order was canceled.");
            } else {
                println!("failed to cancel:");
                for (order_id, reason) in &report.failed {
                    println!("  {order_id} — {reason}");
                }
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("could not list open orders — nothing was attempted: {e}");
            std::process::exit(1);
        }
    }
}
