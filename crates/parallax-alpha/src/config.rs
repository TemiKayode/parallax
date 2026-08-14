//! Deserializes exactly the JSON shape `parallax_research.export.
//! export_config` (the offline Python side, design doc §12) writes: every
//! constant an alpha source hardcoded — a weather std-dev floor, a
//! per-series economic surprise std, a news sensitivity — ships as data
//! here instead. Before this existed, the offline research loop could
//! report that a constant was miscalibrated and had no way to actually
//! change it: `export_config` wrote a JSON artifact nothing in Rust ever
//! read (design doc review 2.13). The Rust fixture and the Python
//! exporter test parse the same bytes, so a schema change here breaks one
//! of the two suites rather than silently shipping stale constants.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct WeatherConfig {
    pub min_std_dev: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EconConfig {
    pub surprise_std_by_series: HashMap<String, f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewsConfig {
    pub sensitivity: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlphaConfig {
    pub weather: WeatherConfig,
    pub econ: EconConfig,
    pub news: NewsConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl AlphaConfig {
    pub fn from_json(s: &str) -> Result<Self, ConfigError> {
        let config: AlphaConfig = serde_json::from_str(s)?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json(&text)
    }

    /// Rejects a config whose constants would silently *disable* the
    /// model they configure rather than merely retune it — a zero or
    /// negative `min_std_dev`/`sensitivity` reads everywhere downstream
    /// as "more confident," not "broken" (the same class of bug §1.1
    /// fixes for live data, applied to the config that ships from offline
    /// research instead).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(self.weather.min_std_dev > 0.0 && self.weather.min_std_dev.is_finite()) {
            return Err(ConfigError::Invalid(format!(
                "weather.min_std_dev must be positive and finite, got {}",
                self.weather.min_std_dev
            )));
        }
        if !(self.news.sensitivity.is_finite() && self.news.sensitivity >= 0.0) {
            return Err(ConfigError::Invalid(format!(
                "news.sensitivity must be non-negative and finite, got {}",
                self.news.sensitivity
            )));
        }
        for (series, std) in &self.econ.surprise_std_by_series {
            if !(std.is_finite() && *std > 0.0) {
                return Err(ConfigError::Invalid(format!(
                    "econ.surprise_std_by_series[{series}] must be positive and finite, got {std}"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape `offline/tests/test_export.py` asserts
    /// `export_config` writes — this is the shared fixture the review
    /// promised: a schema change on either side breaks the other suite.
    fn exported_json() -> &'static str {
        r#"{
            "weather": {"min_std_dev": 0.015},
            "econ": {"surprise_std_by_series": {"cpi_yoy": 0.2, "nfp": 45000.0}},
            "news": {"sensitivity": 0.35}
        }"#
    }

    #[test]
    fn parses_the_exporters_exact_json_shape() {
        let config = AlphaConfig::from_json(exported_json()).unwrap();
        assert_eq!(config.weather.min_std_dev, 0.015);
        assert_eq!(
            config.econ.surprise_std_by_series.get("nfp"),
            Some(&45000.0)
        );
        assert_eq!(config.news.sensitivity, 0.35);
    }

    #[test]
    fn rejects_a_zero_min_std_dev_that_would_silently_overclaim_confidence() {
        let bad = r#"{
            "weather": {"min_std_dev": 0.0},
            "econ": {"surprise_std_by_series": {}},
            "news": {"sensitivity": 0.35}
        }"#;
        assert!(AlphaConfig::from_json(bad).is_err());
    }

    #[test]
    fn rejects_a_negative_series_surprise_std() {
        let bad = r#"{
            "weather": {"min_std_dev": 0.01},
            "econ": {"surprise_std_by_series": {"cpi_yoy": -0.2}},
            "news": {"sensitivity": 0.35}
        }"#;
        assert!(AlphaConfig::from_json(bad).is_err());
    }

    #[test]
    fn load_reads_and_parses_a_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "parallax_alpha_config_test_{}.json",
            std::process::id()
        ));
        std::fs::write(&path, exported_json()).unwrap();
        let config = AlphaConfig::load(&path).unwrap();
        assert_eq!(config.news.sensitivity, 0.35);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn malformed_json_is_a_clear_error_not_a_panic() {
        assert!(AlphaConfig::from_json("not json").is_err());
    }
}
