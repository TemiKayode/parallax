//! Shared HTTP plumbing for the live venue adapters: a client configured
//! with real timeouts, a self-throttling rate limiter, and a response
//! decoder that turns an HTTP status into a typed `ExecError` instead of
//! calling `.json()` blind.

use parallax_types::{ExecError, VenueId};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// `reqwest::Client::new()` has no timeout at all: a venue that accepts a
/// connection and never answers hangs the calling task forever, and
/// whatever it was waiting to update (the book, a fill) silently stops
/// (design doc review 3.2). Three timeouts, not one: a request that's
/// slow to *finish* is different from one that's slow to *connect*, and
/// an idle pooled connection should be recycled rather than held forever.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        // `build()` only fails on TLS-backend initialization failure — an
        // environment-level misconfiguration (missing root certs, a
        // broken rustls install) that would make every venue call fail
        // anyway. Threading a `Result` through every adapter constructor
        // to handle a case that means "this process cannot make HTTPS
        // requests at all" buys nothing; fail loudly at construction
        // instead of quietly on the first live call.
        .expect("failed to build HTTP client")
}

/// Token-bucket self-throttle. `rate_limit_per_sec` on `VenueCapabilities`
/// existed and nothing read it (design doc review 3.4) — every adapter
/// should acquire a permit before sending, staying below the venue's
/// published limit rather than skating against it and finding out from a
/// 429.
///
/// `reserved_for_cancel` tokens are held back from ordinary traffic
/// (`acquire`) and spendable only through `acquire_for_cancel`
/// (docs/GOING-LIVE.md Stage 2: "hold back enough tokens to cancel every
/// resting order at all times" — running out of cancel capacity while
/// holding live quotes during a fault is the worst reachable state, and
/// it's preventable by exactly this arithmetic). `RateLimiter::new`
/// reserves nothing, matching its existing behavior; call
/// `with_reserved_for_cancel` to opt in.
pub struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    reserved_for_cancel: f64,
    state: Mutex<RateLimiterState>,
}

struct RateLimiterState {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(requests_per_sec: u32) -> Self {
        Self::with_reserved_for_cancel(requests_per_sec, 0)
    }

    pub fn with_reserved_for_cancel(requests_per_sec: u32, reserved_for_cancel: u32) -> Self {
        let capacity = (requests_per_sec.max(1)) as f64;
        // Reserving more than the whole bucket would starve ordinary
        // traffic completely — clamped, not trusted.
        let reserved_for_cancel = (reserved_for_cancel as f64).min(capacity);
        RateLimiter {
            capacity,
            refill_per_sec: capacity,
            reserved_for_cancel,
            state: Mutex::new(RateLimiterState {
                tokens: capacity,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Waits, if necessary, until a request is allowed, then consumes one
    /// token. Never returns early just because another caller is also
    /// waiting — each call independently earns its own token. Will not
    /// spend the last `reserved_for_cancel` tokens — use
    /// `acquire_for_cancel` for a cancel request.
    pub async fn acquire(&self) {
        self.acquire_above_floor(self.reserved_for_cancel).await;
    }

    /// Same as `acquire`, but may spend all the way down to zero,
    /// including the tokens `acquire` refuses to touch. The only caller
    /// that should ever reach for this is an actual cancel request — a
    /// reserve that ordinary traffic can also dip into on a good excuse
    /// isn't a reserve.
    pub async fn acquire_for_cancel(&self) {
        self.acquire_above_floor(0.0).await;
    }

    async fn acquire_above_floor(&self, floor: f64) {
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now
                    .saturating_duration_since(state.last_refill)
                    .as_secs_f64();
                state.tokens = (state.tokens + elapsed * self.refill_per_sec).min(self.capacity);
                state.last_refill = now;
                let available = state.tokens - floor;
                if available >= 1.0 {
                    state.tokens -= 1.0;
                    None
                } else {
                    let deficit = 1.0 - available;
                    Some(Duration::from_secs_f64(
                        (deficit / self.refill_per_sec).max(0.0),
                    ))
                }
            };
            match wait {
                None => return,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }
}

/// Maps an HTTP status (plus an optional `Retry-After` header value) to
/// the `ExecError` it implies, or `None` for success. Split out from
/// `json_or_error` so the status-to-error mapping is testable without
/// constructing a live `reqwest::Response`.
fn classify_status(
    venue: VenueId,
    status: reqwest::StatusCode,
    retry_after: Option<&str>,
) -> Option<ExecError> {
    if status.as_u16() == 429 {
        let retry_after_ms = retry_after
            .and_then(|s| s.parse::<u64>().ok())
            .map(|secs| secs * 1000)
            .unwrap_or(1_000);
        return Some(ExecError::RateLimited {
            venue,
            retry_after_ms,
        });
    }
    if status.as_u16() == 425 {
        // Distinct from the generic `!is_success()` fallback below on
        // purpose (docs/GOING-LIVE.md Stage 2) — folding this into
        // `Rejected` loses the signal that aggressive immediate retry is
        // exactly the wrong response to a matching engine that's still
        // coming back up.
        let retry_after_ms = retry_after
            .and_then(|s| s.parse::<u64>().ok())
            .map(|secs| secs * 1000)
            .unwrap_or(5_000);
        return Some(ExecError::VenueRestarting {
            venue,
            retry_after_ms,
        });
    }
    if !status.is_success() {
        return Some(ExecError::Rejected {
            venue,
            reason: format!("HTTP {status}"),
        });
    }
    None
}

/// Checks the response's status before ever calling `.json()` — a 429 or
/// 500 with a JSON error body used to parse "successfully" and fail
/// downstream as a confusing missing-field error, losing the
/// `Retry-After` hint along the way (design doc review 3.3).
pub async fn json_or_error<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    venue: VenueId,
) -> Result<T, ExecError> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(err) = classify_status(venue, status, retry_after.as_deref()) {
        return Err(err);
    }
    resp.json::<T>().await.map_err(|e| ExecError::Connection {
        venue,
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_status_classifies_as_no_error() {
        assert!(classify_status(VenueId::Kalshi, reqwest::StatusCode::OK, None).is_none());
    }

    #[test]
    fn rate_limited_status_carries_the_retry_after_header() {
        match classify_status(
            VenueId::Kalshi,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some("30"),
        ) {
            Some(ExecError::RateLimited { retry_after_ms, .. }) => {
                assert_eq!(retry_after_ms, 30_000)
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn rate_limited_without_a_header_still_gets_a_conservative_default() {
        match classify_status(
            VenueId::Kalshi,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            None,
        ) {
            Some(ExecError::RateLimited { .. }) => {}
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn a_server_error_status_classifies_as_rejected() {
        match classify_status(
            VenueId::Kalshi,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            None,
        ) {
            Some(ExecError::Rejected { .. }) => {}
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn http_425_is_a_distinct_venue_restarting_error_not_a_generic_rejection() {
        let status = reqwest::StatusCode::from_u16(425).unwrap();
        match classify_status(VenueId::Polymarket, status, Some("10")) {
            Some(ExecError::VenueRestarting { retry_after_ms, .. }) => {
                assert_eq!(retry_after_ms, 10_000)
            }
            other => panic!("expected VenueRestarting, got {other:?}"),
        }
    }

    #[test]
    fn http_425_without_a_header_still_gets_a_conservative_default() {
        let status = reqwest::StatusCode::from_u16(425).unwrap();
        match classify_status(VenueId::Polymarket, status, None) {
            Some(ExecError::VenueRestarting { retry_after_ms, .. }) => {
                assert!(retry_after_ms > 0)
            }
            other => panic!("expected VenueRestarting, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rate_limiter_throttles_bursts_above_capacity() {
        let limiter = RateLimiter::new(1_000); // generous, just proving it doesn't hang
        let start = Instant::now();
        for _ in 0..5 {
            limiter.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn rate_limiter_delays_once_the_bucket_is_exhausted() {
        let limiter = RateLimiter::new(2); // 2/sec
        let start = Instant::now();
        for _ in 0..3 {
            limiter.acquire().await;
        }
        // Burst of 2 is free; the 3rd must wait for a refill.
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn ordinary_traffic_cannot_spend_the_reserved_cancel_tokens() {
        // 3/sec, 1 reserved for cancel: ordinary acquire() can only ever
        // draw down to 1 token remaining, never below it.
        let limiter = RateLimiter::with_reserved_for_cancel(3, 1);
        let start = Instant::now();
        limiter.acquire().await;
        limiter.acquire().await;
        // The 2 non-reserved tokens are spent; a 3rd ordinary acquire()
        // must wait for a refill rather than eating the reserve.
        limiter.acquire().await;
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn acquire_for_cancel_can_spend_the_reserve_immediately() {
        let limiter = RateLimiter::with_reserved_for_cancel(3, 1);
        limiter.acquire().await;
        limiter.acquire().await;
        // Ordinary traffic has now exhausted everything but the reserve.
        // A cancel must still go through without waiting.
        let start = Instant::now();
        limiter.acquire_for_cancel().await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn reserving_more_than_capacity_is_clamped_not_trusted() {
        // Asking to reserve 100 out of a 3-token bucket must not starve
        // every ordinary request forever — it's clamped to the capacity.
        let limiter = RateLimiter::with_reserved_for_cancel(3, 100);
        let start = Instant::now();
        limiter.acquire_for_cancel().await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }
}
