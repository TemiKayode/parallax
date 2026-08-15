//! `docs/GOING-LIVE.md` Stage 4: "Paper-trade against the live feed...
//! catches an entire class of bugs no backtest can (schema drift,
//! reconnect storms, auth expiry, rate-limit behaviour under load) at
//! zero financial risk."
//!
//! What this module (and `record_main.rs`, which drives it) actually
//! builds, stated plainly: continuous real-feed ingestion into a real
//! `ConsolidatedBook`, with health tracked the same way
//! `parallax_sim::FeeVerifier` tracks fee divergence — consecutive
//! failures, not single blips, because a fetch that fails once in a
//! while is normal internet, and a fetch that fails ten times in a row
//! is a real problem.
//!
//! What this does **not** do: exercise the strategy/risk pipeline
//! against real trading signals. There is no live alpha source in this
//! repo — the weather/econ/news/oracle sources are all demo- or
//! offline-config-driven, not fed by a real live external feed — and
//! Kalshi's real `KXHIGHCHI` listing and Polymarket's real top-volume
//! market are two different real-world events with no shared canonical
//! contract id to run a strategy against (the dashboard's live-quotes
//! panel already discloses this same fact). Claiming this tests
//! "paper-trading" in the full sense the doc means would be exactly the
//! kind of overclaim this repo has tried not to make anywhere else.

use parallax_types::VenueId;
use std::collections::HashMap;

/// One venue crossing from "probably fine" to "something is actually
/// wrong" — `consecutive_failures` has reached the configured threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedHealthAlert {
    pub venue: VenueId,
    pub consecutive_failures: u32,
}

/// Tracks a consecutive-failure streak per venue, independently — one
/// venue being down says nothing about the other, and conflating them
/// into a single counter would mask which one actually needs attention.
pub struct FeedHealthMonitor {
    halt_after: u32,
    streaks: HashMap<VenueId, u32>,
}

impl FeedHealthMonitor {
    pub fn new(halt_after: u32) -> Self {
        FeedHealthMonitor {
            halt_after: halt_after.max(1),
            streaks: HashMap::new(),
        }
    }

    /// Records one fetch attempt's outcome for `venue`. A success resets
    /// that venue's streak to zero — this tracks "how bad is it *right
    /// now*," not a lifetime failure count. Returns an alert only once
    /// the streak reaches `halt_after`, and again on every failure past
    /// that point (an operator watching should keep hearing about an
    /// ongoing outage, not just its first moment).
    pub fn record(&mut self, venue: VenueId, success: bool) -> Option<FeedHealthAlert> {
        let streak = self.streaks.entry(venue).or_insert(0);
        if success {
            *streak = 0;
            return None;
        }
        *streak += 1;
        if *streak >= self.halt_after {
            Some(FeedHealthAlert {
                venue,
                consecutive_failures: *streak,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_failure_below_the_threshold_raises_no_alert() {
        let mut monitor = FeedHealthMonitor::new(3);
        assert_eq!(monitor.record(VenueId::Kalshi, false), None);
        assert_eq!(monitor.record(VenueId::Kalshi, false), None);
    }

    #[test]
    fn reaching_the_threshold_raises_an_alert_with_the_streak_length() {
        let mut monitor = FeedHealthMonitor::new(3);
        monitor.record(VenueId::Kalshi, false);
        monitor.record(VenueId::Kalshi, false);
        assert_eq!(
            monitor.record(VenueId::Kalshi, false),
            Some(FeedHealthAlert {
                venue: VenueId::Kalshi,
                consecutive_failures: 3
            })
        );
    }

    #[test]
    fn continuing_failure_keeps_alerting_past_the_threshold() {
        let mut monitor = FeedHealthMonitor::new(2);
        monitor.record(VenueId::Polymarket, false);
        monitor.record(VenueId::Polymarket, false);
        assert_eq!(
            monitor.record(VenueId::Polymarket, false),
            Some(FeedHealthAlert {
                venue: VenueId::Polymarket,
                consecutive_failures: 3
            })
        );
    }

    #[test]
    fn a_success_resets_the_streak() {
        let mut monitor = FeedHealthMonitor::new(2);
        monitor.record(VenueId::Kalshi, false);
        assert_eq!(monitor.record(VenueId::Kalshi, true), None);
        // Streak reset — one more failure alone must not re-trigger.
        assert_eq!(monitor.record(VenueId::Kalshi, false), None);
    }

    #[test]
    fn venues_are_tracked_independently() {
        let mut monitor = FeedHealthMonitor::new(2);
        monitor.record(VenueId::Kalshi, false);
        monitor.record(VenueId::Kalshi, false);
        // Polymarket has had zero failures — must not be affected by
        // Kalshi's streak.
        assert_eq!(monitor.record(VenueId::Polymarket, false), None);
    }

    #[test]
    fn zero_configuration_is_clamped_to_at_least_one() {
        let mut monitor = FeedHealthMonitor::new(0);
        assert!(monitor.record(VenueId::Kalshi, false).is_some());
    }
}
