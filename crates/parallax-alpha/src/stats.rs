//! Small numerical helpers shared by the alpha sources. No external crate
//! pulled in for a single function — this is the entire dependency.

/// Abramowitz & Stegun 7.1.26 approximation of the error function.
/// Max absolute error ≈ 1.5e-7, comfortably below the precision anything
/// downstream (a probability rounded to a venue's tick size) can use.
fn erf(x: f64) -> f64 {
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    sign * y
}

pub fn normal_cdf(z: f64) -> f64 {
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

/// Standard normal density — used by the APERTURE barrier-probability
/// model (design doc §4) to propagate uncertainty in the underlying
/// z-score into an uncertainty on the resulting probability via the
/// delta method: d/dz Φ(z) = φ(z). Φ is steepest at z = 0, so the same
/// absolute uncertainty in z produces the *most* probability uncertainty
/// exactly at the coin-flip point and the *least* deep in either tail —
/// the opposite of naive intuition, and the correct direction.
pub fn normal_pdf(z: f64) -> f64 {
    (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// P(X > threshold) for X ~ Normal(mean, std).
pub fn prob_exceeds(mean: f64, std: f64, threshold: f64) -> f64 {
    normal_cdf((mean - threshold) / std.max(1e-9))
}

/// No probability anywhere in this crate is ever allowed to be exactly
/// `0.0` or `1.0`. A unanimous small sample or a "certain" resolution
/// scores infinite log loss the first time it turns out wrong, and tells
/// a stat-arb engine that any price on the other side of it is free money
/// (design doc review 2.10).
pub const PROB_EPSILON: f64 = 1e-4;

pub fn clamp_probability(p: f64) -> f64 {
    p.clamp(PROB_EPSILON, 1.0 - PROB_EPSILON)
}

/// Beta-Binomial posterior mean and standard deviation for `successes`
/// out of `n` trials under a `Beta(prior_alpha, prior_beta)` prior —
/// Jeffreys' `(0.5, 0.5)` is the conventional uninformative default.
/// Unlike a raw proportion (`successes / n`), this never actually reaches
/// 0 or 1 for a finite sample, which is the entire point: a unanimous
/// five-member ensemble reporting exactly `1.0` is claiming infinite
/// confidence from five data points (design doc review 2.10).
pub fn beta_binomial_posterior(
    successes: f64,
    n: f64,
    prior_alpha: f64,
    prior_beta: f64,
) -> (f64, f64) {
    let successes = successes.clamp(0.0, n.max(0.0));
    let a = prior_alpha + successes;
    let b = prior_beta + (n - successes).max(0.0);
    let mean = a / (a + b);
    let variance = (a * b) / ((a + b).powi(2) * (a + b + 1.0));
    (clamp_probability(mean), variance.max(0.0).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_cdf_matches_known_values() {
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-6);
        assert!((normal_cdf(1.0) - 0.8413).abs() < 1e-3);
        assert!((normal_cdf(-1.0) - 0.1587).abs() < 1e-3);
    }

    #[test]
    fn prob_exceeds_is_higher_when_mean_is_further_above_threshold() {
        let near = prob_exceeds(10.0, 2.0, 9.0);
        let far = prob_exceeds(15.0, 2.0, 9.0);
        assert!(far > near);
    }

    #[test]
    fn normal_pdf_peaks_at_zero_and_integrates_to_roughly_one() {
        assert!(normal_pdf(0.0) > normal_pdf(1.0));
        assert!(normal_pdf(0.0) > normal_pdf(-1.0));
        assert!((normal_pdf(0.0) - 0.3989422804).abs() < 1e-6);
        // Coarse Riemann-sum sanity check that this is a real density, not just peaked.
        let mut total = 0.0;
        let step = 0.01;
        let mut z = -8.0;
        while z <= 8.0 {
            total += normal_pdf(z) * step;
            z += step;
        }
        assert!((total - 1.0).abs() < 1e-3);
    }

    #[test]
    fn normal_pdf_is_symmetric() {
        assert!((normal_pdf(1.3) - normal_pdf(-1.3)).abs() < 1e-12);
    }

    #[test]
    fn beta_binomial_posterior_never_reaches_exactly_zero_or_one() {
        let (mean, _) = beta_binomial_posterior(5.0, 5.0, 0.5, 0.5);
        assert!(
            mean < 1.0,
            "unanimous sample must not report exact certainty"
        );
        let (mean, _) = beta_binomial_posterior(0.0, 5.0, 0.5, 0.5);
        assert!(mean > 0.0);
    }

    #[test]
    fn beta_binomial_posterior_std_dev_shrinks_as_sample_size_grows() {
        let (_, small_std) = beta_binomial_posterior(3.0, 5.0, 0.5, 0.5);
        let (_, large_std) = beta_binomial_posterior(60.0, 100.0, 0.5, 0.5);
        assert!(large_std < small_std);
    }

    #[test]
    fn clamp_probability_never_returns_the_extremes() {
        assert!(clamp_probability(0.0) > 0.0);
        assert!(clamp_probability(1.0) < 1.0);
        assert!((clamp_probability(0.5) - 0.5).abs() < 1e-12);
    }
}
