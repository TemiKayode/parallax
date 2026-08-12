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

/// P(X > threshold) for X ~ Normal(mean, std).
pub fn prob_exceeds(mean: f64, std: f64, threshold: f64) -> f64 {
    normal_cdf((mean - threshold) / std.max(1e-9))
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
}
