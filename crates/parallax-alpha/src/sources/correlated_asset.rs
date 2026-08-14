use crate::source::AlphaSource;
use parallax_types::{AlphaEventKind, CanonicalContractId, ProbabilityEstimate, RawEvent};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct CorrelatedAssetPayload {
    contract: String,
    /// Paired historical log-returns — `correlated_returns[i]` and
    /// `driver_returns[i]` must be the same timestamp, same length.
    correlated_returns: Vec<f64>,
    driver_returns: Vec<f64>,
    current_correlated_return: f64,
    current_driver_return: f64,
}

/// No-intercept OLS slope of `y` on `x` — returns's expected mean is
/// ~0 over short horizons, so forcing the fit through the origin is the
/// right simplification rather than fitting a spurious intercept on
/// noise. `None` if `x` has no variance to regress against.
fn estimate_beta(x: &[f64], y: &[f64]) -> Option<f64> {
    let sum_xy: f64 = x.iter().zip(y).map(|(xi, yi)| xi * yi).sum();
    let sum_xx: f64 = x.iter().map(|xi| xi * xi).sum();
    if sum_xx < 1e-12 {
        return None;
    }
    Some(sum_xy / sum_xx)
}

/// The signal-redundancy control from design doc §4b: a correlated
/// asset's raw move is not allowed to vote on the target market directly
/// — only the *residual*, the part of its move that the driver asset
/// (e.g. BTC) does not already explain, becomes a probability estimate.
/// A move that exactly matches what the driver predicts contributes
/// nothing, correctly, because `ReferenceDistanceSource` already saw
/// that same systemic move directly and more reliably.
pub struct CorrelatedAssetSource {
    name: String,
    kinds: [AlphaEventKind; 1],
    /// Max probability displacement from 0.5 at an extreme residual —
    /// deliberately smaller than the primary reference-distance source's
    /// influence, since this is a secondary, confirming signal.
    pub sensitivity: f64,
    pub min_std_dev: f64,
    /// Fixed rather than dynamically rescaled by how much variance the
    /// driver explains — the residual's own z-score (below) already
    /// captures "how informative is this move," so scaling std_dev by
    /// R² on top of that would double-count the same effect.
    pub base_std_dev: f64,
    correlation_group: Option<String>,
}

impl CorrelatedAssetSource {
    pub fn new(name: impl Into<String>) -> Self {
        CorrelatedAssetSource {
            name: name.into(),
            kinds: [AlphaEventKind::ReferenceAsset],
            sensitivity: 0.15,
            min_std_dev: 0.02,
            base_std_dev: 0.15,
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

impl AlphaSource for CorrelatedAssetSource {
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
        let payload: CorrelatedAssetPayload = serde_json::from_value(event.payload.clone()).ok()?;
        if payload.correlated_returns.len() != payload.driver_returns.len()
            || payload.correlated_returns.len() < 2
        {
            return None;
        }

        let beta = estimate_beta(&payload.driver_returns, &payload.correlated_returns)?;

        let residuals: Vec<f64> = payload
            .correlated_returns
            .iter()
            .zip(&payload.driver_returns)
            .map(|(c, d)| c - beta * d)
            .collect();
        let n = residuals.len() as f64;
        let mean_residual = residuals.iter().sum::<f64>() / n;
        let residual_std = (residuals
            .iter()
            .map(|r| (r - mean_residual).powi(2))
            .sum::<f64>()
            / (n - 1.0))
            .sqrt()
            .max(self.min_std_dev);

        let current_residual =
            payload.current_correlated_return - beta * payload.current_driver_return;
        let z = current_residual / residual_std;
        let shift = self.sensitivity * z.tanh();
        let probability = (0.5 + shift).clamp(0.0, 1.0);

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability,
            std_dev: self.base_std_dev,
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

    fn event(
        correlated_returns: Vec<f64>,
        driver_returns: Vec<f64>,
        current_correlated_return: f64,
        current_driver_return: f64,
    ) -> RawEvent {
        RawEvent {
            source: "eth-vs-btc".into(),
            kind: AlphaEventKind::ReferenceAsset,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: json!({
                "contract": "crypto.updown.btc.gt.0.2026-08-12t12-05-00z.binance",
                "correlated_returns": correlated_returns,
                "driver_returns": driver_returns,
                "current_correlated_return": current_correlated_return,
                "current_driver_return": current_driver_return,
            }),
        }
    }

    #[test]
    fn estimate_beta_recovers_a_known_linear_relationship() {
        let x = vec![0.01, -0.02, 0.03, -0.01, 0.02];
        let y: Vec<f64> = x.iter().map(|xi| 2.0 * xi).collect();
        let beta = estimate_beta(&x, &y).unwrap();
        assert!((beta - 2.0).abs() < 1e-9);
    }

    #[test]
    fn estimate_beta_is_none_when_driver_has_no_variance() {
        assert!(estimate_beta(&[0.0, 0.0, 0.0], &[0.01, 0.02, -0.01]).is_none());
    }

    #[test]
    fn a_move_fully_explained_by_the_driver_contributes_nothing() {
        // History: correlated = 1.5 * driver exactly. Current move also
        // matches that ratio exactly -> zero residual -> no independent signal.
        let driver = vec![0.01, -0.02, 0.03, -0.01, 0.02];
        let correlated: Vec<f64> = driver.iter().map(|d| 1.5 * d).collect();
        let src = CorrelatedAssetSource::new("eth-residual");
        let est = src
            .on_event(&event(correlated, driver, 1.5 * 0.015, 0.015))
            .unwrap();
        assert!(
            (est.probability - 0.5).abs() < 1e-6,
            "probability was {}",
            est.probability
        );
    }

    #[test]
    fn a_move_beyond_what_the_driver_explains_shifts_probability_up() {
        let driver = vec![0.01, -0.02, 0.03, -0.01, 0.02];
        let correlated: Vec<f64> = driver.iter().map(|d| 1.0 * d).collect();
        let src = CorrelatedAssetSource::new("eth-residual");
        // current driver move is 0.01 (beta*driver = 0.01), but correlated
        // asset moved 0.05 -> large positive residual beyond history's noise.
        let est = src
            .on_event(&event(correlated, driver, 0.05, 0.01))
            .unwrap();
        assert!(est.probability > 0.5, "probability was {}", est.probability);
    }

    #[test]
    fn mismatched_length_arrays_yield_no_opinion() {
        let src = CorrelatedAssetSource::new("eth-residual");
        assert!(src
            .on_event(&event(vec![0.01, 0.02], vec![0.01], 0.0, 0.0))
            .is_none());
    }

    #[test]
    fn irrelevant_event_kind_yields_no_opinion() {
        let src = CorrelatedAssetSource::new("eth-residual");
        let mut ev = event(vec![0.01, 0.02], vec![0.01, 0.02], 0.0, 0.0);
        ev.kind = AlphaEventKind::Weather;
        assert!(src.on_event(&ev).is_none());
    }
}
