import numpy as np

from parallax_research.nlp.sentiment_backtest import grid_search_sensitivity, score_sensitivity


def _synthetic_dataset(true_sensitivity: float, n: int = 4000, seed: int = 11):
    rng = np.random.default_rng(seed)
    polarity = rng.uniform(-1.0, 1.0, size=n)
    relevance = rng.uniform(0.0, 1.0, size=n)
    prob = np.clip(0.5 + polarity * relevance * true_sensitivity, 0.0, 1.0)
    outcomes = (rng.uniform(0.0, 1.0, size=n) < prob).astype(float)
    return polarity, relevance, outcomes


def test_score_sensitivity_is_lower_at_the_true_generating_value():
    polarity, relevance, outcomes = _synthetic_dataset(true_sensitivity=0.4)
    score_at_true = score_sensitivity(polarity, relevance, outcomes, sensitivity=0.4)
    score_far_off = score_sensitivity(polarity, relevance, outcomes, sensitivity=0.0)
    assert score_at_true < score_far_off


def test_grid_search_recovers_the_true_sensitivity_reasonably_closely():
    polarity, relevance, outcomes = _synthetic_dataset(true_sensitivity=0.4)
    best_sensitivity, best_score = grid_search_sensitivity(polarity, relevance, outcomes)
    assert abs(best_sensitivity - 0.4) <= 0.075
    assert best_score < 0.25
