//! The alpha model layer (design doc §7): the `AlphaSource` plugin trait,
//! an inverse-variance-with-disagreement aggregator that turns many
//! sources' opinions into one `FairValue`, and eight concrete sources —
//! weather ensemble, econ nowcast, news sentiment, oracle resolution
//! (PARALLAX), the crypto reference-distance barrier model,
//! correlated-asset residual, and order-flow imbalance (APERTURE, design
//! doc §4), plus a tennis match-state source for sports event markets —
//! each a generalized, from-scratch reimplementation of a principle
//! named in the relevant design doc, not a copy of any reference
//! project's code.

#![forbid(unsafe_code)]

mod aggregator;
pub mod config;
mod forecast_quality;
mod source;
mod sources;
mod stats;

pub use aggregator::{aggregate, try_aggregate, AggregateError, AggregatorConfig};
pub use config::{AlphaConfig, ConfigError, EconConfig, NewsConfig, TennisConfig, WeatherConfig};
pub use forecast_quality::{ForecastQualityTracker, ReliabilityPoint};
pub use source::AlphaSource;
pub use sources::{
    is_break_point, CorrelatedAssetSource, EconNowcastSource, NewsSentimentSource,
    OracleResolutionSource, OrderFlowImbalanceSource, ReferenceDistanceSource, TennisFeedAlert,
    TennisFeedHealth, TennisMatchStateSource, WeatherEnsembleSource,
};
pub use stats::{
    beta_binomial_posterior, clamp_probability, normal_cdf, normal_pdf, prob_exceeds, PROB_EPSILON,
};
