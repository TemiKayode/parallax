use crate::adapter::VenueAdapter;
use async_trait::async_trait;
use parallax_types::{
    ExecError, OrderAck, OrderId, OrderIntent, SettlementModel, VenueCapabilities, VenueId,
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
    signer: Arc<dyn PolymarketOrderSigner>,
}

impl PolymarketAdapter {
    /// `clob_base_url` defaults to `https://clob.polymarket.com`, the
    /// production CLOB API host documented at docs.polymarket.com as of
    /// 2026-08. The Gamma API (`https://gamma-api.polymarket.com`) is a
    /// separate host used for market/event discovery, not order
    /// management, and isn't wired in here.
    pub fn new(signer: Arc<dyn PolymarketOrderSigner>) -> Self {
        PolymarketAdapter {
            http: reqwest::Client::new(),
            clob_base_url: "https://clob.polymarket.com".to_string(),
            signer,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.clob_base_url = base_url.into();
        self
    }

    /// `GET /book?token_id=...` — public, unauthenticated.
    pub async fn fetch_book_raw(&self, token_id: &str) -> Result<Value, ExecError> {
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
        resp.json::<Value>()
            .await
            .map_err(|e| ExecError::Connection {
                venue: VenueId::Polymarket,
                message: e.to_string(),
            })
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
            maker_fee_bps: 0.0,
            taker_fee_bps: 0.0,
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
        let order_json = serde_json::json!({
            "tokenID": order.contract.0,
            "price": order.price,
            "size": order.size,
            "side": if order.side == parallax_types::Side::Buy { "BUY" } else { "SELL" },
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

    async fn cancel(&self, order_id: OrderId) -> Result<(), ExecError> {
        Err(ExecError::NotFound(order_id))
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

    #[tokio::test]
    async fn submit_refuses_without_a_configured_signer() {
        let adapter = PolymarketAdapter::new(Arc::new(UnconfiguredPolymarketSigner));
        let order = OrderIntent {
            venue: VenueId::Polymarket,
            contract: parallax_types::CanonicalContractId(
                "wx.temp.chicago.gt.869.2026-08-12.nws_official".into(),
            ),
            outcome: parallax_types::Outcome::Yes,
            side: parallax_types::Side::Buy,
            price: 0.6,
            size: 10.0,
            order_type: parallax_types::OrderType::Limit,
            engine: parallax_types::EngineId::MarketMaking,
            created_at: parallax_types::Timestamp::from_nanos(0),
        };
        assert!(adapter.submit(order).await.is_err());
    }
}
