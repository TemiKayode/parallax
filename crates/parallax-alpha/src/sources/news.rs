use crate::source::AlphaSource;
use parallax_types::{
    AlphaEventKind, CanonicalContractId, EstimateKind, ProbabilityEstimate, RawEvent,
    StalenessPolicy,
};
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
    /// Max log-odds displacement at polarity=±1, relevance=1.
    sensitivity: f64,
    /// Headlines below this relevance are treated as not-about-this-contract.
    min_relevance: f64,
    correlation_group: Option<String>,
}

impl NewsSentimentSource {
    pub fn new(name: impl Into<String>) -> Self {
        NewsSentimentSource {
            name: name.into(),
            kinds: [AlphaEventKind::NewsHeadline],
            sensitivity: 2.0,
            min_relevance: 0.15,
            correlation_group: None,
        }
    }

    pub fn with_correlation_group(mut self, group: impl Into<String>) -> Self {
        self.correlation_group = Some(group.into());
        self
    }

    /// Builds from the offline-fitted config artifact (design doc review
    /// 2.13).
    pub fn from_config(name: impl Into<String>, config: &crate::config::NewsConfig) -> Self {
        NewsSentimentSource {
            sensitivity: config.sensitivity,
            ..Self::new(name)
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

        // A headline is a *directional* signal, not an absolute opinion —
        // returning `0.5 + polarity*relevance*sensitivity` as an absolute
        // estimate anchored the pooled probability to 0.5 on every single
        // headline (of any polarity) and could pull a 0.95 contract
        // *down* toward 0.8 on bullish news, because the aggregator
        // treated it as competing evidence about the *level* rather than
        // a nudge on top of it (design doc review 2.6). A shift in
        // log-odds space is scale-free: the same headline moves a 0.50
        // contract a lot and a 0.97 contract very little, applied by the
        // aggregator on top of the pooled absolute estimate, never voted
        // into that pool.
        let shift = payload.polarity.clamp(-1.0, 1.0)
            * payload.relevance.clamp(0.0, 1.0)
            * self.sensitivity;
        // Low relevance means low confidence: std_dev grows as relevance
        // shrinks, floored so a maximally-relevant headline still isn't
        // treated as certain on its own.
        let std_dev = (0.5 / payload.relevance.max(self.min_relevance)).min(3.0);

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability: shift,
            std_dev,
            as_of: event.receive_ts,
            kind: EstimateKind::LogOddsShift,
            staleness: StalenessPolicy::Decays,
            correlation_group: self.correlation_group.clone(),
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
                "contract": "econ.cpi_yoy.us.gt_30.2026-08-13.bls",
                "polarity": polarity,
                "relevance": relevance,
            }),
        }
    }

    #[test]
    fn strongly_relevant_bullish_headline_is_a_positive_log_odds_shift() {
        let src = NewsSentimentSource::new("wire-nlp");
        let est = src.on_event(&event(0.9, 0.9)).unwrap();
        assert_eq!(est.kind, EstimateKind::LogOddsShift);
        assert!(est.probability > 0.0, "shift was {}", est.probability);
    }

    #[test]
    fn bearish_headline_is_a_negative_shift() {
        let src = NewsSentimentSource::new("wire-nlp");
        let est = src.on_event(&event(-0.9, 0.9)).unwrap();
        assert!(est.probability < 0.0, "shift was {}", est.probability);
    }

    #[test]
    fn zero_polarity_is_a_true_no_op_shift() {
        let src = NewsSentimentSource::new("wire-nlp");
        let est = src.on_event(&event(0.0, 0.9)).unwrap();
        assert!(est.probability.abs() < 1e-9);
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

    #[test]
    fn from_config_uses_the_offline_fitted_sensitivity() {
        let config = crate::config::NewsConfig { sensitivity: 0.1 };
        let src = NewsSentimentSource::from_config("wire-nlp", &config);
        let default_src = NewsSentimentSource::new("wire-nlp");
        let low_sensitivity = src.on_event(&event(0.9, 0.9)).unwrap();
        let default_sensitivity = default_src.on_event(&event(0.9, 0.9)).unwrap();
        assert!(low_sensitivity.probability.abs() < default_sensitivity.probability.abs());
    }
}
