//! A local web dashboard for PARALLAX. Runs entirely against synthetic
//! data through the in-memory `PaperAdapter` — no live venue connection,
//! no credentials, nothing here can place a real order. It exists so the
//! engine in `crates/` is something you can look at in a browser instead
//! of only reading test output.
//!
//! Run with `cargo run -p parallax-ui` (or the installed `parallax-ui`
//! binary — see the repo README for "run from anywhere" instructions),
//! then open http://127.0.0.1:7878.

mod dto;

use axum::extract::Json;
use axum::http::header;
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

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(index))
        .route("/style.css", get(style_css))
        .route("/app.js", get(app_js))
        .route("/api/arb", post(arb_handler))
        .route("/api/backtest", post(backtest_handler));

    let addr = "127.0.0.1:7878";
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind to 127.0.0.1:7878");
    println!("PARALLAX dashboard running at http://{addr}");
    println!("Synthetic data only — no live venue connection, no credentials, no real orders.");
    axum::serve(listener, app).await.expect("server error");
}
