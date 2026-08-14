use crate::source::AlphaSource;
use crate::stats::{normal_cdf, normal_pdf};
use parallax_types::{AlphaEventKind, CanonicalContractId, ProbabilityEstimate, RawEvent};
use serde::Deserialize;

/// Expected `RawEvent::payload` shape for a reference-asset price update.
/// `recent_prices` is a short trailing window at `tick_interval_secs`
/// spacing — as with the weather source, resolving *which* canonical
/// contracts a given asset's feed is relevant to, and maintaining that
/// rolling window, is an ingestion-layer concern; this source takes the
/// snapshot as already assembled.
#[derive(Debug, Deserialize)]
struct ReferenceDistancePayload {
    contract: String,
    reference_price: f64,
    current_price: f64,
    seconds_remaining: f64,
    recent_prices: Vec<f64>,
    tick_interval_secs: f64,
}

/// The APERTURE fair-value model's core (design doc §4): prices a
/// "does the asset finish above its opening reference price" barrier
/// outcome with the standard digital-option result — the probability
/// that driftless-to-slightly-drifting Brownian motion finishes above a
/// level is `Φ((x + μτ) / (σ√τ))`. This one formula is deliberately how
/// "distance from reference," "speed of the recent move," "volatility,"
/// and "time decay" combine: the same price distance implies a near
/// coin-flip with minutes left and near-certainty with seconds left,
/// because `σ√τ` shrinks as τ→0 — no separate time-decay heuristic
/// needed, the formula already has the right shape as τ approaches zero
/// (it degenerates into a step function at the reference price, which is
/// exactly correct: with no time left, "above or below now" *is* the
/// answer).
pub struct ReferenceDistanceSource {
    name: String,
    kinds: [AlphaEventKind; 1],
    /// How much of the recent trend to extrapolate as drift, in `[0, 1]`.
    /// 0 = driftless (the conservative default for very short horizons);
    /// higher values chase recent momentum harder, at the risk of
    /// extrapolating noise.
    pub momentum_coefficient: f64,
    /// Base uncertainty in the z-score itself (model risk in the
    /// volatility/drift estimates), before the `1/√n` shrinkage from
    /// sample size and the `φ(z)` scaling from the delta method.
    pub base_z_uncertainty: f64,
    pub min_std_dev: f64,
    correlation_group: Option<String>,
}

impl ReferenceDistanceSource {
    pub fn new(name: impl Into<String>) -> Self {
        ReferenceDistanceSource {
            name: name.into(),
            kinds: [AlphaEventKind::ReferenceAsset],
            momentum_coefficient: 0.3,
            base_z_uncertainty: 0.5,
            min_std_dev: 0.01,
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

impl AlphaSource for ReferenceDistanceSource {
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
        let payload: ReferenceDistancePayload =
            serde_json::from_value(event.payload.clone()).ok()?;
        if payload.reference_price <= 0.0
            || payload.current_price <= 0.0
            || payload.tick_interval_secs <= 0.0
            || payload.recent_prices.len() < 2
        {
            return None;
        }

        let log_returns: Vec<f64> = payload
            .recent_prices
            .windows(2)
            .map(|w| (w[1] / w[0]).ln())
            .collect();
        let n = log_returns.len() as f64;
        let mean_return = log_returns.iter().sum::<f64>() / n;

        let std_return = if log_returns.len() >= 2 {
            let variance = log_returns
                .iter()
                .map(|r| (r - mean_return).powi(2))
                .sum::<f64>()
                / (n - 1.0);
            variance.sqrt()
        } else {
            self.min_std_dev
        };

        let sigma_per_second = (std_return / payload.tick_interval_secs.sqrt()).max(1e-9);
        let mu_per_second = self.momentum_coefficient * mean_return / payload.tick_interval_secs;

        let x = (payload.current_price / payload.reference_price).ln();
        let tau = payload.seconds_remaining.max(1e-6);
        let denom = (sigma_per_second * tau.sqrt()).max(1e-9);
        let z = (x + mu_per_second * tau) / denom;

        let probability = normal_cdf(z);
        let z_uncertainty = self.base_z_uncertainty / n.sqrt();
        let std_dev = (normal_pdf(z) * z_uncertainty).clamp(self.min_std_dev, 0.5);

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability,
            std_dev,
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
        reference_price: f64,
        current_price: f64,
        seconds_remaining: f64,
        recent_prices: Vec<f64>,
        tick_interval_secs: f64,
    ) -> RawEvent {
        RawEvent {
            source: "binance-btc".into(),
            kind: AlphaEventKind::ReferenceAsset,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload: json!({
                "contract": "crypto.updown.btc.gt.0.2026-08-12t12-05-00z.binance",
                "reference_price": reference_price,
                "current_price": current_price,
                "seconds_remaining": seconds_remaining,
                "recent_prices": recent_prices,
                "tick_interval_secs": tick_interval_secs,
            }),
        }
    }

    #[test]
    fn at_the_reference_price_with_no_momentum_probability_is_a_coin_flip() {
        let src = ReferenceDistanceSource::new("btc-barrier");
        let flat = vec![65000.0, 65000.0, 65000.0, 65000.0];
        let est = src
            .on_event(&event(65000.0, 65000.0, 120.0, flat, 5.0))
            .unwrap();
        assert!((est.probability - 0.5).abs() < 1e-9);
    }

    #[test]
    fn same_distance_closer_to_expiry_is_more_confident_up() {
        let src = ReferenceDistanceSource::new("btc-barrier");
        // Oscillating around 65100 with ~zero net drift (mu ~ 0), so only
        // tau changes between the two calls, isolating the pure
        // distance/time-decay effect the design doc's diagram shows.
        // (A genuinely trending window is a different, also-correct case:
        // with real drift, z ~ (mu/sigma)*sqrt(tau) can *grow* with more
        // time left, since drift compounds faster than diffusion's sqrt(tau)
        // decay — see the momentum-dominated test below.)
        let oscillating = vec![65100.0, 65105.0, 65095.0, 65100.0, 65100.0];
        let far = src
            .on_event(&event(65000.0, 65100.0, 300.0, oscillating.clone(), 5.0))
            .unwrap();
        let near = src
            .on_event(&event(65000.0, 65100.0, 15.0, oscillating, 5.0))
            .unwrap();
        assert!(
            near.probability > far.probability,
            "near={} far={}",
            near.probability,
            far.probability
        );
        assert!(
            near.probability > 0.9,
            "expected near-certainty with little time left, got {}",
            near.probability
        );
    }

    #[test]
    fn strong_enough_momentum_can_make_more_time_left_more_confident_not_less() {
        // The flip side of the test above: with real drift baked into the
        // recent window, extrapolated over more remaining time, z can grow
        // with tau instead of shrink — drift compounds linearly in tau,
        // diffusion only as sqrt(tau). This is correct Brownian-motion-
        // with-drift behavior, not a bug, and is worth locking in as a
        // test so a future "simplify away mu" refactor can't reintroduce
        // the opposite assumption silently.
        let src = ReferenceDistanceSource::new("btc-barrier");
        let trending = vec![65000.0, 65010.0, 65020.0, 65030.0, 65100.0];
        let shorter = src
            .on_event(&event(65000.0, 65100.0, 15.0, trending.clone(), 5.0))
            .unwrap();
        let longer = src
            .on_event(&event(65000.0, 65100.0, 300.0, trending, 5.0))
            .unwrap();
        assert!(
            longer.probability > shorter.probability,
            "longer={} shorter={}",
            longer.probability,
            shorter.probability
        );
    }

    #[test]
    fn more_price_samples_produce_a_tighter_estimate() {
        let src = ReferenceDistanceSource::new("btc-barrier");
        let short_history = vec![65000.0, 65020.0, 65010.0];
        let mut long_history = Vec::new();
        for _ in 0..15 {
            long_history.extend_from_slice(&[65000.0, 65020.0, 65010.0]);
        }
        let short_est = src
            .on_event(&event(65000.0, 65015.0, 120.0, short_history, 5.0))
            .unwrap();
        let long_est = src
            .on_event(&event(65000.0, 65015.0, 120.0, long_history, 5.0))
            .unwrap();
        assert!(
            long_est.std_dev < short_est.std_dev,
            "long={} short={}",
            long_est.std_dev,
            short_est.std_dev
        );
    }

    #[test]
    fn below_reference_price_implies_probability_under_half() {
        let src = ReferenceDistanceSource::new("btc-barrier");
        let flat = vec![65000.0, 64990.0, 64980.0, 64950.0];
        let est = src
            .on_event(&event(65000.0, 64950.0, 120.0, flat, 5.0))
            .unwrap();
        assert!(est.probability < 0.5);
    }

    #[test]
    fn irrelevant_event_kind_yields_no_opinion() {
        let src = ReferenceDistanceSource::new("btc-barrier");
        let mut ev = event(65000.0, 65100.0, 120.0, vec![1.0, 2.0], 5.0);
        ev.kind = AlphaEventKind::Weather;
        assert!(src.on_event(&ev).is_none());
    }
}
