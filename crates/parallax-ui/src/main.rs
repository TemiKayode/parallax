//! A local web dashboard for PARALLAX. Two kinds of data live here, and
//! the UI is explicit about which is which: the backtest/arb-detector
//! panels run against synthetic data through the in-memory `PaperAdapter`,
//! while the "Live venue quotes" panel makes real HTTP calls to Kalshi's
//! and Polymarket's public market-data endpoints (see `live.rs`) and
//! shows exactly what those venues are quoting right now. Neither path
//! can place a real order — that stays gated behind an unconfigured
//! signer regardless of which panel you're looking at.
//!
//! Run with `cargo run -p parallax-ui` (or the installed `parallax-ui`
//! binary — see the repo README for "run from anywhere" instructions),
//! then open http://127.0.0.1:7878.

#![forbid(unsafe_code)]

mod dto;
mod live;

use axum::error_handling::HandleErrorLayer;
use axum::extract::{DefaultBodyLimit, Json, Request};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{BoxError, Router};
use dto::{ArbResponse, BacktestResponse};
use serde::Deserialize;
use std::time::Duration;
use tower::limit::ConcurrencyLimitLayer;
use tower::timeout::TimeoutLayer;
use tower::ServiceBuilder;

/// Above this, a request body is rejected outright rather than read into
/// memory — the arb-detector body is four floats; nothing legitimate
/// needs more than a few KB (design doc review 3.21).
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;

/// How many requests may be in flight at once. `/api/backtest` runs a
/// full synthetic backtest per call — unauthenticated, unbounded
/// concurrency here is a self-inflicted resource-exhaustion path (design
/// doc review 3.21).
const MAX_CONCURRENT_REQUESTS: usize = 16;

/// A stuck downstream call (a hung Kalshi/Polymarket connection outside
/// its own client-level timeout, a future request handler that blocks)
/// must not hold a concurrency-limit permit forever — that would starve
/// every other request behind it. Defense in depth on top of
/// `parallax_venues::http_client`'s own 10s per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Adds the response headers a browser needs to defend the dashboard
/// against itself, even though every dynamic value it renders is already
/// escaped client-side (design doc review, Phase 3 audit): a strict CSP
/// with no external resource loads is possible here specifically because
/// the page has zero inline `<script>`/`<style>` and zero CDN/external
/// URLs — verified, not assumed, before writing this policy.
async fn security_headers(request: Request, next: Next) -> impl IntoResponse {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'self'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    // Legacy header some older browsers still honor; explicitly disabled
    // per current guidance, since the browser's own XSS auditor has
    // itself been a source of vulnerabilities and the CSP above is the
    // real defense.
    headers.insert(
        HeaderName::from_static("x-xss-protection"),
        HeaderValue::from_static("0"),
    );
    response
}

/// `TimeoutLayer` turns a slow request into a `tower::timeout::error::
/// Elapsed` at the `Service` level, which axum can't turn into an HTTP
/// response on its own — a `Router`'s underlying service must be
/// infallible. This is the standard axum pattern for bridging that: pair
/// `TimeoutLayer` with `HandleErrorLayer` so a timeout becomes a normal
/// 408 response instead of a type error at compile time or a dropped
/// connection at runtime.
async fn handle_timeout_error(err: BoxError) -> (StatusCode, String) {
    if err.is::<tower::timeout::error::Elapsed>() {
        (
            StatusCode::REQUEST_TIMEOUT,
            "request took too long".to_string(),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("unhandled error: {err}"),
        )
    }
}

const INDEX_HTML: &str = include_str!("../static/index.html");
const STYLE_CSS: &str = include_str!("../static/style.css");
const APP_JS: &str = include_str!("../static/app.js");

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        APP_JS,
    )
}

#[derive(Deserialize)]
struct ArbRequest {
    polymarket_bid: f64,
    polymarket_ask: f64,
    polymarket_size: f64,
    kalshi_bid: f64,
    kalshi_ask: f64,
    kalshi_size: f64,
}

async fn arb_handler(Json(req): Json<ArbRequest>) -> Json<ArbResponse> {
    let arb = parallax_cli::sample_arb(
        req.polymarket_bid,
        req.polymarket_ask,
        req.polymarket_size,
        req.kalshi_bid,
        req.kalshi_ask,
        req.kalshi_size,
    );
    Json(ArbResponse::from(arb))
}

async fn backtest_handler() -> Json<BacktestResponse> {
    let report = parallax_cli::run_demo_backtest().await;
    Json(BacktestResponse::from(report))
}

async fn live_kalshi_handler() -> Result<Json<live::LiveQuote>, (StatusCode, String)> {
    live::fetch_live_kalshi()
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))
}

async fn live_polymarket_handler() -> Result<Json<live::LiveQuote>, (StatusCode, String)> {
    live::fetch_live_polymarket()
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))
}

async fn health_handler() -> &'static str {
    "ok"
}

/// Resolves once Ctrl+C is received. If installing the signal handler
/// itself fails — extremely rare, and only on platforms without the
/// expected signal-handling primitives — that must not take an
/// otherwise-healthy, actively-serving dashboard down with it: log the
/// problem and fall back to a future that never resolves, so graceful
/// shutdown-via-signal simply doesn't trigger rather than the whole
/// process panicking out from under live requests.
async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => println!("shutdown signal received, draining in-flight requests..."),
        Err(e) => {
            eprintln!(
                "warning: could not install Ctrl+C handler ({e}); graceful shutdown via signal is disabled for this run"
            );
            std::future::pending::<()>().await;
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/style.css", get(style_css))
        .route("/app.js", get(app_js))
        .route("/health", get(health_handler))
        .route("/api/arb", post(arb_handler))
        .route("/api/backtest", post(backtest_handler))
        .route("/api/live/kalshi", get(live_kalshi_handler))
        .route("/api/live/polymarket", get(live_polymarket_handler))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(ConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_timeout_error))
                .layer(TimeoutLayer::new(REQUEST_TIMEOUT)),
        )
        .layer(middleware::from_fn(security_headers));

    // Loopback-only by default (design doc review 3.21): every route
    // here is unauthenticated, and `/api/backtest` runs real work per
    // request. Binding to all interfaces is an explicit, named opt-in,
    // never the default a stray env or a copy-pasted deploy config
    // silently inherits.
    let allow_remote = std::env::var("PARALLAX_UI_ALLOW_REMOTE").is_ok();
    let host = if allow_remote { "0.0.0.0" } else { "127.0.0.1" };
    let addr = format!("{host}:7878");
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        eprintln!("failed to bind to {addr}: {e}");
        e
    })?;
    println!("PARALLAX dashboard running at http://{addr}");
    if allow_remote {
        println!(
            "PARALLAX_UI_ALLOW_REMOTE is set: listening on all interfaces, not just loopback."
        );
    }
    println!("Live venue quotes: real read-only API calls to Kalshi and Polymarket.");
    println!("Backtest/arb-detector panels: synthetic data via the in-memory PaperAdapter.");
    println!("No live order submission anywhere — that stays gated behind an unconfigured signer.");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
}
