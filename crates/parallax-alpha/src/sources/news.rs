use crate::source::AlphaSource;
use parallax_types::{AlphaEventKind, CanonicalContractId, ProbabilityEstimate, RawEvent};
use serde::Deserialize;

/// This source deliberately does NOT implement NLP itself. Headline
/// polarity/entity-relevance extraction is a heavier, separately-trained
/// model (design doc §12: offline Python research) that publishes its
/// output onto the wire; this struct's only job is converting an
/// already-scored headline into a calibrated probability nudge with
/// confidence that decays with both relevance and time (staleness decay
/// is handled by the aggregator via `as_of`, not duplicated here).
#[derive(Debug, Deserialize)]
struct NewsPayload {
    contract: String,
    /// -1.0 (strongly bearish for YES) .. 1.0 (strongly bullish for YES)
    polarity: f64,
    /// 0.0 (headline barely concerns this contract) .. 1.0 (directly on point)
    relevance: f64,
}

pub struct NewsSentimentSource {
    name: String,
    kinds: [AlphaEventKind; 1],
    /// Max probability displacement from 0.5 at polarity=±1, relevance=1.
    sensitivity: f64,
    /// Headlines below this relevance are treated as not-about-this-contract.
    min_relevance: f64,
}

impl NewsSentimentSource {
    pub fn new(name: impl Into<String>) -> Self {
        NewsSentimentSource {
            name: name.into(),
            kinds: [AlphaEventKind::NewsHeadline],
            sensitivity: 0.30,
            min_relevance: 0.15,
        }
    }
}

impl AlphaSource for NewsSentimentSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn event_kinds(&self) -> &[AlphaEventKind] {
        &self.kinds
    }

    fn on_event(&self, event: &RawEvent) -> Option<ProbabilityEstimate> {
        if event.kind != AlphaEventKind::NewsHeadline {
            return None;
        }
        let payload: NewsPayload = serde_json::from_value(event.payload.clone()).ok()?;
        if payload.relevance < self.min_relevance {
            return None;
        }

        let displacement = payload.polarity.clamp(-1.0, 1.0)
            * payload.relevance.clamp(0.0, 1.0)
            * self.sensitivity;
        let probability = (0.5 + displacement).clamp(0.0, 1.0);
        // Low relevance means low confidence: std_dev grows as relevance
        // shrinks, floored so a maximally-relevant headline still isn't
        // treated as certain on its own.
        let std_dev = (0.08 / payload.relevance.max(self.min_relevance)).min(0.5);

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability,
            std_dev,
            as_of: event.receive_ts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::Timestamp;
    use serde_json::json;

    fn event(polarity: f64, relevance: f64) -> RawEvent {
        RawEvent {
            source: "wire".into(),
            kind: AlphaEventKind::NewsHeadline,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: json!({
                "contract": "econ.cpi_yoy.us.gt.30.2026-08-13.bls",
                "polarity": polarity,
                "relevance": relevance,
            }),
        }
    }

    #[test]
    fn strongly_relevant_bullish_headline_moves_probability_up() {
        let src = NewsSentimentSource::new("wire-nlp");
        let est = src.on_event(&event(0.9, 0.9)).unwrap();
        assert!(est.probability > 0.6, "probability was {}", est.probability);
    }

    #[test]
    fn irrelevant_headline_is_silently_ignored() {
        let src = NewsSentimentSource::new("wire-nlp");
        assert!(src.on_event(&event(0.9, 0.02)).is_none());
    }

    #[test]
    fn low_relevance_headline_has_wide_uncertainty() {
        let src = NewsSentimentSource::new("wire-nlp");
        let low = src.on_event(&event(0.5, 0.2)).unwrap();
        let high = src.on_event(&event(0.5, 0.95)).unwrap();
        assert!(low.std_dev > high.std_dev);
    }
}
