"""Offline calibration for the weather ensemble alpha source (design doc
§7, §12).

This is the research counterpart to
``crates/parallax-alpha/src/sources/weather.rs``'s ``WeatherEnsembleSource``:
that Rust code turns ensemble-member exceedance counts into a probability
estimate with an Agresti-Coull-style standard-error floor. This module
answers the question that Rust code can't answer on its own — *was that a
well-calibrated probability, against realized outcomes?* — so the
shrinkage constants baked into the hot path can be validated (or retuned)
before they ship, rather than trusted on the strength of the formula
alone.
"""

from __future__ import annotations

import numpy as np
import pandas as pd


def brier_score(probabilities: np.ndarray, outcomes: np.ndarray) -> float:
    """Mean squared error between forecast probability and the {0,1}
    outcome. 0 is a perfect forecaster; 0.25 is what an always-50%
    forecaster scores against a 50/50 base rate — any fitted model that
    can't beat 0.25 against its own base rate isn't adding information.
    """
    probabilities = np.asarray(probabilities, dtype=float)
    outcomes = np.asarray(outcomes, dtype=float)
    if probabilities.shape != outcomes.shape:
        raise ValueError("probabilities and outcomes must be the same shape")
    if probabilities.size == 0:
        raise ValueError("probabilities must be non-empty")
    return float(np.mean((probabilities - outcomes) ** 2))


def reliability_curve(probabilities: np.ndarray, outcomes: np.ndarray, n_bins: int = 10) -> pd.DataFrame:
    """Bins forecasts by predicted probability and reports the realized
    outcome frequency in each bin. A well-calibrated model's realized
    frequency should track the bin's predicted probability closely; a
    model whose 90%-confidence bin only resolves YES 60% of the time is
    overconfident, and the corresponding Rust `min_std_dev` floor should
    be raised rather than trusting the point estimate as-is.
    """
    probabilities = np.asarray(probabilities, dtype=float)
    outcomes = np.asarray(outcomes, dtype=float)
    if probabilities.shape != outcomes.shape:
        raise ValueError("probabilities and outcomes must be the same shape")

    bin_edges = np.linspace(0.0, 1.0, n_bins + 1)
    bin_idx = np.clip(np.digitize(probabilities, bin_edges) - 1, 0, n_bins - 1)

    frame = pd.DataFrame({"bin": bin_idx, "predicted": probabilities, "outcome": outcomes})
    grouped = (
        frame.groupby("bin")
        .agg(mean_predicted=("predicted", "mean"), realized_frequency=("outcome", "mean"), count=("outcome", "size"))
        .reset_index()
    )
    grouped["bin_lower"] = bin_edges[grouped["bin"]]
    grouped["bin_upper"] = bin_edges[grouped["bin"] + 1]
    return grouped[["bin_lower", "bin_upper", "mean_predicted", "realized_frequency", "count"]]


def ensemble_probability(forecasts: np.ndarray, threshold: float) -> tuple[float, float]:
    """Mirrors ``WeatherEnsembleSource``'s Rust logic exactly: the
    probability is the fraction of ensemble members exceeding
    ``threshold``, and the standard deviation uses the same
    Agresti-Coull-style shrinkage — ``sqrt((p*(1-p) + 0.25) / (n + 1))`` —
    so a value computed here is directly comparable to what the hot path
    would produce for identical input, letting this module validate the
    Rust formula against history rather than reinventing a different one.
    """
    forecasts = np.asarray(forecasts, dtype=float)
    if forecasts.size == 0:
        raise ValueError("forecasts must be non-empty")
    n = len(forecasts)
    p = float(np.mean(forecasts > threshold))
    std = float(np.sqrt((p * (1.0 - p) + 0.25) / (n + 1.0)))
    return p, std
