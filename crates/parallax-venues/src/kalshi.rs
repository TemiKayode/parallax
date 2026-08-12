use crate::adapter::VenueAdapter;
use async_trait::async_trait;
use parallax_types::{
    ExecError, OrderAck, OrderId, OrderIntent, OrderType, SettlementModel, Side, VenueCapabilities,
    VenueId,
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
}

impl KalshiAdapter {
    /// `base_url` defaults to the production REST host documented at
    /// docs.kalshi.com/getting_started/api_environments as of 2026-08:
    /// `https://external-api.kalshi.com/trade-api/v2`. Override for the
    /// demo/sandbox environment during testing.
    pub fn new(signer: Arc<dyn KalshiRequestSigner>) -> Self {
        KalshiAdapter {
            http: reqwest::Client::new(),
            base_url: "https://external-api.kalshi.com/trade-api/v2".to_string(),
            signer,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// `GET /markets/{ticker}/orderbook` — public, unauthenticated.
    /// Returns the raw JSON body; use `parse_orderbook` to normalize it.
    pub async fn fetch_orderbook_raw(&self, ticker: &str) -> Result<Value, ExecError> {
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
        resp.json::<Value>()
            .await
            .map_err(|e| ExecError::Connection {
                venue: VenueId::Kalshi,
                message: e.to_string(),
            })
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
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
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
            // Kalshi's fee schedule is a nonlinear per-contract curve, not
            // a flat bps rate — this placeholder must be replaced with
            // the real fee function before any PnL estimate is trusted.
            maker_fee_bps: 0.0,
            taker_fee_bps: 0.0,
            rate_limit_per_sec: 10,
        }
    }

    /// Structurally complete (auth header derivation, request body shape)
    /// but deliberately does not perform the live HTTP call yet: the
    /// order-creation payload shape below is reconstructed from public
    /// documentation rather than exercised against a live endpoint, and
    /// shipping unverified field names against an endpoint that moves
    /// real money is the wrong tradeoff for a reference implementation.
    /// Wire up the final `self.http.post(...)` call once the body has
    /// been confirmed against the current API reference (or the venue's
    /// official SDK) and tested against the demo/sandbox environment.
    async fn submit(&self, order: OrderIntent) -> Result<OrderAck, ExecError> {
        let path = "/portfolio/orders";
        let timestamp_ms = order.created_at.as_nanos() / 1_000_000;
        let _headers = self
            .signer
            .sign("POST", path, timestamp_ms)
            .map_err(|reason| ExecError::Rejected {
                venue: VenueId::Kalshi,
                reason,
            })?;

        // Field names below follow docs.kalshi.com/api-reference/orders/create-order-v2
        // as researched 2026-08: `ticker`, `client_order_id`, `side`
        // ("yes"/"no"), `action` ("buy"/"sell"), `count`, `type`
        // ("limit"/"market"), `price` in integer cents, `time_in_force`.
        // This must be re-verified against the live schema before use —
        // Kalshi has changed this payload shape between API versions.
        let _body = serde_json::json!({
            "ticker": order.contract.0,
            "action": if order.side == Side::Buy { "buy" } else { "sell" },
            "side": "yes",
            "count": order.size as i64,
            "type": match order.order_type {
                OrderType::Limit => "limit",
                OrderType::ImmediateOrCancel => "limit",
            },
            "price": (order.price * 100.0).round() as i64,
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

    async fn cancel(&self, order_id: OrderId) -> Result<(), ExecError> {
        Err(ExecError::NotFound(order_id))
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

    #[tokio::test]
    async fn submit_refuses_without_a_configured_signer() {
        let adapter = KalshiAdapter::new(Arc::new(UnconfiguredKalshiSigner));
        let order = OrderIntent {
            venue: VenueId::Kalshi,
            contract: parallax_types::CanonicalContractId(
                "wx.temp.chicago.gt.869.2026-08-12.nws_official".into(),
            ),
            outcome: parallax_types::Outcome::Yes,
            side: Side::Buy,
            price: 0.6,
            size: 10.0,
            order_type: OrderType::Limit,
            engine: parallax_types::EngineId::MarketMaking,
            created_at: parallax_types::Timestamp::from_nanos(0),
        };
        let result = adapter.submit(order).await;
        assert!(
            result.is_err(),
            "submit must refuse to send a live order with no signer configured"
        );
    }
}
