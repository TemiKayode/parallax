//! `docs/GOING-LIVE.md` Stage 3, "alert on divergence, not on badness":
//!
//! > The useful alerts are: reconciliation mismatch; realised fee ≠
//! > modelled fee; rejection rate above baseline; feed staleness;
//! > realised fill rate diverging from the model's assumption. Each of
//! > these means a *model* is wrong, which is the failure that
//! > compounds.
//!
//! Every detector here inspects a report or piece of state this repo
//! already produces — it doesn't invent new telemetry, it's the missing
//! step of actually looking at telemetry that already existed. Feed
//! staleness in particular: `ConsolidatedBook::rejected_ticks()` was
//! already tracked, with a comment reading "surfaced via the report" next
//! to code that never surfaced it anywhere. This closes that gap, and
//! the two others (`reconcile_startup`'s report, `BacktestReport`'s
//! rejection histogram) that were sitting fully computed with nothing
//! reading them for a divergence signal.
//!
//! Fee-model verification and fill-rate divergence — the other two
//! bullets — live in `fee_verification` and are their own module; they
//! need per-fill state this module's report-level detectors don't.

use crate::reconcile::ReconciliationReport;
use crate::report::BacktestReport;
use parallax_book::ConsolidatedBook;

#[derive(Debug, Clone, PartialEq)]
pub enum Alert {
    /// `reconcile_startup` could not fully confirm every recovered order
    /// and/or fetch positions — the gate is refusing every order, as
    /// designed, but that fact needs to reach a human, not just sit
    /// correctly enforced in memory.
    ReconciliationMismatch {
        unresolved_orders: usize,
        detail: String,
    },
    /// `ConsolidatedBook` rejected one or more ticks outright (failed
    /// `NormalizedTick::validate()`) — the feed's shape changed, or
    /// started sending malformed records. Distinct from staleness below:
    /// this is about *validity*, not *age*.
    FeedDataRejected { rejected_ticks: u64 },
    /// One or more orders were rejected by `RejectReason::FeedStale` this
    /// run — the risk gate saw quotes older than
    /// `RiskLimits::max_feed_staleness_ns` and refused to trade on them.
    FeedStaleness { count: u64 },
    /// The realized rejection rate (`orders_rejected_by_risk /
    /// orders_proposed`) exceeded `baseline` by more than the caller's
    /// tolerance. `baseline` is supplied, not hardcoded — there's no
    /// single universally-correct rejection rate, only "higher than what
    /// this strategy/config combination normally sees."
    RejectionRateAboveBaseline { rate: f64, baseline: f64 },
}

/// `docs/GOING-LIVE.md` Stage 1's own rule made loud: "on disagreement,
/// adopt the venue's view, log the delta loudly, and stay flat until a
/// human has looked at it." `reconcile_startup` already stays flat
/// (refuses every order via `RejectReason::NotReconciled`) when it can't
/// fully confirm; this is the "log... loudly" half.
pub fn check_reconciliation(report: &ReconciliationReport) -> Option<Alert> {
    if report.gate_reconciled {
        return None;
    }
    let detail = if report.orders_unresolved.is_empty() {
        "position or open-order fetch failed".to_string()
    } else {
        report
            .orders_unresolved
            .iter()
            .map(|(id, reason)| format!("{id}: {reason}"))
            .collect::<Vec<_>>()
            .join("; ")
    };
    Some(Alert::ReconciliationMismatch {
        unresolved_orders: report.orders_unresolved.len(),
        detail,
    })
}

/// Checks `book`'s malformed-tick counter — see `Alert::FeedDataRejected`.
pub fn check_feed_data_quality(book: &ConsolidatedBook) -> Option<Alert> {
    let rejected = book.rejected_ticks();
    if rejected > 0 {
        Some(Alert::FeedDataRejected {
            rejected_ticks: rejected,
        })
    } else {
        None
    }
}

/// Checks a completed run's rejection histogram for `FeedStale` entries —
/// see `Alert::FeedStaleness`.
pub fn check_feed_staleness(report: &BacktestReport) -> Option<Alert> {
    let count: u64 = report
        .rejection_histogram
        .iter()
        .filter(|rc| rc.reason == "FeedStale")
        .map(|rc| rc.count)
        .sum();
    if count > 0 {
        Some(Alert::FeedStaleness { count })
    } else {
        None
    }
}

/// Checks a completed run's rejection rate against `baseline` — see
/// `Alert::RejectionRateAboveBaseline`. `tolerance` absorbs ordinary
/// run-to-run noise around the baseline; only a rate that clears
/// `baseline + tolerance` is a divergence worth alerting on rather than
/// business as usual.
pub fn check_rejection_rate(
    report: &BacktestReport,
    baseline: f64,
    tolerance: f64,
) -> Option<Alert> {
    if report.orders_proposed == 0 {
        return None;
    }
    let rate = report.orders_rejected_by_risk as f64 / report.orders_proposed as f64;
    if rate > baseline + tolerance {
        Some(Alert::RejectionRateAboveBaseline { rate, baseline })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::RejectionCount;
    use parallax_types::{CanonicalContractId, NormalizedTick, Timestamp, VenueId};

    #[test]
    fn a_reconciled_report_raises_no_alert() {
        let report = ReconciliationReport {
            positions_loaded: 3,
            open_orders: Vec::new(),
            orders_resolved: 1,
            orders_unresolved: Vec::new(),
            gate_reconciled: true,
        };
        assert_eq!(check_reconciliation(&report), None);
    }

    #[test]
    fn an_unreconciled_report_raises_a_loud_alert_with_the_reasons() {
        let report = ReconciliationReport {
            positions_loaded: 0,
            open_orders: Vec::new(),
            orders_resolved: 0,
            orders_unresolved: vec![(
                parallax_types::ClientOrderId("plx-abc".into()),
                "timed out".to_string(),
            )],
            gate_reconciled: false,
        };
        match check_reconciliation(&report) {
            Some(Alert::ReconciliationMismatch {
                unresolved_orders,
                detail,
            }) => {
                assert_eq!(unresolved_orders, 1);
                assert!(detail.contains("timed out"));
            }
            other => panic!("expected ReconciliationMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_book_raises_no_feed_data_alert() {
        let book = ConsolidatedBook::new();
        assert_eq!(check_feed_data_quality(&book), None);
    }

    #[test]
    fn a_book_with_a_rejected_tick_raises_an_alert() {
        let mut book = ConsolidatedBook::new();
        book.update(NormalizedTick {
            venue: VenueId::Kalshi,
            contract: CanonicalContractId("c".into()),
            bid: f64::NAN, // fails NormalizedTick::validate()
            bid_size: 1.0,
            ask: 0.5,
            ask_size: 1.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        });
        assert_eq!(
            check_feed_data_quality(&book),
            Some(Alert::FeedDataRejected { rejected_ticks: 1 })
        );
    }

    fn report_with_histogram(histogram: Vec<RejectionCount>) -> BacktestReport {
        BacktestReport {
            rejection_histogram: histogram,
            ..Default::default()
        }
    }

    #[test]
    fn no_feed_stale_entries_raises_no_alert() {
        let report = report_with_histogram(vec![RejectionCount {
            reason: "PriceThroughBook".into(),
            count: 3,
        }]);
        assert_eq!(check_feed_staleness(&report), None);
    }

    #[test]
    fn feed_stale_entries_raise_an_alert_with_the_total_count() {
        let report = report_with_histogram(vec![
            RejectionCount {
                reason: "FeedStale".into(),
                count: 4,
            },
            RejectionCount {
                reason: "PriceThroughBook".into(),
                count: 2,
            },
        ]);
        assert_eq!(
            check_feed_staleness(&report),
            Some(Alert::FeedStaleness { count: 4 })
        );
    }

    #[test]
    fn a_rejection_rate_within_tolerance_of_baseline_raises_no_alert() {
        // 12% vs a 10% baseline, 5% tolerance.
        let report = BacktestReport {
            orders_proposed: 100,
            orders_rejected_by_risk: 12,
            ..Default::default()
        };
        assert_eq!(check_rejection_rate(&report, 0.10, 0.05), None);
    }

    #[test]
    fn a_rejection_rate_beyond_tolerance_raises_an_alert() {
        // 40% vs a 10% baseline, 5% tolerance.
        let report = BacktestReport {
            orders_proposed: 100,
            orders_rejected_by_risk: 40,
            ..Default::default()
        };
        match check_rejection_rate(&report, 0.10, 0.05) {
            Some(Alert::RejectionRateAboveBaseline { rate, baseline }) => {
                assert!((rate - 0.40).abs() < 1e-9);
                assert!((baseline - 0.10).abs() < 1e-9);
            }
            other => panic!("expected RejectionRateAboveBaseline, got {other:?}"),
        }
    }

    #[test]
    fn zero_proposed_orders_cannot_divide_by_zero() {
        let report = BacktestReport::default();
        assert_eq!(check_rejection_rate(&report, 0.10, 0.05), None);
    }
}
