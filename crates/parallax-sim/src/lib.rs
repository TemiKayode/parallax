//! The replay/backtest harness (design doc §15): loads a chronological
//! JSONL corpus of ticks and alpha events and drives the exact same
//! `parallax-book` / `parallax-alpha` / `parallax-risk` / `parallax-strategy`
//! types a live deployment would use, against the in-memory `PaperAdapter`
//! from `parallax-venues`. Swapping `PaperAdapter` for a live `VenueAdapter`
//! is the only thing that changes to go from backtest to live — that's
//! the payoff of the trait boundaries built into every other crate.

mod engine;
mod replay;
mod report;

pub use engine::Backtest;
pub use replay::{load_jsonl, parse_jsonl, ReplayEvent};
pub use report::BacktestReport;
