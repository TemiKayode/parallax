use crate::source::AlphaSource;
use crate::stats::beta_binomial_posterior;
use parallax_types::{
    AlphaEventKind, CanonicalContractId, Direction, EstimateKind, ProbabilityEstimate, RawEvent,
    StalenessPolicy,
};
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
    /// The contract's direction as one of `"gt"`, `"lt"`, `"between"`. If
    /// absent, falls back to parsing it out of `contract`'s own canonical
    /// id (design doc review 1.4) — the id already encodes it, so a
    /// well-formed contract id never actually needs this repeated.
    direction: Option<String>,
    /// Required only when `direction` is `"between"` and `contract`'s own
    /// id doesn't already parse to one.
    upper_tenths: Option<i64>,
    /// Per-ensemble-member forecast, same units as `threshold_tenths`.
    ensemble_forecast_tenths: Vec<i64>,
}

fn resolve_direction(payload: &WeatherPayload) -> Option<Direction> {
    match payload.direction.as_deref() {
        Some("gt") => Some(Direction::GreaterThan),
        Some("lt") => Some(Direction::LessThan),
        Some("between") => payload
            .upper_tenths
            .map(|upper| Direction::Between { upper }),
        // An explicit-but-unrecognized direction string is a payload bug,
        // not silently ignorable — the fallback below only fires when
        // nothing was said at all.
        Some(_) => None,
        None => CanonicalContractId(payload.contract.clone()).direction(),
    }
}

/// Implements the "ensemble disagreement is the uncertainty term"
/// principle generalized from the PolyWeather / Kalshi-Weather-Model
/// lineage (design doc §3, §7): the probability estimate is a
/// Beta-Binomial posterior mean over the fraction of ensemble members
/// that satisfy the contract's *actual* direction (not just "exceeds," as
/// a `LessThan` or `Between` contract are exactly-backwards or entirely
/// unpriced under a unconditional "count(member > threshold)" — design
/// doc review 1.4), and the uncertainty is the posterior standard
/// deviation, which is large precisely when the ensemble disagrees or the
/// sample is small, and never collapses to exact certainty even for a
/// unanimous ensemble (design doc review 2.10).
pub struct WeatherEnsembleSource {
    name: String,
    kinds: [AlphaEventKind; 1],
    correlation_group: Option<String>,
    /// Floor applied on top of the Beta-Binomial posterior's own
    /// std_dev — defense in depth, and the knob the offline research
    /// pipeline's `AlphaConfig` actually controls (design doc review
    /// 2.13).
    min_std_dev: f64,
}

impl WeatherEnsembleSource {
    pub fn new(name: impl Into<String>) -> Self {
        WeatherEnsembleSource {
            name: name.into(),
            kinds: [AlphaEventKind::Weather],
            correlation_group: None,
            min_std_dev: 0.0,
        }
    }

    /// Builds from the offline-fitted config artifact (design doc review
    /// 2.13) rather than the built-in defaults.
    pub fn from_config(name: impl Into<String>, config: &crate::config::WeatherConfig) -> Self {
        WeatherEnsembleSource {
            min_std_dev: config.min_std_dev,
            ..Self::new(name)
        }
    }

    /// Marks every estimate this source emits as correlated with every
    /// other estimate sharing the same group — e.g. several ensemble
    /// products that share initial conditions. The aggregator haircuts a
    /// group's combined weight instead of pooling them as independent
    /// observations (design doc review 2.9).
    pub fn with_correlation_group(mut self, group: impl Into<String>) -> Self {
        self.correlation_group = Some(group.into());
        self
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
        // Silence beats a backwards trade: with no direction available
        // from either the payload or the contract id, emit nothing rather
        // than guess (design doc review 1.4).
        let direction = resolve_direction(&payload)?;

        let n = payload.ensemble_forecast_tenths.len() as f64;
        let successes = payload
            .ensemble_forecast_tenths
            .iter()
            .filter(|&&f| match direction {
                Direction::GreaterThan => f > payload.threshold_tenths,
                Direction::LessThan => f < payload.threshold_tenths,
                Direction::Between { upper } => f >= payload.threshold_tenths && f <= upper,
            })
            .count() as f64;

        let (probability, std_dev) = beta_binomial_posterior(successes, n, 0.5, 0.5);
        let std_dev = std_dev.max(self.min_std_dev);

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability,
            std_dev,
            as_of: event.receive_ts,
            kind: EstimateKind::Absolute,
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

    fn event(direction: &str, threshold: i64, forecasts: Vec<i64>) -> RawEvent {
        RawEvent {
            source: "hrrr-ensemble".into(),
            kind: AlphaEventKind::Weather,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: json!({
                "contract": "wx.temp.chicago.gt_869.2026-08-12.nws_official",
                "threshold_tenths": threshold,
                "direction": direction,
                "ensemble_forecast_tenths": forecasts,
            }),
        }
    }

    #[test]
    fn unanimous_ensemble_is_confident_but_not_exactly_certain() {
        let src = WeatherEnsembleSource::new("hrrr");
        let est = src
            .on_event(&event("gt", 869, vec![900, 910, 895, 905, 890]))
            .unwrap();
        assert!(est.probability > 0.9, "probability was {}", est.probability);
        assert!(
            est.probability < 1.0,
            "a 5-member unanimous ensemble must not report exact certainty, got {}",
            est.probability
        );
    }

    #[test]
    fn split_ensemble_is_uncertain() {
        let src = WeatherEnsembleSource::new("hrrr");
        let est = src
            .on_event(&event("gt", 869, vec![900, 850, 895, 840, 860]))
            .unwrap();
        assert!(est.probability > 0.0 && est.probability < 1.0);
        assert!(est.std_dev > 0.15, "std_dev was {}", est.std_dev);
    }

    #[test]
    fn irrelevant_event_kind_yields_no_opinion() {
        let src = WeatherEnsembleSource::new("hrrr");
        let mut ev = event("gt", 869, vec![900]);
        ev.kind = AlphaEventKind::NewsHeadline;
        assert!(src.on_event(&ev).is_none());
    }

    #[test]
    fn less_than_direction_is_priced_correctly_not_backwards() {
        // All 5 members are BELOW the threshold: a "less than" contract
        // should be priced confidently YES, not confidently NO (design
        // doc review 1.4's exact failure mode).
        let src = WeatherEnsembleSource::new("hrrr");
        let est = src
            .on_event(&event("lt", 869, vec![800, 810, 795, 805, 790]))
            .unwrap();
        assert!(est.probability > 0.8, "probability was {}", est.probability);
    }

    #[test]
    fn between_direction_uses_both_bounds() {
        let mut ev = event("between", 800, vec![850, 850, 850, 750, 950]);
        ev.payload["upper_tenths"] = json!(900);
        let src = WeatherEnsembleSource::new("hrrr");
        let est = src.on_event(&ev).unwrap();
        // 3 of 5 members (850,850,850) land inside [800,900].
        assert!(
            (est.probability - 0.6).abs() < 0.15,
            "probability was {}",
            est.probability
        );
    }

    #[test]
    fn missing_direction_falls_back_to_the_contract_ids_own_direction() {
        let mut ev = event("gt", 869, vec![900, 910, 895]);
        ev.payload.as_object_mut().unwrap().remove("direction");
        // The contract id itself encodes "gt_869" -> GreaterThan.
        let src = WeatherEnsembleSource::new("hrrr");
        let est = src.on_event(&ev).unwrap();
        assert!(est.probability > 0.5);
    }

    #[test]
    fn no_direction_available_anywhere_yields_silence_not_a_guess() {
        let mut ev = event("gt", 869, vec![900]);
        ev.payload["contract"] = json!("not-a-well-formed-id");
        ev.payload.as_object_mut().unwrap().remove("direction");
        let src = WeatherEnsembleSource::new("hrrr");
        assert!(src.on_event(&ev).is_none());
    }

    #[test]
    fn correlation_group_is_attached_when_configured() {
        let src = WeatherEnsembleSource::new("hrrr").with_correlation_group("nwp-ensemble");
        let est = src.on_event(&event("gt", 869, vec![900])).unwrap();
        assert_eq!(est.correlation_group.as_deref(), Some("nwp-ensemble"));
    }

    #[test]
    fn from_config_applies_the_offline_fitted_std_dev_floor() {
        let config = crate::config::WeatherConfig { min_std_dev: 0.5 };
        let src = WeatherEnsembleSource::from_config("hrrr", &config);
        // A huge, unanimous ensemble would otherwise report a tiny std_dev.
        let forecasts: Vec<i64> = (0..50).map(|_| 900).collect();
        let est = src.on_event(&event("gt", 869, forecasts)).unwrap();
        assert!(est.std_dev >= 0.5, "std_dev was {}", est.std_dev);
    }
}
