# PARALLAX

[![CI](https://github.com/TemiKayode/parallax/actions/workflows/ci.yml/badge.svg)](https://github.com/TemiKayode/parallax/actions/workflows/ci.yml)

A cross-venue, multi-asset, event-driven statistical arbitrageur for prediction markets. Full system design: [PARALLAX — Cross-Venue Prediction-Market Statistical Arbitrage: System Design](https://claude.ai/code/artifact/8922a0e2-37fa-4b76-affb-398dd1487c6e).

This repo is the reference implementation of that design. It's a working skeleton with real, tested logic in every layer — not a mockup: **61 Rust tests and 15 Python tests pass**, clippy is clean with zero warnings, and the web dashboard pulls **real, live quotes** from Kalshi's and Polymarket's public market-data APIs alongside a CLI/dashboard demo that runs a full tick-to-fill scenario against the real engine. What it is *not* yet is a live trading system: order *submission* to Kalshi and Polymarket is deliberately incomplete (see [Status](#status)) until request signing and the live payload shape are verified against each venue's current order-entry API — see [Production readiness](#production-readiness) for exactly what that gap means. Live **data** and live **trading** are different claims; this repo makes the first one honestly and does not make the second one at all.

## Layout

```
crates/
  parallax-types      canonical contract schema, event/order/position types — pure data, no I/O
  parallax-bus         lock-free event bus (crossbeam ArrayQueue) wiring the pipeline's topics
  parallax-book        consolidated cross-venue order book + direct arbitrage detection
  parallax-alpha       AlphaSource trait, fair-value aggregator, weather/econ/news/oracle sources
  parallax-risk        the risk gate: position limits, correlated-cluster netting, kill switches
  parallax-strategy    market making / stat-arb / liquidity sniping engines + online calibration
  parallax-venues      VenueAdapter trait, PaperAdapter (real matching engine), Kalshi/Polymarket adapters
  parallax-sim         replay/backtest harness driving the real pipeline against historical/synthetic data
  parallax-cli         library + `parallax-demo` binary: the synthetic scenario, printed to a terminal
  parallax-ui          `parallax-ui` binary: the same scenario, served as a local web dashboard
offline/
  parallax_research/   Python: offline calibration for the constants baked into parallax-alpha
  tests/               pytest suite for the above
```

Every crate maps to a section of the design doc — see each crate's top-level doc comment for the exact cross-reference.

## Quick start

```bash
git clone https://github.com/TemiKayode/parallax.git
cd parallax
cargo test --workspace          # 61 tests
cargo run -p parallax-cli       # terminal demo
cargo run -p parallax-ui        # web dashboard at http://127.0.0.1:7878
```

## Run it from anywhere

The two binaries don't need to run from inside the repo — `cargo install` puts them on your `PATH` (`~/.cargo/bin`, which the standard Rust installer already adds for you) so `parallax-demo` and `parallax-ui` work as plain commands from any directory, on any machine with Rust installed:

```bash
# directly from GitHub, no clone needed
cargo install --git https://github.com/TemiKayode/parallax parallax-cli parallax-ui

# or, from a local clone (e.g. while developing)
cargo install --path crates/parallax-cli
cargo install --path crates/parallax-ui
```

Then, from anywhere:

```bash
parallax-demo   # terminal report
parallax-ui     # opens the dashboard at http://127.0.0.1:7878
```

No Rust installed at all? The only other thing that runs standalone is the Python research suite — `pip install -r offline/requirements.txt && pytest` inside `offline/`. There's currently no prebuilt binary release; `cargo install` is the supported path (see [Production readiness](#production-readiness) for what a real release pipeline would still need).

## The web dashboard (`parallax-ui`)

A small local server (axum) that talks to the *real* engine over a JSON API — nothing in the UI is mocked or precomputed:

- **Live venue quotes** — on load, the server discovers a currently-open market in Kalshi's real `KXHIGHCHI` series ("highest temperature in Chicago" — the same underlying event the synthetic demo below is modeled on) and the highest-24h-volume active market on Polymarket, fetches each venue's live order book over a real HTTP call, and normalizes it with the exact same parsers (`parse_kalshi_orderbook`, `parse_polymarket_book`) that ship tested against fixtures in `parallax-venues`. Both venues' public market-data endpoints are unauthenticated, so this needs no API keys. It's not a matched contract pair for arbitrage — Polymarket doesn't currently list an equivalent Chicago-temperature market — so the two cards show real, independent, live quotes rather than forcing a misleading "same contract" comparison. Requires outbound internet access; refresh anytime with the button.
- **Cross-venue arbitrage detector** — type in bid/ask for both venues, it calls `parallax-book::detect_arb` live and shows whether a riskless edge exists. Manual input, synthetic — this is where you explore the mechanism on a genuinely matched contract pair, which the live panel above can't currently offer.
- **Backtest runner** — one button re-runs the full weather-update → stale-quote → fill scenario through the actual `parallax-sim::Backtest` pipeline (alpha aggregation, all three strategy engines, the risk gate, the matching engine) and renders the report: fills, filled volume, mark-to-model PnL, open positions. Synthetic data via the in-memory `PaperAdapter`.

It binds to `127.0.0.1` only and ships zero JS dependencies (plain HTML/CSS/vanilla JS, no build step, no CDN calls). No order is ever placed by any panel — that stays gated behind `UnconfiguredKalshiSigner` / `UnconfiguredPolymarketSigner` regardless of which panel you're using.

## Status

**Solid and tested:**
- Canonical contract normalization across venues (`parallax-types`, `parallax-book`) — proven with a test that maps a Polymarket-style metric listing and a Kalshi-style imperial listing to the identical canonical id.
- Direct cross-venue arbitrage detection, independent of any model (`parallax-book::detect_arb`).
- The fair-value aggregator: inverse-variance weighting with a disagreement term that widens the confidence band when sources disagree, and a staleness decay that down-weights old estimates (`parallax-alpha`).
- The risk gate: per-contract/per-cluster/per-venue/gross limits, kill switches at three scopes, and — the one that matters most — `check_batch`, which prevents two strategy engines that each individually clear a limit from jointly blowing through it in the same tick.
- The three strategy engines and their triggering logic (market making ladders off the consolidated fair value; stat-arb firing only when a venue price sits fully outside the confidence band; sniping taking exactly the resting size at a stale quote via IOC).
- `PaperAdapter`, a real in-memory matching engine (not a stub) with correct marketable-limit, resting-limit, and IOC semantics, including a regression test proving two orders reacting to the same tick can't jointly overfill against liquidity that only exists once.
- The full pipeline wired together in `parallax-sim::Backtest` and exercised by an integration test, the CLI demo, and the web dashboard: a weather ensemble update shifts the fair value, a stale quote gets bought, the fill lands in the risk gate's position book, and the report shows correct mark-to-model PnL.
- **Live market-data reads against both venues' real production APIs.** `KalshiAdapter::fetch_open_markets_for_series_raw` + `fetch_orderbook_raw` + `parse_kalshi_orderbook`, and `PolymarketAdapter::fetch_active_markets_raw` + `fetch_book_raw` + `parse_polymarket_book`, were run against `external-api.kalshi.com` and `clob.polymarket.com`/`gamma-api.polymarket.com` while building this — not just unit-tested against fixtures. Both parsers matched the real response shapes without any code changes required.

**Deliberately incomplete, and why:**
- `KalshiAdapter::submit` and `PolymarketAdapter::submit` build the request but stop short of the live HTTP call. Both venues' order-*creation* payload shapes (as opposed to the market-data read shapes above, which are verified) were reconstructed from public documentation during this build, not exercised against a live endpoint — shipping unverified field names against an API that moves real money is the wrong tradeoff for a reference implementation.
- Both adapters require an explicit `KalshiRequestSigner` / `PolymarketOrderSigner` before they'll even attempt to submit — the default `UnconfiguredKalshiSigner` / `UnconfiguredPolymarketSigner` always refuses. RSA-PSS (Kalshi) and EIP-712 (Polymarket) signing over a key that can move funds is deliberately not hand-rolled here; wire in an audited implementation.
- Live market data is on-demand (fetched when the dashboard loads or you click refresh), not a continuous streaming ingestion loop feeding `parallax-book`/`parallax-strategy` automatically — there's no WebSocket subscription, scheduler, or long-running process wiring live quotes into the risk-gated strategy pipeline yet. No FIX client or FPGA/kernel-bypass anything either — see [§2 of the design doc](https://claude.ai/code/artifact/8922a0e2-37fa-4b76-affb-398dd1487c6e) for why most of that infrastructure doesn't apply to these venues the way the original brief assumed, and what's realistic instead.
- No production deployment (hot-hot redundancy, PTP clock sync) — that's infrastructure work for when there's a real venue connection to make redundant.

## Production readiness

"Production ready" means two different things here, and only one of them is true today.

**As an open-source software project, yes:** the workspace builds clean, `cargo clippy --workspace --all-targets -- -D warnings` passes with zero warnings, `cargo fmt --all -- --check` passes, CI (`.github/workflows/ci.yml`) runs fmt + clippy + the full Rust and Python test suites on every push and PR, it's MIT licensed, and both binaries install and run from anywhere via `cargo install`. That's the bar this repo is held to and meets.

**As a system that trades real money, no — and it shouldn't claim to.** Beyond the signing/payload gaps listed in [Status](#status), going live would still need, at minimum:
- A compliance review of venue ToS, geofencing, and market-manipulation rules for your jurisdiction and account type — see the design doc's compliance section (§16). This is not legal advice and nothing in this repo constitutes it.
- Real request signing (RSA-PSS for Kalshi, EIP-712 for Polymarket) backed by real key management, tested against each venue's sandbox/demo environment before touching production credentials.
- Secrets handling for API keys that isn't "environment variables on a laptop" — a secrets manager, least-privilege access, and key rotation.
- The hot-hot redundancy, clock discipline, and monitoring/alerting described in design doc §14, none of which exists yet — right now there's one process, one region, and no failover.
- Position/PnL reconciliation against each venue's own account state, not just this repo's internal bookkeeping — the risk gate here is a control on outgoing orders, not a substitute for checking what the venue actually thinks you hold.

Treat this repo as a correct, tested foundation to build that on, not as something to point at a funded account today.

## Before going anywhere near real funds

Read the design doc's compliance section. This is engineering, not legal, compliance, or investment advice — venue terms of service, geofencing, and market-manipulation rules are real constraints that need a real review, and every API detail in the Kalshi/Polymarket adapters should be re-verified against current documentation before use, since both venues' APIs evolve.

## License

MIT — see [LICENSE](LICENSE).
