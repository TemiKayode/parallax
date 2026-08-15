use crate::adapter::VenueAdapter;
use crate::http::{client, json_or_error, RateLimiter};
use crate::rounding::{round_lot, round_price};
use crate::symbol_registry::SymbolRegistry;
use async_trait::async_trait;
use parallax_types::{
    ClientOrderId, ExecError, OrderAck, OrderId, OrderIntent, OrderType, SettlementModel, Side,
    Timestamp, VenueCapabilities, VenueId,
};
use serde_json::Value;
use std::sync::Arc;

/// Kalshi's request-signing scheme (per docs.kalshi.com, verified
/// 2026-08): API-key + RSA-PSS signature over `timestamp + method + path`,
/// sent as the `KALSHI-ACCESS-KEY` / `KALSHI-ACCESS-TIMESTAMP` /
/// `KALSHI-ACCESS-SIGNATURE` headers. Kalshi separately offers a FIX
/// 50SP2 order-entry gateway to qualifying accounts (see design doc §2 —
/// this is a correction to the original "no FIX" assumption) and AWS
/// PrivateLink for Premier-tier+ private connectivity; neither changes
/// what's implemented here, which targets the public REST/WS surface.
///
/// RSA-PSS signing over a live trading key is security-sensitive and
/// deliberately NOT implemented inline in this reference repo — wire in
/// an implementation backed by your own key management (HSM, KMS, or
/// Kalshi's official SDK) before enabling live order submission.
pub trait KalshiRequestSigner: Send + Sync {
    fn sign(
        &self,
        method: &str,
        path: &str,
        timestamp_ms: i64,
    ) -> Result<KalshiAuthHeaders, String>;
}

pub struct KalshiAuthHeaders {
    pub access_key: String,
    pub timestamp_ms: i64,
    pub signature_base64: String,
}

/// The default signer: refuses every request. This is the safety rail —
/// `KalshiAdapter` cannot accidentally submit a live order until a real
/// signer is explicitly configured.
pub struct UnconfiguredKalshiSigner;

impl KalshiRequestSigner for UnconfiguredKalshiSigner {
    fn sign(
        &self,
        _method: &str,
        _path: &str,
        _timestamp_ms: i64,
    ) -> Result<KalshiAuthHeaders, String> {
        Err("no KalshiRequestSigner configured — order submission is disabled until one is supplied".into())
    }
}

pub struct KalshiAdapter {
    http: reqwest::Client,
    base_url: String,
    signer: Arc<dyn KalshiRequestSigner>,
    symbols: Arc<SymbolRegistry>,
    rate_limiter: RateLimiter,
}

impl KalshiAdapter {
    /// `base_url` defaults to the production REST host documented at
    /// docs.kalshi.com/getting_started/api_environments as of 2026-08:
    /// `https://external-api.kalshi.com/trade-api/v2`. Override for the
    /// demo/sandbox environment during testing. `symbols` is shared with
    /// whatever subscribes to Kalshi's listings, so a mapping populated
    /// there is immediately visible to submission here (design doc review
    /// 1.6).
    pub fn new(signer: Arc<dyn KalshiRequestSigner>, symbols: Arc<SymbolRegistry>) -> Self {
        KalshiAdapter {
            http: client(),
            base_url: "https://external-api.kalshi.com/trade-api/v2".to_string(),
            signer,
            symbols,
            // Published limit is higher; this self-throttles well below it
            // rather than skating against it (design doc review 3.4).
            rate_limiter: RateLimiter::new(8),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn symbols(&self) -> &SymbolRegistry {
        &self.symbols
    }

    /// `GET /markets?series_ticker=...&status=open` — public,
    /// unauthenticated. Lets a caller discover a live, currently-open
    /// market for a series (e.g. `KXHIGHCHI`, Kalshi's real "highest
    /// temperature in Chicago" series) instead of hardcoding a specific
    /// dated ticker that would go stale the moment that market closes.
    pub async fn fetch_open_markets_for_series_raw(
        &self,
        series_ticker: &str,
        limit: u32,
    ) -> Result<Value, ExecError> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/markets", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("series_ticker", series_ticker), ("status", "open")])
            .query(&[("limit", limit)])
            .send()
            .await
            .map_err(|e| ExecError::Connection {
                venue: VenueId::Kalshi,
                message: e.to_string(),
            })?;
        json_or_error(resp, VenueId::Kalshi).await
    }

    /// `GET /markets/{ticker}/orderbook` — public, unauthenticated.
    /// Returns the raw JSON body; use `parse_orderbook` to normalize it.
    pub async fn fetch_orderbook_raw(&self, ticker: &str) -> Result<Value, ExecError> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/markets/{}/orderbook", self.base_url, ticker);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| ExecError::Connection {
                venue: VenueId::Kalshi,
                message: e.to_string(),
            })?;
        json_or_error(resp, VenueId::Kalshi).await
    }
}

/// Kalshi's orderbook shows resting BUY depth on both the YES and NO
/// side (there is no separate "ask" array): a resting bid to buy NO at
/// price Q is economically the same offer as being willing to sell YES
/// at `1 - Q`, since a YES contract and a NO contract on the same market
/// always sum to $1 at settlement. So the effective best ask for YES is
/// `1 - (best NO bid)`. This function accepts either the `orderbook` or
/// `orderbook_fp`/`*_dollars` wrapper naming reported across API
/// versions, and both string and numeric price/size encodings — but the
/// exact response shape should be reverified against a live call before
/// production use; it was not exercised against a live endpoint while
/// building this reference implementation.
pub fn parse_orderbook(json: &Value) -> Result<(f64, f64, f64, f64), String> {
    let book = json
        .get("orderbook")
        .or_else(|| json.get("orderbook_fp"))
        .ok_or("response missing an `orderbook`/`orderbook_fp` field")?;
    let yes = book
        .get("yes")
        .or_else(|| book.get("yes_dollars"))
        .and_then(Value::as_array)
        .ok_or("orderbook missing `yes`/`yes_dollars` levels")?;
    let no = book
        .get("no")
        .or_else(|| book.get("no_dollars"))
        .and_then(Value::as_array)
        .ok_or("orderbook missing `no`/`no_dollars` levels")?;

    let best_yes_bid = best_level(yes).ok_or("no resting yes bid levels")?;
    let best_no_bid = best_level(no).ok_or("no resting no bid levels")?;

    let bid = best_yes_bid.0;
    let bid_size = best_yes_bid.1;
    let ask = 1.0 - best_no_bid.0;
    let ask_size = best_no_bid.1;
    Ok((bid, bid_size, ask, ask_size))
}

fn best_level(levels: &[Value]) -> Option<(f64, f64)> {
    levels
        .iter()
        .filter_map(|level| {
            let arr = level.as_array()?;
            let price = number_at(arr, 0)?;
            let size = number_at(arr, 1)?;
            Some((price, size))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
}

fn number_at(arr: &[Value], idx: usize) -> Option<f64> {
    let v = arr.get(idx)?;
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

#[async_trait]
impl VenueAdapter for KalshiAdapter {
    fn venue_id(&self) -> VenueId {
        VenueId::Kalshi
    }

    fn capabilities(&self) -> VenueCapabilities {
        VenueCapabilities {
            venue: VenueId::Kalshi,
            settlement: SettlementModel::CentralLimitOrderBook,
            // Kalshi quotes in whole cents -> 0.01 in probability space.
            min_tick: 0.01,
            min_order_size: 1.0,
            fee_model: parallax_types::FeeModel::kalshi_default(),
            rate_limit_per_sec: 10,
        }
    }

    /// Structurally complete (auth header derivation, request body shape,
    /// idempotency key, venue symbol lookup) but deliberately does not
    /// perform the live HTTP call yet: the order-creation payload shape
    /// below is reconstructed from public documentation rather than
    /// exercised against a live endpoint, and shipping unverified field
    /// names against an endpoint that moves real money is the wrong
    /// tradeoff for a reference implementation. Wire up the final
    /// `self.http.post(...)` call once the body has been confirmed
    /// against the current API reference (or the venue's official SDK)
    /// and tested against the demo/sandbox environment.
    async fn submit(&self, order: OrderIntent) -> Result<OrderAck, ExecError> {
        order.validate().map_err(|e| ExecError::Rejected {
            venue: VenueId::Kalshi,
            reason: e.to_string(),
        })?;

        let symbol = self
            .symbols
            .lookup(VenueId::Kalshi, &order.contract, order.outcome)
            .ok_or_else(|| ExecError::Rejected {
                venue: VenueId::Kalshi,
                reason: format!(
                    "no venue symbol mapping registered for {} ({:?}) — subscribe before trading it",
                    order.contract, order.outcome
                ),
            })?;

        self.rate_limiter.acquire().await;

        let path = "/portfolio/orders";
        // Signed at send time, not `order.created_at`: a queued order
        // carries a stale timestamp and would be rejected by the venue's
        // signature window (design doc review 3.16).
        let timestamp_ms = Timestamp::now().as_nanos() / 1_000_000;
        let _headers = self
            .signer
            .sign("POST", path, timestamp_ms)
            .map_err(|reason| ExecError::Rejected {
                venue: VenueId::Kalshi,
                reason,
            })?;

        let caps = self.capabilities();
        let rounded_price = round_price(order.price, caps.min_tick, order.side);
        let lot =
            round_lot(order.size, caps.min_order_size).ok_or_else(|| ExecError::Rejected {
                venue: VenueId::Kalshi,
                reason: format!(
                    "size {} rounds below the venue minimum {}",
                    order.size, caps.min_order_size
                ),
            })?;
        let client_order_id = ClientOrderId::derive(&order);

        // Field names below follow docs.kalshi.com/api-reference/orders/create-order-v2
        // as researched 2026-08: `ticker`, `client_order_id`, `side`
        // ("yes"/"no"), `action` ("buy"/"sell"), `count`, `type`
        // ("limit"/"market"), `price` in integer cents, `time_in_force`.
        // This must be re-verified against the live schema before use —
        // Kalshi has changed this payload shape between API versions.
        let _body = serde_json::json!({
            "ticker": symbol,
            "client_order_id": client_order_id.0,
            "action": if order.side == Side::Buy { "buy" } else { "sell" },
            // The venue's `side` field is the outcome (YES/NO), not the
            // buy/sell action — hardcoding "yes" here submitted every NO
            // order as YES (design doc review 3.14).
            "side": match order.outcome {
                parallax_types::Outcome::Yes => "yes",
                parallax_types::Outcome::No => "no",
            },
            "count": lot,
            "type": match order.order_type {
                OrderType::Limit => "limit",
                OrderType::ImmediateOrCancel => "limit",
            },
            "price": (rounded_price * 100.0).round() as i64,
            "time_in_force": match order.order_type {
                OrderType::Limit => "resting",
                OrderType::ImmediateOrCancel => "immediate_or_cancel",
            },
        });

        Err(ExecError::Connection {
            venue: VenueId::Kalshi,
            message: "live order submission requires a configured KalshiRequestSigner and a verified request body — see module docs".into(),
        })
    }

    /// Gated behind the signer with the same discipline as `submit` — a
    /// market maker that cannot cancel accumulates resting ladders it can
    /// never retract, which is as safety-critical as submission itself
    /// (design doc review 3.5).
    async fn cancel(&self, order_id: OrderId) -> Result<(), ExecError> {
        let path = format!("/portfolio/orders/{}", order_id.0);
        self.rate_limiter.acquire().await;
        let timestamp_ms = Timestamp::now().as_nanos() / 1_000_000;
        let _headers = self
            .signer
            .sign("DELETE", &path, timestamp_ms)
            .map_err(|reason| ExecError::Rejected {
                venue: VenueId::Kalshi,
                reason,
            })?;
        Err(ExecError::NotFound(order_id))
    }

    /// Not yet wired to a live call, for the same reason `submit` isn't:
    /// the query shape (`GET /portfolio/orders?client_order_id=...`, per
    /// public documentation) hasn't been exercised against a live
    /// endpoint. Refusing loudly here is the correct behavior for
    /// `execution::submit_idempotent` — it must never treat "we don't
    /// know" as "the venue has no record," which is the one condition
    /// that licenses a resend.
    async fn find_order_by_client_id(
        &self,
        _client_order_id: &parallax_types::ClientOrderId,
    ) -> Result<Option<OrderAck>, ExecError> {
        Err(ExecError::Connection {
            venue: VenueId::Kalshi,
            message: "order lookup by client_order_id is not yet implemented for Kalshi — verify GET /portfolio/orders against docs.kalshi.com before wiring into live idempotent retry".into(),
        })
    }

    /// Not yet wired to a live call — same reasoning as
    /// `find_order_by_client_id` above, for `GET /portfolio/positions`.
    async fn fetch_positions(&self) -> Result<Vec<parallax_types::Position>, ExecError> {
        Err(ExecError::Connection {
            venue: VenueId::Kalshi,
            message: "position fetch is not yet implemented for Kalshi — verify GET /portfolio/positions against docs.kalshi.com before wiring into live reconciliation".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_orderbook_fp_shape_with_string_prices() {
        // Shape reported by public documentation/tooling as of 2026-08
        // (see module docs for the caveat on reverifying this live).
        let json = serde_json::json!({
            "orderbook_fp": {
                "yes_dollars": [["0.62", "150"], ["0.61", "80"]],
                "no_dollars": [["0.35", "120"], ["0.34", "60"]],
            }
        });
        let (bid, bid_size, ask, ask_size) = parse_orderbook(&json).unwrap();
        assert_eq!(bid, 0.62);
        assert_eq!(bid_size, 150.0);
        // best no bid is 0.35 -> effective yes ask = 1 - 0.35 = 0.65
        assert!((ask - 0.65).abs() < 1e-9);
        assert_eq!(ask_size, 120.0);
    }

    #[test]
    fn parses_plain_orderbook_shape_with_numeric_prices() {
        let json = serde_json::json!({
            "orderbook": {
                "yes": [[0.58, 40]],
                "no": [[0.30, 25]],
            }
        });
        let (bid, bid_size, ask, ask_size) = parse_orderbook(&json).unwrap();
        assert_eq!(bid, 0.58);
        assert_eq!(bid_size, 40.0);
        assert!((ask - 0.70).abs() < 1e-9);
        assert_eq!(ask_size, 25.0);
    }

    #[test]
    fn missing_orderbook_field_is_a_clear_error_not_a_panic() {
        let json = serde_json::json!({ "unexpected": true });
        assert!(parse_orderbook(&json).is_err());
    }

    fn sample_order() -> OrderIntent {
        OrderIntent {
            venue: VenueId::Kalshi,
            contract: parallax_types::CanonicalContractId(
                "wx.temp.chicago.gt_869.2026-08-12.nws_official".into(),
            ),
            outcome: parallax_types::Outcome::Yes,
            side: Side::Buy,
            price: 0.6,
            size: 10.0,
            order_type: OrderType::Limit,
            engine: parallax_types::EngineId::MarketMaking,
            created_at: parallax_types::Timestamp::from_nanos(0),
        }
    }

    #[tokio::test]
    async fn submit_refuses_without_a_configured_signer() {
        let adapter = KalshiAdapter::new(
            Arc::new(UnconfiguredKalshiSigner),
            Arc::new(SymbolRegistry::new()),
        );
        let result = adapter.submit(sample_order()).await;
        assert!(
            result.is_err(),
            "submit must refuse to send a live order with no signer configured"
        );
    }

    #[tokio::test]
    async fn submit_refuses_when_no_symbol_mapping_is_registered() {
        struct AlwaysSigns;
        impl KalshiRequestSigner for AlwaysSigns {
            fn sign(
                &self,
                _: &str,
                _: &str,
                timestamp_ms: i64,
            ) -> Result<KalshiAuthHeaders, String> {
                Ok(KalshiAuthHeaders {
                    access_key: "k".into(),
                    timestamp_ms,
                    signature_base64: "sig".into(),
                })
            }
        }
        let adapter = KalshiAdapter::new(Arc::new(AlwaysSigns), Arc::new(SymbolRegistry::new()));
        match adapter.submit(sample_order()).await {
            Err(ExecError::Rejected { reason, .. }) => {
                assert!(reason.contains("no venue symbol mapping"))
            }
            other => panic!("expected a symbol-mapping rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_refuses_without_a_configured_signer() {
        let adapter = KalshiAdapter::new(
            Arc::new(UnconfiguredKalshiSigner),
            Arc::new(SymbolRegistry::new()),
        );
        match adapter.cancel(OrderId("x".into())).await {
            Err(ExecError::Rejected { .. }) => {}
            other => panic!("expected Rejected (unconfigured signer), got {other:?}"),
        }
    }
}
