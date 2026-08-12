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

mod dto;
mod live;

use axum::extract::Json;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;
use dto::{ArbResponse, BacktestResponse};
use serde::Deserialize;

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
    kalshi_bid: f64,
    kalshi_ask: f64,
}

async fn arb_handler(Json(req): Json<ArbRequest>) -> Json<ArbResponse> {
    let arb = parallax_cli::sample_arb(
        req.polymarket_bid,
        req.polymarket_ask,
        req.kalshi_bid,
        req.kalshi_ask,
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

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(index))
        .route("/style.css", get(style_css))
        .route("/app.js", get(app_js))
        .route("/api/arb", post(arb_handler))
        .route("/api/backtest", post(backtest_handler))
        .route("/api/live/kalshi", get(live_kalshi_handler))
        .route("/api/live/polymarket", get(live_polymarket_handler));

    let addr = "127.0.0.1:7878";
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind to 127.0.0.1:7878");
    println!("PARALLAX dashboard running at http://{addr}");
    println!("Live venue quotes: real read-only API calls to Kalshi and Polymarket.");
    println!("Backtest/arb-detector panels: synthetic data via the in-memory PaperAdapter.");
    println!("No live order submission anywhere — that stays gated behind an unconfigured signer.");
    axum::serve(listener, app).await.expect("server error");
}
