use parallax_types::{CanonicalContractId, VenueId};

#[derive(Debug, Default, Clone)]
pub struct BacktestReport {
    pub ticks_processed: u64,
    pub alpha_events_processed: u64,
    pub orders_proposed: u64,
    pub orders_rejected_by_risk: u64,
    pub orders_failed_submission: u64,
    pub fills: u64,
    pub filled_volume: f64,
    /// Mark-to-model unrealized P&L on every non-flat position at the end
    /// of the run, using each contract's last consolidated mid.
    pub unrealized_pnl: f64,
    pub open_positions: Vec<(VenueId, CanonicalContractId, f64, f64)>,
}
