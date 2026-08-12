use crate::source::AlphaSource;
use parallax_types::{AlphaEventKind, CanonicalContractId, ProbabilityEstimate, RawEvent};
use serde::Deserialize;

/// Expected `RawEvent::payload` shape for a weather ensemble update.
///
/// In production, ingestion resolves which canonical contracts a given
/// weather station/model update is relevant to via a subscription table
/// (a station feeds N threshold contracts); this crate takes the
/// canonical id as already resolved and embedded in the payload, since
/// that resolution step is an ingestion-layer concern, not an alpha-model
/// concern.
#[derive(Debug, Deserialize)]
struct WeatherPayload {
    contract: String,
    threshold_tenths: i64,
    /// Per-ensemble-member forecast, same units as `threshold_tenths`.
    ensemble_forecast_tenths: Vec<i64>,
}

/// Implements the "ensemble disagreement is the uncertainty term"
/// principle generalized from the PolyWeather / Kalshi-Weather-Model
/// lineage (design doc §3, §7): the probability estimate is the fraction
/// of ensemble members that exceed the threshold, and the uncertainty
/// is the standard error of that fraction — which is large precisely
/// when the ensemble disagrees, and shrinks as members converge.
pub struct WeatherEnsembleSource {
    name: String,
    kinds: [AlphaEventKind; 1],
}

impl WeatherEnsembleSource {
    pub fn new(name: impl Into<String>) -> Self {
        WeatherEnsembleSource {
            name: name.into(),
            kinds: [AlphaEventKind::Weather],
        }
    }
}

impl AlphaSource for WeatherEnsembleSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn event_kinds(&self) -> &[AlphaEventKind] {
        &self.kinds
    }

    fn on_event(&self, event: &RawEvent) -> Option<ProbabilityEstimate> {
        if event.kind != AlphaEventKind::Weather {
            return None;
        }
        let payload: WeatherPayload = serde_json::from_value(event.payload.clone()).ok()?;
        if payload.ensemble_forecast_tenths.is_empty() {
            return None;
        }

        let n = payload.ensemble_forecast_tenths.len() as f64;
        let exceed = payload
            .ensemble_forecast_tenths
            .iter()
            .filter(|&&f| f > payload.threshold_tenths)
            .count() as f64;
        let p = exceed / n;

        // Agresti-Coull-style shrinkage: behaves sanely even at n=1
        // (where a naive standard-error-of-a-proportion would report
        // false certainty) and shrinks toward the classical standard
        // error as the ensemble grows.
        let std_dev = ((p * (1.0 - p) + 0.25) / (n + 1.0)).sqrt();

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability: p,
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

    fn event(threshold: i64, forecasts: Vec<i64>) -> RawEvent {
        RawEvent {
            source: "hrrr-ensemble".into(),
            kind: AlphaEventKind::Weather,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: json!({
                "contract": "wx.temp.chicago.gt.869.2026-08-12.nws_official",
                "threshold_tenths": threshold,
                "ensemble_forecast_tenths": forecasts,
            }),
        }
    }

    #[test]
    fn unanimous_ensemble_is_confident() {
        let src = WeatherEnsembleSource::new("hrrr");
        let est = src
            .on_event(&event(869, vec![900, 910, 895, 905, 890]))
            .unwrap();
        assert_eq!(est.probability, 1.0);
        assert!(est.std_dev < 0.25, "std_dev was {}", est.std_dev);
    }

    #[test]
    fn split_ensemble_is_uncertain() {
        let src = WeatherEnsembleSource::new("hrrr");
        let est = src
            .on_event(&event(869, vec![900, 850, 895, 840, 860]))
            .unwrap();
        assert!(est.probability > 0.0 && est.probability < 1.0);
        assert!(est.std_dev > 0.15, "std_dev was {}", est.std_dev);
    }

    #[test]
    fn irrelevant_event_kind_yields_no_opinion() {
        let src = WeatherEnsembleSource::new("hrrr");
        let mut ev = event(869, vec![900]);
        ev.kind = AlphaEventKind::NewsHeadline;
        assert!(src.on_event(&ev).is_none());
    }
}
