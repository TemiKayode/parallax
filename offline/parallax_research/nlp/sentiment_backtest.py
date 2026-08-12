"""Offline evaluation harness for ``NewsSentimentSource``'s fixed
sensitivity constant (``crates/parallax-alpha/src/sources/news.rs``).

``NewsSentimentSource`` is deliberately not an NLP model itself — per its
own module doc, it converts an already-scored ``(polarity, relevance)``
pair into a probability nudge, on the premise that headline scoring is a
separately-trained model this repo doesn't own. This module checks
whether the Rust source's fixed sensitivity constant is actually
well-calibrated against historical ``(polarity, relevance, outcome)``
triples, using the same Brier-score criterion as the weather calibration
module, and can grid-search a better value.
"""

from __future__ import annotations

import numpy as np

from parallax_research.weather.calibration import brier_score


def score_sensitivity(
    polarity: np.ndarray, relevance: np.ndarray, outcomes: np.ndarray, sensitivity: float, base_rate: float = 0.5
) -> float:
    """Brier score of the ``NewsSentimentSource`` formula —
    ``clamp(base_rate + polarity * relevance * sensitivity, 0, 1)`` — at a
    given sensitivity constant. Lower is better.
    """
    polarity = np.clip(np.asarray(polarity, dtype=float), -1.0, 1.0)
    relevance = np.clip(np.asarray(relevance, dtype=float), 0.0, 1.0)
    probabilities = np.clip(base_rate + polarity * relevance * sensitivity, 0.0, 1.0)
    return brier_score(probabilities, np.asarray(outcomes, dtype=float))


def grid_search_sensitivity(
    polarity: np.ndarray, relevance: np.ndarray, outcomes: np.ndarray, grid: np.ndarray | None = None
) -> tuple[float, float]:
    """Sweeps the sensitivity constant over ``grid`` (default: 0.0 to 1.0
    in steps of 0.025) and returns ``(best_sensitivity, best_brier_score)``
    — the value to paste into ``NewsSentimentSource::new``'s
    ``sensitivity`` field if it beats the current constant.
    """
    if grid is None:
        grid = np.linspace(0.0, 1.0, 41)
    scored = [(float(s), score_sensitivity(polarity, relevance, outcomes, float(s))) for s in grid]
    return min(scored, key=lambda pair: pair[1])
