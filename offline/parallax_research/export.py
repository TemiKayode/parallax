"""Exports fitted offline parameters into the JSON config artifact the
Rust hot path loads at startup (design doc §11 / §12: "heavier model
fitting... ships as a config/weights artifact the hot path loads, never
trained in production"). This keeps the boundary explicit — numbers
computed here are data, not code, and the hot path never depends on
Python at runtime, only on this file's JSON output.
"""

from __future__ import annotations

import json
from pathlib import Path


def export_config(
    path: Path,
    *,
    weather_min_std_dev: float,
    econ_surprise_std_by_series: dict[str, float],
    news_sensitivity: float,
) -> None:
    config = {
        "weather": {"min_std_dev": weather_min_std_dev},
        "econ": {"surprise_std_by_series": econ_surprise_std_by_series},
        "news": {"sensitivity": news_sensitivity},
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(config, indent=2, sort_keys=True))
