import json

from parallax_research.export import export_config


def test_export_config_writes_expected_json_shape(tmp_path):
    out_path = tmp_path / "nested" / "alpha_config.json"
    export_config(
        out_path,
        weather_min_std_dev=0.015,
        econ_surprise_std_by_series={"cpi_yoy": 0.2, "nfp": 45000.0},
        news_sensitivity=0.35,
    )

    assert out_path.exists()
    loaded = json.loads(out_path.read_text())
    assert loaded == {
        "weather": {"min_std_dev": 0.015},
        "econ": {"surprise_std_by_series": {"cpi_yoy": 0.2, "nfp": 45000.0}},
        "news": {"sensitivity": 0.35},
    }
