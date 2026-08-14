use crate::source::AlphaSource;
use parallax_types::{AlphaEventKind, CanonicalContractId, ProbabilityEstimate, RawEvent};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct OrderFlowPayload {
    contract: String,
    /// Resting size near the touch on the reference asset's own book —
    /// not Polymarket's book, which the edge calculator (design doc §5)
    /// handles separately.
    bid_size: f64,
    ask_size: f64,
}

/// Order-flow imbalance on the reference asset's own book (design doc
/// §4a) — a resting-bid/resting-ask size skew is a well-established,
/// weak, short-horizon directional signal distinct from Polymarket's
/// liquidity, which only matters for execution cost, not for what the
/// asset is about to do. Deliberately a smaller `sensitivity` than
/// `NewsSentimentSource`'s: this is one modest vote among several, not a
/// standalone signal.
pub struct OrderFlowImbalanceSource {
    name: String,
    kinds: [AlphaEventKind; 1],
    pub sensitivity: f64,
    pub std_dev: f64,
    /// Below this much combined resting size, the reading is too thin to
    /// trust at all — silence, not a noisy guess.
    pub min_total_size: f64,
    correlation_group: Option<String>,
}

impl OrderFlowImbalanceSource {
    pub fn new(name: impl Into<String>) -> Self {
        OrderFlowImbalanceSource {
            name: name.into(),
            kinds: [AlphaEventKind::ReferenceAsset],
            sensitivity: 0.10,
            std_dev: 0.18,
            min_total_size: 1.0,
            correlation_group: None,
        }
    }

    /// Marks every estimate this source emits as correlated with every
    /// other estimate sharing the same group (design doc review 2.9).
    pub fn with_correlation_group(mut self, group: impl Into<String>) -> Self {
        self.correlation_group = Some(group.into());
        self
    }
}

impl AlphaSource for OrderFlowImbalanceSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn event_kinds(&self) -> &[AlphaEventKind] {
        &self.kinds
    }

    fn on_event(&self, event: &RawEvent) -> Option<ProbabilityEstimate> {
        if event.kind != AlphaEventKind::ReferenceAsset {
            return None;
        }
        let payload: OrderFlowPayload = serde_json::from_value(event.payload.clone()).ok()?;
        let total = payload.bid_size + payload.ask_size;
        if total < self.min_total_size {
            return None;
        }

        let imbalance = (payload.bid_size - payload.ask_size) / total; // in [-1, 1]
        let probability = (0.5 + self.sensitivity * imbalance).clamp(0.0, 1.0);

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability,
            std_dev: self.std_dev,
            as_of: event.receive_ts,
            kind: parallax_types::EstimateKind::Absolute,
            staleness: parallax_types::StalenessPolicy::Decays,
            correlation_group: self.correlation_group.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::Timestamp;
    use serde_json::json;

    fn event(bid_size: f64, ask_size: f64) -> RawEvent {
        RawEvent {
            source: "binance-btc-book".into(),
            kind: AlphaEventKind::ReferenceAsset,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: json!({
                "contract": "crypto.updown.btc.gt.0.2026-08-12t12-05-00z.binance",
                "bid_size": bid_size,
                "ask_size": ask_size,
            }),
        }
    }

    #[test]
    fn heavier_resting_bids_than_asks_shift_probability_up() {
        let src = OrderFlowImbalanceSource::new("ofi");
        let est = src.on_event(&event(300.0, 100.0)).unwrap();
        assert!(est.probability > 0.5);
    }

    #[test]
    fn heavier_resting_asks_than_bids_shift_probability_down() {
        let src = OrderFlowImbalanceSource::new("ofi");
        let est = src.on_event(&event(100.0, 300.0)).unwrap();
        assert!(est.probability < 0.5);
    }

    #[test]
    fn balanced_book_is_a_coin_flip() {
        let src = OrderFlowImbalanceSource::new("ofi");
        let est = src.on_event(&event(150.0, 150.0)).unwrap();
        assert!((est.probability - 0.5).abs() < 1e-9);
    }

    #[test]
    fn too_thin_a_book_is_silently_ignored_not_a_noisy_guess() {
        let src = OrderFlowImbalanceSource::new("ofi");
        assert!(src.on_event(&event(0.2, 0.1)).is_none());
    }

    #[test]
    fn irrelevant_event_kind_yields_no_opinion() {
        let src = OrderFlowImbalanceSource::new("ofi");
        let mut ev = event(300.0, 100.0);
        ev.kind = AlphaEventKind::Oracle;
        assert!(src.on_event(&ev).is_none());
    }
}
