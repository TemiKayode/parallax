//! `docs/GOING-LIVE.md` Stage 1: "Clock discipline belongs here: signed
//! requests carry timestamps, and skew shows up as authentication
//! failures at the worst possible moment. NTP, monitored, with an alert
//! on drift."
//!
//! This repo has no NTP client — that's OS-level infrastructure, out of
//! scope for an application crate — and no venue credentials to
//! authenticate with yet (see the `KalshiRequestSigner`/
//! `PolymarketOrderSigner` doc comments on why signing is deliberately
//! not implemented inline here). What it can build and verify today:
//! comparing this process's own clock against a real, independent
//! reference every live venue call already touches for free — the HTTP
//! `Date` response header (`http::parse_http_date`/`response_date`),
//! present on every response regardless of the endpoint's own business
//! meaning — and alerting on a *sustained* divergence rather than a
//! single reading. Same "streak, not a blip" design `FeedHealthMonitor`
//! (`parallax-cli`) and `parallax_sim::FeeVerifier` already use elsewhere
//! in this repo: the `Date` header has one-second resolution and isn't
//! round-trip-time-corrected, so a single skewed reading is
//! indistinguishable from ordinary one-way network jitter. Several in a
//! row, independently against the same venue, is what a genuinely
//! drifting local clock looks like.

use parallax_types::{Timestamp, VenueId};
use std::collections::HashMap;

/// One venue's clock-skew streak has reached the configured threshold —
/// the local clock has been consistently off by more than `max_skew_ms`
/// against `venue`'s own reported time, not just once.
#[derive(Debug, Clone, PartialEq)]
pub struct ClockSkewAlert {
    pub venue: VenueId,
    /// Positive: the local clock is ahead of the venue. Negative: behind.
    pub skew_ms: i64,
    pub consecutive: u32,
}

/// Tracks a consecutive-out-of-tolerance streak per venue, independently
/// — one venue's connection routing through a slow or congested path (and
/// so reading skewed) says nothing about another venue's.
pub struct ClockSkewMonitor {
    max_skew_ms: i64,
    alert_after: u32,
    streaks: HashMap<VenueId, u32>,
}

impl ClockSkewMonitor {
    pub fn new(max_skew_ms: i64, alert_after: u32) -> Self {
        ClockSkewMonitor {
            max_skew_ms: max_skew_ms.max(0),
            alert_after: alert_after.max(1),
            streaks: HashMap::new(),
        }
    }

    /// Records one `(local, venue-reported)` timestamp pair for `venue`.
    /// A reading within tolerance resets that venue's streak to zero —
    /// this tracks "is skew ongoing right now," not a lifetime count.
    /// Returns an alert only once the streak reaches `alert_after`, and
    /// again on every out-of-tolerance reading past that point, carrying
    /// the *current* reading's skew — an operator watching should keep
    /// hearing about ongoing drift, not just its first moment.
    pub fn record(
        &mut self,
        venue: VenueId,
        local: Timestamp,
        remote: Timestamp,
    ) -> Option<ClockSkewAlert> {
        let skew_ms = local.since(remote) / 1_000_000;
        let streak = self.streaks.entry(venue).or_insert(0);
        if skew_ms.abs() <= self.max_skew_ms {
            *streak = 0;
            return None;
        }
        *streak += 1;
        if *streak >= self.alert_after {
            Some(ClockSkewAlert {
                venue,
                skew_ms,
                consecutive: *streak,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: i64) -> Timestamp {
        Timestamp::from_nanos(n * 1_000_000)
    }

    #[test]
    fn a_reading_within_tolerance_raises_no_alert() {
        let mut monitor = ClockSkewMonitor::new(500, 3);
        assert_eq!(monitor.record(VenueId::Kalshi, ms(1_000), ms(1_200)), None);
    }

    #[test]
    fn out_of_tolerance_readings_below_the_streak_threshold_raise_no_alert() {
        let mut monitor = ClockSkewMonitor::new(500, 3);
        assert_eq!(monitor.record(VenueId::Kalshi, ms(2_000), ms(1_000)), None);
        assert_eq!(monitor.record(VenueId::Kalshi, ms(2_000), ms(1_000)), None);
    }

    #[test]
    fn reaching_the_streak_threshold_raises_an_alert_with_the_current_skew() {
        let mut monitor = ClockSkewMonitor::new(500, 3);
        monitor.record(VenueId::Kalshi, ms(2_000), ms(1_000));
        monitor.record(VenueId::Kalshi, ms(2_000), ms(1_000));
        assert_eq!(
            monitor.record(VenueId::Kalshi, ms(2_000), ms(1_000)),
            Some(ClockSkewAlert {
                venue: VenueId::Kalshi,
                skew_ms: 1_000,
                consecutive: 3,
            })
        );
    }

    #[test]
    fn a_negative_skew_local_clock_behind_is_detected_by_magnitude() {
        let mut monitor = ClockSkewMonitor::new(500, 1);
        assert_eq!(
            monitor.record(VenueId::Kalshi, ms(1_000), ms(2_000)),
            Some(ClockSkewAlert {
                venue: VenueId::Kalshi,
                skew_ms: -1_000,
                consecutive: 1,
            })
        );
    }

    #[test]
    fn continuing_drift_keeps_alerting_past_the_threshold() {
        let mut monitor = ClockSkewMonitor::new(500, 2);
        monitor.record(VenueId::Polymarket, ms(2_000), ms(1_000));
        monitor.record(VenueId::Polymarket, ms(2_000), ms(1_000));
        assert_eq!(
            monitor.record(VenueId::Polymarket, ms(2_000), ms(1_000)),
            Some(ClockSkewAlert {
                venue: VenueId::Polymarket,
                skew_ms: 1_000,
                consecutive: 3,
            })
        );
    }

    #[test]
    fn an_in_tolerance_reading_resets_the_streak() {
        let mut monitor = ClockSkewMonitor::new(500, 2);
        monitor.record(VenueId::Kalshi, ms(2_000), ms(1_000));
        assert_eq!(monitor.record(VenueId::Kalshi, ms(1_000), ms(1_000)), None);
        // Streak reset — one more out-of-tolerance reading alone must not
        // re-trigger.
        assert_eq!(monitor.record(VenueId::Kalshi, ms(2_000), ms(1_000)), None);
    }

    #[test]
    fn venues_are_tracked_independently() {
        let mut monitor = ClockSkewMonitor::new(500, 2);
        monitor.record(VenueId::Kalshi, ms(2_000), ms(1_000));
        monitor.record(VenueId::Kalshi, ms(2_000), ms(1_000));
        // Polymarket has had zero out-of-tolerance readings — must not be
        // affected by Kalshi's streak.
        assert_eq!(
            monitor.record(VenueId::Polymarket, ms(1_000), ms(1_000)),
            None
        );
    }

    #[test]
    fn zero_configuration_is_clamped_to_at_least_one() {
        let mut monitor = ClockSkewMonitor::new(500, 0);
        assert!(monitor
            .record(VenueId::Kalshi, ms(2_000), ms(1_000))
            .is_some());
    }

    #[test]
    fn exactly_at_the_tolerance_boundary_does_not_alert() {
        let mut monitor = ClockSkewMonitor::new(500, 1);
        assert_eq!(monitor.record(VenueId::Kalshi, ms(1_500), ms(1_000)), None);
    }
}
