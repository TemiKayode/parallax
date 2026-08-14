use parallax_types::{CanonicalContractId, VenueId};

/// One rejection reason's occurrence count — the fastest diagnostic when
/// a strategy suddenly stops trading, since it tells you *why* rather
/// than just *that* (design doc review 3.22).
#[derive(Debug, Clone, PartialEq)]
pub struct RejectionCount {
    pub reason: String,
    pub count: u64,
}

#[derive(Debug, Default, Clone)]
pub struct BacktestReport {
    pub ticks_processed: u64,
    pub alpha_events_processed: u64,
    pub orders_proposed: u64,
    pub orders_rejected_by_risk: u64,
    pub orders_failed_submission: u64,
    pub fills: u64,
    pub filled_volume: f64,
    /// Sum of every fill's `risk_notional`-equivalent size — how much
    /// money actually changed hands' worth of exposure, not just how many
    /// contracts. A backtest report with only a fill count and no
    /// realized/fees/notional breakdown had exactly one number
    /// (unrealized mark-to-model) to show for an entire run (design doc
    /// review 3.22).
    pub gross_notional_traded: f64,
    /// Realized P&L across every closing/flipping fill.
    pub realized_pnl: f64,
    /// Mark-to-model unrealized P&L on every non-flat position at the end
    /// of the run, using each contract's last consolidated mid.
    pub unrealized_pnl: f64,
    /// Total fees paid across every fill.
    pub fees_paid: f64,
    pub open_positions: Vec<(VenueId, CanonicalContractId, f64, f64)>,
    /// Equity (realized + unrealized + fees already deducted) sampled
    /// after every fill — what a drawdown chart is drawn from.
    pub equity_curve: Vec<f64>,
    /// The largest peak-to-trough drop in `equity_curve`, in the same
    /// units as P&L.
    pub max_drawdown: f64,
    pub rejection_histogram: Vec<RejectionCount>,
    /// Set once `PipelineBus::integrity_violated()` (or the equivalent
    /// bus-drop signal) has fired during this run — the position book is
    /// known-wrong from that point on, so `headline()` refuses to quote a
    /// P&L number a reader could mistake for a real one (design doc
    /// review 1.9).
    pub bus_integrity_violated: bool,
}

impl BacktestReport {
    pub fn gross_pnl(&self) -> f64 {
        self.realized_pnl + self.unrealized_pnl
    }

    pub fn net_pnl(&self) -> f64 {
        self.gross_pnl() - self.fees_paid
    }

    pub fn record_rejection(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        match self
            .rejection_histogram
            .iter_mut()
            .find(|r| r.reason == reason)
        {
            Some(existing) => existing.count += 1,
            None => self
                .rejection_histogram
                .push(RejectionCount { reason, count: 1 }),
        }
        self.orders_rejected_by_risk += 1;
    }

    /// Appends `equity` to the equity curve and updates `max_drawdown`
    /// from the running peak.
    pub fn record_equity(&mut self, equity: f64) {
        let peak = self
            .equity_curve
            .iter()
            .cloned()
            .fold(f64::MIN, f64::max)
            .max(equity);
        let drawdown = peak - equity;
        if drawdown > self.max_drawdown {
            self.max_drawdown = drawdown;
        }
        self.equity_curve.push(equity);
    }

    /// A blunt, one-line summary — deliberately conservative: it refuses
    /// to quote a P&L at all once the bus integrity has been violated,
    /// because every position/fill number since is potentially wrong in
    /// a way nothing downstream can detect on its own (design doc review
    /// 1.9).
    pub fn headline(&self) -> String {
        if self.bus_integrity_violated {
            return "INTEGRITY VIOLATED: a critical bus topic dropped an order ack during this run — the position book, and therefore every P&L number below, cannot be trusted.".to_string();
        }
        format!(
            "{} fills, {:.4} gross / {:.4} net PnL ({:.4} fees), {:.4} max drawdown",
            self.fills,
            self.gross_pnl(),
            self.net_pnl(),
            self.fees_paid,
            self.max_drawdown
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_pnl_subtracts_fees_from_gross() {
        let report = BacktestReport {
            realized_pnl: 10.0,
            unrealized_pnl: 2.0,
            fees_paid: 3.0,
            ..Default::default()
        };
        assert!((report.gross_pnl() - 12.0).abs() < 1e-9);
        assert!((report.net_pnl() - 9.0).abs() < 1e-9);
    }

    #[test]
    fn record_rejection_aggregates_by_reason() {
        let mut report = BacktestReport::default();
        report.record_rejection("FeedStale");
        report.record_rejection("FeedStale");
        report.record_rejection("ContractLimitExceeded");
        assert_eq!(report.orders_rejected_by_risk, 3);
        let stale = report
            .rejection_histogram
            .iter()
            .find(|r| r.reason == "FeedStale")
            .unwrap();
        assert_eq!(stale.count, 2);
    }

    #[test]
    fn record_equity_tracks_the_running_max_drawdown() {
        let mut report = BacktestReport::default();
        report.record_equity(100.0);
        report.record_equity(120.0); // new peak
        report.record_equity(90.0); // 30 drawdown from peak
        report.record_equity(110.0); // recovers, drawdown stays at 30
        assert!((report.max_drawdown - 30.0).abs() < 1e-9);
    }

    #[test]
    fn headline_refuses_to_quote_pnl_once_integrity_is_violated() {
        let report = BacktestReport {
            bus_integrity_violated: true,
            realized_pnl: 1000.0,
            ..Default::default()
        };
        assert!(report.headline().contains("INTEGRITY VIOLATED"));
        assert!(!report.headline().contains("1000"));
    }
}
