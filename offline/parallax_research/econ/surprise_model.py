"""Offline fitting for the econ-release nowcast used by
``EconNowcastSource`` (``crates/parallax-alpha/src/sources/econ.rs``).

The Rust source prices a release against a normal-CDF link
(``prob_exceeds`` in ``crates/parallax-alpha/src/stats.rs``) using a
per-series ``surprise_std`` supplied by ingestion config. This module is
where that constant — and, as a validation check, the shape of the link
function itself — gets estimated from history before shipping.
"""

from __future__ import annotations

import numpy as np
from sklearn.linear_model import LogisticRegression


def historical_surprise_std(consensus: np.ndarray, actual: np.ndarray) -> float:
    """Standard deviation of ``actual - consensus`` across historical
    releases of one series — exactly the value the Rust
    ``EconNowcastSource`` expects as its ``surprise_std`` config field.
    """
    consensus = np.asarray(consensus, dtype=float)
    actual = np.asarray(actual, dtype=float)
    if consensus.shape != actual.shape:
        raise ValueError("consensus and actual must be the same shape")
    if len(consensus) < 2:
        raise ValueError("need at least two paired (consensus, actual) observations")
    return float(np.std(actual - consensus, ddof=1))


def fit_logistic_link(standardized_surprise: np.ndarray, outcomes: np.ndarray) -> LogisticRegression:
    """Fits P(outcome=1 | standardized surprise) via logistic regression,
    as a check against the Rust source's normal-CDF link assumption — if
    the fitted logistic slope/intercept diverge meaningfully from what a
    standard-normal CDF would imply, that's a signal the parametric link
    itself needs revisiting, not just the ``surprise_std`` constant.
    """
    x = np.asarray(standardized_surprise, dtype=float).reshape(-1, 1)
    y = np.asarray(outcomes, dtype=float)
    if len(x) != len(y):
        raise ValueError("standardized_surprise and outcomes must be the same length")
    if len(np.unique(y)) < 2:
        raise ValueError("need both outcome classes present to fit a logistic link")
    model = LogisticRegression()
    model.fit(x, y)
    return model
