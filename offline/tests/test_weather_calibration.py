import numpy as np
import pytest

from parallax_research.weather.calibration import brier_score, ensemble_probability, reliability_curve


def test_brier_score_is_zero_for_perfect_forecasts():
    assert brier_score(np.array([1.0, 0.0, 1.0]), np.array([1.0, 0.0, 1.0])) == 0.0


def test_brier_score_is_quarter_for_always_50_50_against_balanced_outcomes():
    probs = np.array([0.5, 0.5, 0.5, 0.5])
    outcomes = np.array([1.0, 0.0, 1.0, 0.0])
    assert brier_score(probs, outcomes) == pytest.approx(0.25)


def test_brier_score_rejects_mismatched_shapes():
    with pytest.raises(ValueError):
        brier_score(np.array([0.5, 0.5]), np.array([1.0]))


def test_ensemble_probability_matches_rust_formula_for_unanimous_ensemble():
    # Same fixture as the Rust test
    # `sources::weather::tests::unanimous_ensemble_is_confident`.
    forecasts = np.array([900, 910, 895, 905, 890])
    p, std = ensemble_probability(forecasts, threshold=869)
    assert p == 1.0
    assert std < 0.25


def test_ensemble_probability_matches_rust_formula_for_split_ensemble():
    # Same fixture as the Rust test
    # `sources::weather::tests::split_ensemble_is_uncertain`.
    forecasts = np.array([900, 850, 895, 840, 860])
    p, std = ensemble_probability(forecasts, threshold=869)
    assert 0.0 < p < 1.0
    assert std > 0.15


def test_ensemble_probability_rejects_empty_input():
    with pytest.raises(ValueError):
        ensemble_probability(np.array([]), threshold=869)


def test_reliability_curve_reports_frequency_close_to_prediction_when_well_calibrated():
    rng = np.random.default_rng(7)
    n = 20_000
    probs = rng.uniform(0.0, 1.0, size=n)
    outcomes = (rng.uniform(0.0, 1.0, size=n) < probs).astype(float)

    curve = reliability_curve(probs, outcomes, n_bins=10)
    assert len(curve) == 10
    assert curve["count"].sum() == n
    # A well-calibrated forecaster's realized frequency should track its
    # own mean predicted probability within a small tolerance at this
    # sample size.
    assert (curve["realized_frequency"] - curve["mean_predicted"]).abs().max() < 0.05
