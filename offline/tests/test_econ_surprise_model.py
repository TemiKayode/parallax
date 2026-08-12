import numpy as np
import pytest

from parallax_research.econ.surprise_model import fit_logistic_link, historical_surprise_std


def test_historical_surprise_std_matches_manual_calculation():
    consensus = np.array([3.0, 3.1, 2.9, 3.2, 3.0])
    actual = np.array([3.2, 3.0, 2.8, 3.5, 2.9])
    expected = float(np.std(actual - consensus, ddof=1))
    assert historical_surprise_std(consensus, actual) == pytest.approx(expected)


def test_historical_surprise_std_requires_at_least_two_observations():
    with pytest.raises(ValueError):
        historical_surprise_std(np.array([3.0]), np.array([3.1]))


def test_historical_surprise_std_requires_matching_shapes():
    with pytest.raises(ValueError):
        historical_surprise_std(np.array([3.0, 3.1]), np.array([3.1]))


def test_fit_logistic_link_recovers_a_strong_positive_relationship():
    rng = np.random.default_rng(3)
    n = 500
    surprise = rng.normal(0.0, 1.0, size=n)
    # Outcomes strongly driven by surprise sign -> a real, learnable signal.
    prob_yes = 1.0 / (1.0 + np.exp(-2.5 * surprise))
    outcomes = (rng.uniform(0.0, 1.0, size=n) < prob_yes).astype(float)

    model = fit_logistic_link(surprise, outcomes)
    # A positive surprise should predict YES with high confidence.
    predicted = model.predict_proba(np.array([[2.0]]))[0][1]
    assert predicted > 0.85


def test_fit_logistic_link_requires_both_outcome_classes():
    surprise = np.array([0.1, 0.2, 0.3])
    outcomes = np.array([1.0, 1.0, 1.0])
    with pytest.raises(ValueError):
        fit_logistic_link(surprise, outcomes)
