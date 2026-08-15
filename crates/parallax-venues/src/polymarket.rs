use crate::adapter::VenueAdapter;
use crate::http::{client, json_or_error, RateLimiter};
use crate::rounding::{round_lot, round_price};
use crate::symbol_registry::SymbolRegistry;
use async_trait::async_trait;
use parallax_types::{
    ClientOrderId, ExecError, OrderAck, OrderId, OrderIntent, OrderType, SettlementModel,
    VenueCapabilities, VenueId,
};
use serde_json::Value;
use std::sync::Arc;

/// Polymarket's CLOB order authentication is two-layered (per
/// docs.polymarket.com, verified 2026-08): an L1 EIP-712 signature over
/// the order itself using the trading wallet's private key, plus L2
/// `POLY_*` headers derived from API credentials that were themselves
/// generated via an L1-signed request. Matching happens off-chain on
/// Polymarket's CLOB operator; settlement batches on Polygon separately
/// (design doc §2/§9) — a confirmed match is not the same event as
/// on-chain settlement finality.
///
/// EIP-712/secp256k1 signing over a wallet that can move real funds is
/// deliberately NOT implemented inline here — wire in `py-clob-client`'s
/// signing logic, `ethers-rs`, or your own audited signer before enabling
/// live order submission.
pub trait PolymarketOrderSigner: Send + Sync {
    /// Returns the L2 `POLY_*` header set for a request, and the L1-signed
    /// order payload ready to submit.
    fn sign_order(&self, order_json: &Value) -> Result<PolymarketSignedOrder, String>;
}

pub struct PolymarketSignedOrder {
    pub signed_order_json: Value,
    pub poly_address: String,
    pub poly_signature: String,
    pub poly_timestamp: String,
    pub poly_api_key: String,
    pub poly_passphrase: String,
}

pub struct UnconfiguredPolymarketSigner;

impl PolymarketOrderSigner for UnconfiguredPolymarketSigner {
    fn sign_order(&self, _order_json: &Value) -> Result<PolymarketSignedOrder, String> {
        Err("no PolymarketOrderSigner configured — order submission is disabled until one is supplied".into())
    }
}

pub struct PolymarketAdapter {
    http: reqwest::Client,
    clob_base_url: String,
    gamma_base_url: String,
    signer: Arc<dyn PolymarketOrderSigner>,
    symbols: Arc<SymbolRegistry>,
    rate_limiter: RateLimiter,
}

impl PolymarketAdapter {
    /// `clob_base_url` defaults to `https://clob.polymarket.com` (order
    /// book state and order management) and `gamma_base_url` to
    /// `https://gamma-api.polymarket.com` (event/market discovery) — the
    /// two production hosts documented at docs.polymarket.com as of
    /// 2026-08. `symbols` is shared with whatever subscribes to
    /// Polymarket's listings (design doc review 1.6): each outcome trades
    /// as its own token, so the mapping is keyed on (contract, outcome),
    /// not contract alone.
    pub fn new(signer: Arc<dyn PolymarketOrderSigner>, symbols: Arc<SymbolRegistry>) -> Self {
        PolymarketAdapter {
            http: client(),
            clob_base_url: "https://clob.polymarket.com".to_string(),
            gamma_base_url: "https://gamma-api.polymarket.com".to_string(),
            signer,
            symbols,
            // 2 of the 8 tokens/sec are held back for cancel requests
            // specifically — docs/GOING-LIVE.md Stage 2: running out of
            // cancel capacity while holding live quotes during a fault is
            // the worst reachable state.
            rate_limiter: RateLimiter::with_reserved_for_cancel(8, 2),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.clob_base_url = base_url.into();
        self
    }

    pub fn with_gamma_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.gamma_base_url = base_url.into();
        self
    }

    pub fn symbols(&self) -> &SymbolRegistry {
        &self.symbols
    }

    /// `GET /markets?active=true&closed=false&order=volume24hr` on the
    /// Gamma API — public, unauthenticated market discovery. Each
    /// returned market's `clobTokenIds` field is what `fetch_book_raw`
    /// needs to look up that market's live order book on the CLOB.
    pub async fn fetch_active_markets_raw(&self, limit: u32) -> Result<Value, ExecError> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/markets", self.gamma_base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("active", "true"),
                ("closed", "false"),
                ("order", "volume24hr"),
                ("ascending", "false"),
            ])
            .query(&[("limit", limit)])
            .send()
            .await
            .map_err(|e| ExecError::Connection {
                venue: VenueId::Polymarket,
                message: e.to_string(),
            })?;
        json_or_error(resp, VenueId::Polymarket).await
    }

    /// `GET /book?token_id=...` — public, unauthenticated.
    pub async fn fetch_book_raw(&self, token_id: &str) -> Result<Value, ExecError> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/book", self.clob_base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await
            .map_err(|e| ExecError::Connection {
                venue: VenueId::Polymarket,
                message: e.to_string(),
            })?;
        json_or_error(resp, VenueId::Polymarket).await
    }
}

/// Unlike Kalshi, Polymarket's `/book` response gives bids and asks
/// directly for the requested `token_id` (one outcome token), each as
/// `{"price": "0.xx", "size": "n"}` levels — no complementary-side
/// derivation needed. Best bid is the highest bid price, best ask the
/// lowest ask price.
pub fn parse_book(json: &Value) -> Result<(f64, f64, f64, f64), String> {
    let bids = json
        .get("bids")
        .and_then(Value::as_array)
        .ok_or("response missing `bids` array")?;
    let asks = json
        .get("asks")
        .and_then(Value::as_array)
        .ok_or("response missing `asks` array")?;

    let best_bid = best_level(bids, true).ok_or("no bid levels")?;
    let best_ask = best_level(asks, false).ok_or("no ask levels")?;

    Ok((best_bid.0, best_bid.1, best_ask.0, best_ask.1))
}

fn best_level(levels: &[Value], want_max: bool) -> Option<(f64, f64)> {
    levels
        .iter()
        .filter_map(|level| {
            let price = field_as_f64(level, "price")?;
            let size = field_as_f64(level, "size")?;
            Some((price, size))
        })
        .fold(None, |acc, (p, s)| match acc {
            None => Some((p, s)),
            Some((bp, bs)) => {
                let better = if want_max { p > bp } else { p < bp };
                if better {
                    Some((p, s))
                } else {
                    Some((bp, bs))
                }
            }
        })
}

fn field_as_f64(v: &Value, key: &str) -> Option<f64> {
    let field = v.get(key)?;
    field
        .as_f64()
        .or_else(|| field.as_str().and_then(|s| s.parse::<f64>().ok()))
}

#[async_trait]
impl VenueAdapter for PolymarketAdapter {
    fn venue_id(&self) -> VenueId {
        VenueId::Polymarket
    }

    fn capabilities(&self) -> VenueCapabilities {
        VenueCapabilities {
            venue: VenueId::Polymarket,
            settlement: SettlementModel::OffChainMatchOnChainSettle,
            // Polymarket's own /book response reports tick_size per
            // market (it varies); 0.01 is a conservative placeholder
            // until that's read per-market at subscribe time.
            min_tick: 0.01,
            min_order_size: 5.0,
            fee_model: parallax_types::FeeModel::polymarket_default(),
            rate_limit_per_sec: 10,
        }
    }

    /// See `KalshiAdapter::submit` for why this stops short of the live
    /// HTTP call: the exact `POST /order` payload shape needs
    /// confirmation against `py-clob-client` or the current API
    /// reference, and EIP-712 order signing needs a real signer wired in
    /// via `PolymarketOrderSigner` — neither should be guessed at when
    /// the target is a venue that moves real funds.
    async fn submit(&self, order: OrderIntent) -> Result<OrderAck, ExecError> {
        order.validate().map_err(|e| ExecError::Rejected {
            venue: VenueId::Polymarket,
            reason: e.to_string(),
        })?;

        let token_id = self
            .symbols
            .lookup(VenueId::Polymarket, &order.contract, order.outcome)
            .ok_or_else(|| ExecError::Rejected {
                venue: VenueId::Polymarket,
                reason: format!(
                    "no venue symbol mapping registered for {} ({:?}) — subscribe before trading it",
                    order.contract, order.outcome
                ),
            })?;

        self.rate_limiter.acquire().await;

        let caps = self.capabilities();
        let rounded_price = round_price(order.price, caps.min_tick, order.side);
        let lot =
            round_lot(order.size, caps.min_order_size).ok_or_else(|| ExecError::Rejected {
                venue: VenueId::Polymarket,
                reason: format!(
                    "size {} rounds below the venue minimum {}",
                    order.size, caps.min_order_size
                ),
            })?;
        let client_order_id = ClientOrderId::derive(&order);

        let order_json = serde_json::json!({
            "tokenID": token_id,
            "price": rounded_price,
            "size": lot,
            "side": if order.side == parallax_types::Side::Buy { "BUY" } else { "SELL" },
            "clientOrderId": client_order_id.0,
            // `orderType` was previously dropped entirely — an IOC would
            // have rested indefinitely instead of canceling its unfilled
            // remainder (design doc review 3.15). GTC/FOK naming follows
            // Polymarket's CLOB docs as of 2026-08; re-verify before use.
            "orderType": match order.order_type {
                OrderType::Limit => "GTC",
                OrderType::ImmediateOrCancel => "FOK",
            },
        });
        let _signed =
            self.signer
                .sign_order(&order_json)
                .map_err(|reason| ExecError::Rejected {
                    venue: VenueId::Polymarket,
                    reason,
                })?;

        Err(ExecError::Connection {
            venue: VenueId::Polymarket,
            message: "live order submission requires a verified request body — see module docs"
                .into(),
        })
    }

    /// Gated behind the signer with the same discipline as `submit`
    /// (design doc review 3.5): a market maker that cannot cancel
    /// accumulates resting ladders it can never retract.
    async fn cancel(&self, order_id: OrderId) -> Result<(), ExecError> {
        // Not `acquire()`: a cancel is exactly the request the reserved
        // tokens exist for, and it must never queue behind ordinary
        // traffic that has already exhausted the shared budget.
        self.rate_limiter.acquire_for_cancel().await;
        let cancel_json = serde_json::json!({ "orderID": order_id.0 });
        let _signed =
            self.signer
                .sign_order(&cancel_json)
                .map_err(|reason| ExecError::Rejected {
                    venue: VenueId::Polymarket,
                    reason,
                })?;
        Err(ExecError::NotFound(order_id))
    }

    /// Not yet wired to a live call, for the same reason `submit` isn't:
    /// the query shape hasn't been exercised against a live CLOB
    /// endpoint. Refusing loudly here is the correct behavior for
    /// `execution::submit_idempotent` — it must never treat "we don't
    /// know" as "the venue has no record," which is the one condition
    /// that licenses a resend.
    async fn find_order_by_client_id(
        &self,
        _client_order_id: &parallax_types::ClientOrderId,
    ) -> Result<Option<OrderAck>, ExecError> {
        Err(ExecError::Connection {
            venue: VenueId::Polymarket,
            message: "order lookup by client_order_id is not yet implemented for Polymarket — verify against the CLOB order-status endpoint before wiring into live idempotent retry".into(),
        })
    }

    /// Not yet wired to a live call — same reasoning as
    /// `find_order_by_client_id` above.
    async fn fetch_positions(&self) -> Result<Vec<parallax_types::Position>, ExecError> {
        Err(ExecError::Connection {
            venue: VenueId::Polymarket,
            message: "position fetch is not yet implemented for Polymarket — verify against the Data API's positions endpoint before wiring into live reconciliation".into(),
        })
    }

    /// Not yet wired to a live call — same reasoning as
    /// `find_order_by_client_id` above. This is the one an out-of-band
    /// cancel-all tool needs most, and it stays refused for the same
    /// reason: guessing at the query shape for a script whose entire
    /// purpose is emergency order cancellation is exactly backwards.
    async fn list_open_orders(&self) -> Result<Vec<OrderId>, ExecError> {
        Err(ExecError::Connection {
            venue: VenueId::Polymarket,
            message: "open-order listing is not yet implemented for Polymarket — verify against the CLOB open-orders endpoint before wiring into live reconciliation or cancel-all".into(),
        })
    }
}

/// docs/GOING-LIVE.md Stage 2: "Polymarket's CLOB has a heartbeat
/// endpoint... wire it before your first live order, not after." Not yet
/// wired to a live call, for the same reason every other query method on
/// this adapter isn't: the heartbeat endpoint's exact shape and required
/// interval need verifying against current CLOB documentation before
/// this is safe to depend on — the highest-value safety control in
/// Stage 2 is also the last one that should ship unverified.
#[async_trait]
impl crate::deadman::DeadmanSwitch for PolymarketAdapter {
    async fn heartbeat(&self) -> Result<(), ExecError> {
        Err(ExecError::Connection {
            venue: VenueId::Polymarket,
            message: "the CLOB heartbeat/dead-man-switch endpoint is not yet implemented — verify its shape and required interval against current CLOB documentation before wiring this in live".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_book_response_with_direct_bids_and_asks() {
        let json = serde_json::json!({
            "market": "0xabc",
            "asset_id": "123",
            "bids": [{"price": "0.45", "size": "100"}, {"price": "0.44", "size": "200"}],
            "asks": [{"price": "0.46", "size": "150"}, {"price": "0.47", "size": "250"}],
            "min_order_size": "5",
            "tick_size": "0.01",
        });
        let (bid, bid_size, ask, ask_size) = parse_book(&json).unwrap();
        assert_eq!(bid, 0.45);
        assert_eq!(bid_size, 100.0);
        assert_eq!(ask, 0.46);
        assert_eq!(ask_size, 150.0);
    }

    #[test]
    fn missing_fields_are_a_clear_error() {
        let json = serde_json::json!({ "market": "0xabc" });
        assert!(parse_book(&json).is_err());
    }

    fn sample_order() -> OrderIntent {
        OrderIntent {
            venue: VenueId::Polymarket,
            contract: parallax_types::CanonicalContractId(
                "wx.temp.chicago.gt_869.2026-08-12.nws_official".into(),
            ),
            outcome: parallax_types::Outcome::Yes,
            side: parallax_types::Side::Buy,
            price: 0.6,
            size: 10.0,
            order_type: OrderType::Limit,
            engine: parallax_types::EngineId::MarketMaking,
            created_at: parallax_types::Timestamp::from_nanos(0),
        }
    }

    #[tokio::test]
    async fn submit_refuses_without_a_configured_signer() {
        let adapter = PolymarketAdapter::new(
            Arc::new(UnconfiguredPolymarketSigner),
            Arc::new(SymbolRegistry::new()),
        );
        assert!(adapter.submit(sample_order()).await.is_err());
    }

    #[tokio::test]
    async fn submit_refuses_when_no_symbol_mapping_is_registered() {
        struct AlwaysSigns;
        impl PolymarketOrderSigner for AlwaysSigns {
            fn sign_order(&self, order_json: &Value) -> Result<PolymarketSignedOrder, String> {
                Ok(PolymarketSignedOrder {
                    signed_order_json: order_json.clone(),
                    poly_address: "0x0".into(),
                    poly_signature: "sig".into(),
                    poly_timestamp: "0".into(),
                    poly_api_key: "key".into(),
                    poly_passphrase: "pass".into(),
                })
            }
        }
        let adapter =
            PolymarketAdapter::new(Arc::new(AlwaysSigns), Arc::new(SymbolRegistry::new()));
        match adapter.submit(sample_order()).await {
            Err(ExecError::Rejected { reason, .. }) => {
                assert!(reason.contains("no venue symbol mapping"))
            }
            other => panic!("expected a symbol-mapping rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_refuses_without_a_configured_signer() {
        let adapter = PolymarketAdapter::new(
            Arc::new(UnconfiguredPolymarketSigner),
            Arc::new(SymbolRegistry::new()),
        );
        assert!(adapter.cancel(OrderId("x".into())).await.is_err());
    }
}
