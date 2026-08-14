# PARALLAX

[![CI](https://github.com/TemiKayode/parallax/actions/workflows/ci.yml/badge.svg)](https://github.com/TemiKayode/parallax/actions/workflows/ci.yml)

A cross-venue, multi-asset, event-driven statistical arbitrageur for prediction markets. Full system design: [PARALLAX — Cross-Venue Prediction-Market Statistical Arbitrage: System Design](https://claude.ai/code/artifact/8922a0e2-37fa-4b76-affb-398dd1487c6e).

This repo is the reference implementation of that design. It's a working skeleton with real, tested logic in every layer — not a mockup: **242 Rust tests and 15 Python tests pass**, in both debug and release, clippy is clean with zero warnings, and the web dashboard pulls **real, live quotes** from Kalshi's and Polymarket's public market-data APIs alongside a CLI/dashboard demo that runs a full tick-to-fill scenario against the real engine, complete with fee-aware fills, a risk gate that rejects some of its own ladder, and a mark-to-market P&L. What it is *not* yet is a live trading system: order *submission* to Kalshi and Polymarket is deliberately incomplete (see [Status](#status)) until request signing and the live payload shape are verified against each venue's current order-entry API — see [Production readiness](#production-readiness) for exactly what that gap means. Live **data** and live **trading** are different claims; this repo makes the first one honestly and does not make the second one at all.

![The PARALLAX dashboard showing live Kalshi and Polymarket quotes, the arbitrage detector, and the backtest runner](docs/dashboard.png)

## Layout

```
crates/
  parallax-types      canonical contract schema, event/order/position types, shared validate() — pure data, no I/O
  parallax-bus         lock-free event bus (crossbeam ArrayQueue) wiring the pipeline's topics, Lossy/Critical topic classification
  parallax-book        consolidated cross-venue order book (deterministic BTreeMap) + direct arbitrage detection
  parallax-alpha       AlphaSource trait, fair-value aggregator, weather/econ/news/oracle sources, offline-config loading, forecast-quality tracking
  parallax-risk        the risk gate: reservation-aware position/notional limits, correlated-cluster netting, price collar, kill switches
  parallax-strategy    market making / stat-arb / liquidity sniping engines + online calibration
  parallax-venues      VenueAdapter trait, PaperAdapter (deterministic matching engine), Kalshi/Polymarket adapters, symbol registry
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
cargo test --workspace          # 242 Rust tests
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
- **Backtest runner** — one button re-runs the full weather-update → stale-quote → fill scenario through the actual `parallax-sim::Backtest` pipeline (alpha aggregation, all three strategy engines, the risk gate, the matching engine) and renders the report: fills, filled volume, realized/unrealized/net PnL, fees paid, open positions. Synthetic data via the in-memory `PaperAdapter`.
- Loopback-only by default (`PARALLAX_UI_ALLOW_REMOTE=1` opts into binding all interfaces), with a request body cap, a bound on concurrent requests, a `/health` endpoint, and a graceful shutdown on Ctrl+C — every route here is unauthenticated, and `/api/backtest` runs real work per request.

It ships zero JS dependencies (plain HTML/CSS/vanilla JS, no build step, no CDN calls). No order is ever placed by any panel — that stays gated behind `UnconfiguredKalshiSigner` / `UnconfiguredPolymarketSigner` regardless of which panel you're using.

## Status

**Solid and tested:**
- Canonical contract normalization across venues (`parallax-types`, `parallax-book`) — proven with tests mapping a Polymarket-style metric listing and a Kalshi-style imperial listing to the identical canonical id, and confirming that case differences, embedded punctuation, and a `Between` contract's own upper bound can no longer collide two economically different bets into one book entry.
- Direct cross-venue arbitrage detection, independent of any model (`parallax-book::detect_arb`), backed by a deterministic (`BTreeMap`) book so the same input always produces the same result.
- The fair-value aggregator: inverse-variance weighting with a disagreement term that widens the confidence band, a staleness decay that only applies to estimates that are actually still unsettled (a finalized oracle resolution doesn't decay), a correlation haircut so N correlated sources don't pool as N independent ones, and a directional log-odds signal (a scored headline) that shifts the pooled estimate instead of voting an absolute opinion into it (`parallax-alpha`).
- Direction-aware alpha sources: the weather and econ sources price whichever of `GreaterThan`/`LessThan`/`Between` the contract actually asks, a Beta-Binomial posterior means a unanimous small ensemble is never reported as exactly certain, and an oracle resolution source that respects UMA-style optimistic-oracle challenge windows (a disputed proposal produces no estimate at all).
- The risk gate: reservations for *working* orders (not just filled positions) so a two-sided quote is charged its real worst case, correlated-cluster netting that's aware of contract direction, notional (not just contract-count) limits, a price collar, session-loss/drawdown auto-trip into the kill switch, and a reconciliation gate that refuses every order until the venue's real position has actually been loaded. `check_batch` prevents two strategy engines that each individually clear a limit from jointly blowing through it in the same tick. The price collar is exercised for real in the demo backtest below: it rejects 6 of the market maker's 16 proposed orders because they'd quote near the fresh, confident fair value while Polymarket's own book is still showing the stale pre-update touch — the same shape of runaway-model/fat-finger order the collar exists to catch.
- The three strategy engines and their triggering logic: market making ladders off the consolidated fair value with a half-spread floored at the real round-trip fee cost; stat-arb sizing by fractional Kelly for a binary contract and firing only when a venue price sits fully outside the confidence band; sniping taking exactly the resting size at a stale quote via IOC, and only once PARALLAX's own fair value postdates the quote being taken.
- `PaperAdapter`, a real in-memory matching engine (not a stub) with deterministic price-time priority, correct marketable-limit/resting-limit/IOC semantics, a resting order filling at *its own* price rather than the aggressor's, and optional queue-position/network-latency modeling for honest (not front-of-queue, not zero-latency) shadow-mode P&L.
- The full pipeline wired together in `parallax-sim::Backtest` and exercised by an integration test, the CLI demo, and the web dashboard: a weather ensemble update shifts the fair value, a stale quote gets bought, the fill lands in the risk gate's position book with a fee charged and slippage recorded, and the report shows realized/unrealized/net PnL, an equity curve with max drawdown, and a histogram of why every rejected order was rejected.
- **Live market-data reads against both venues' real production APIs.** `KalshiAdapter::fetch_open_markets_for_series_raw` + `fetch_orderbook_raw` + `parse_kalshi_orderbook`, and `PolymarketAdapter::fetch_active_markets_raw` + `fetch_book_raw` + `parse_polymarket_book`, were run against `external-api.kalshi.com` and `clob.polymarket.com`/`gamma-api.polymarket.com` while building this — not just unit-tested against fixtures. Both parsers matched the real response shapes without any code changes required.

**Newer and not yet wired into the live pipeline:**
- Three additional alpha sources for crypto up/down markets — `CorrelatedAssetSource` (residual against a driver asset), `OrderFlowImbalanceSource`, `ReferenceDistanceSource` (a barrier-probability model off a reference price) — are implemented and tested (`parallax-alpha`), and can be handed to `Backtest::new` like any other `AlphaSource`, but nothing currently constructs and registers them by default.
- A tradable-edge calculator (`parallax-strategy::edge`, walks real depth rather than pricing off the touch), a position-structure state machine (`parallax-strategy::position_structure`, seven states from flat through hedged-building to unwinding), and a relative-value monitor (`parallax-strategy::relative_value`) are implemented and tested as standalone modules, but no `StrategyEngine` currently consumes them — that's a real design decision (a new engine, or an extension of an existing one) still to be made, not a bug fix.

**Deliberately incomplete, and why:**
- `KalshiAdapter::submit` and `PolymarketAdapter::submit` build a structurally complete request — venue symbol resolved through a `SymbolRegistry`, a deterministic idempotency key, prices/sizes rounded to the venue's own tick/lot grid — but stop short of the live HTTP call. Both venues' order-*creation* payload shapes (as opposed to the market-data read shapes above, which are verified) were reconstructed from public documentation during this build, not exercised against a live endpoint — shipping unverified field names against an API that moves real money is the wrong tradeoff for a reference implementation.
- Both adapters require an explicit `KalshiRequestSigner` / `PolymarketOrderSigner` before they'll even attempt to submit *or cancel* — the default `UnconfiguredKalshiSigner` / `UnconfiguredPolymarketSigner` always refuses. RSA-PSS (Kalshi) and EIP-712 (Polymarket) signing over a key that can move funds is deliberately not hand-rolled here; wire in an audited implementation.
- Live market data is on-demand (fetched when the dashboard loads or you click refresh), not a continuous streaming ingestion loop feeding `parallax-book`/`parallax-strategy` automatically — there's no WebSocket subscription, scheduler, or long-running process wiring live quotes into the risk-gated strategy pipeline yet. No FIX client or FPGA/kernel-bypass anything either — see [§2 of the design doc](https://claude.ai/code/artifact/8922a0e2-37fa-4b76-affb-398dd1487c6e) for why most of that infrastructure doesn't apply to these venues the way the original brief assumed, and what's realistic instead.
- No production deployment (hot-hot redundancy, PTP clock sync) — that's infrastructure work for when there's a real venue connection to make redundant.

## Production readiness

"Production ready" means two different things here, and only one of them is true today.

**As an open-source software project, yes:** the workspace builds clean, `cargo clippy --workspace --all-targets -- -D warnings` passes with zero warnings, `cargo fmt --all -- --check` passes, CI (`.github/workflows/ci.yml`) runs fmt + clippy + the full Rust and Python test suites on every push and PR, it's MIT licensed, and both binaries install and run from anywhere via `cargo install`. That's the bar this repo is held to and meets.

**As a system that trades real money, no — and it shouldn't claim to.** Beyond the signing/payload gaps listed in [Status](#status), going live would still need, at minimum:
- A compliance review of venue ToS, geofencing, and market-manipulation rules for your jurisdiction and account type — see the design doc's compliance section (§16). This is not legal advice and nothing in this repo constitutes it.
- Real request signing (RSA-PSS for Kalshi, EIP-712 for Polymarket) backed by real key management, tested against each venue's sandbox/demo environment before touching production credentials.
- Secrets handling for API keys that isn't "environment variables on a laptop" — a secrets manager, least-privilege access, and key rotation.
- Position reconciliation at startup and on a schedule: `RiskGate::set_position`/`mark_reconciled` exist and the gate refuses to trade until they're called, but the code that actually fetches each venue's real position and working orders at startup doesn't exist yet — reconciliation *readiness* is not reconciliation.
- The hot-hot redundancy, clock discipline, and monitoring/alerting described in design doc §14, none of which exists yet — right now there's one process, one region, and no failover.
- Verification of the fee schedules and rate limits baked into `parallax-types::FeeModel` and each adapter's `capabilities()` against the venues' current published numbers — both venues change these, and a stale fee model silently turns a profitable strategy unprofitable with no error anywhere.

Treat this repo as a correct, tested foundation to build that on, not as something to point at a funded account today.

## Before going anywhere near real funds

Read the design doc's compliance section. This is engineering, not legal, compliance, or investment advice — venue terms of service, geofencing, and market-manipulation rules are real constraints that need a real review, and every API detail in the Kalshi/Polymarket adapters should be re-verified against current documentation before use, since both venues' APIs evolve.

## License

MIT — see [LICENSE](LICENSE).
