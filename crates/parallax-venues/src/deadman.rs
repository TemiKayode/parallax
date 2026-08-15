//! `docs/GOING-LIVE.md` Stage 2, "venue-side dead-man switch":
//!
//! > Polymarket's CLOB has a heartbeat endpoint: stop sending heartbeats
//! > and it cancels all your open orders automatically. This is the
//! > single highest-value safety control available, because it is
//! > enforced by the venue and requires nothing of you at the moment you
//! > most need it. Wire it before your first live order, not after.
//!
//! The kill switch this repo already has (`parallax-risk::KillSwitch`)
//! protects against a strategy that's *wrong*. It does nothing for a
//! process that's crashed, a host that's lost power, or a network
//! that's partitioned while orders are still resting — the venue is the
//! only party still watching at that point, which is exactly why this
//! mechanism has to live on the venue's side, not PARALLAX's.
//!
//! This is named "Polymarket's heartbeat," not "every venue's," on
//! purpose — the doc is specific to Polymarket's CLOB, and nothing here
//! assumes Kalshi has an equivalent without checking first.

use async_trait::async_trait;
use parallax_types::ExecError;
use std::ops::ControlFlow;
use std::time::Duration;

/// A venue-enforced dead-man switch: sending a heartbeat resets the
/// venue's own cancel-on-timeout timer. Miss enough heartbeats and the
/// venue cancels every resting order itself, with no cooperation needed
/// from a process that may no longer be running to give it.
#[async_trait]
pub trait DeadmanSwitch: Send + Sync {
    /// Sends one heartbeat. Must be called more often than the venue's
    /// own timeout for the switch to do anything — see the implementor's
    /// docs for that interval once it's live-verified.
    async fn heartbeat(&self) -> Result<(), ExecError>;
}

/// Sends a heartbeat every `interval`, forever, until `on_result` says to
/// stop. `on_result` sees every attempt's outcome — including failures —
/// so the caller decides how loudly to escalate a miss; this loop itself
/// never swallows one silently by retrying into the next tick without
/// telling anyone.
pub async fn run_heartbeat_loop(
    switch: &dyn DeadmanSwitch,
    interval: Duration,
    mut on_result: impl FnMut(Result<(), ExecError>) -> ControlFlow<()>,
) {
    loop {
        let result = switch.heartbeat().await;
        if on_result(result).is_break() {
            return;
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::VenueId;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeSwitch {
        results: Mutex<Vec<Result<(), ExecError>>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl DeadmanSwitch for FakeSwitch {
        async fn heartbeat(&self) -> Result<(), ExecError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut results = self.results.lock().unwrap();
            if results.is_empty() {
                Ok(())
            } else {
                results.remove(0)
            }
        }
    }

    #[tokio::test]
    async fn sends_a_heartbeat_every_interval_until_told_to_stop() {
        let switch = FakeSwitch::default();
        let mut seen = 0;
        run_heartbeat_loop(&switch, Duration::from_millis(1), |result| {
            assert!(result.is_ok());
            seen += 1;
            if seen >= 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await;
        assert_eq!(switch.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_failed_heartbeat_is_reported_not_swallowed() {
        let switch = FakeSwitch {
            results: Mutex::new(vec![
                Ok(()),
                Err(ExecError::Connection {
                    venue: VenueId::Polymarket,
                    message: "timed out".into(),
                }),
            ]),
            ..Default::default()
        };
        let mut failures_seen = 0;
        let mut attempts = 0;
        run_heartbeat_loop(&switch, Duration::from_millis(1), |result| {
            attempts += 1;
            if result.is_err() {
                failures_seen += 1;
            }
            if attempts >= 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await;
        assert_eq!(failures_seen, 1);
    }

    #[tokio::test]
    async fn stopping_immediately_sends_exactly_one_heartbeat() {
        let switch = FakeSwitch::default();
        run_heartbeat_loop(&switch, Duration::from_secs(3600), |_| {
            ControlFlow::Break(())
        })
        .await;
        // Confirms the loop sends a heartbeat *before* waiting on the
        // interval, not after — an hour-long sleep before the first
        // heartbeat would defeat the entire mechanism on process start.
        assert_eq!(switch.calls.load(Ordering::SeqCst), 1);
    }
}
