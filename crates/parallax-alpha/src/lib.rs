//! The alpha model layer (design doc §7): the `AlphaSource` plugin trait,
//! an inverse-variance-with-disagreement aggregator that turns many
//! sources' opinions into one `FairValue`, and seven concrete sources —
//! weather ensemble, econ nowcast, news sentiment, oracle resolution
//! (PARALLAX), plus the crypto reference-distance barrier model,
//! correlated-asset residual, and order-flow imbalance (APERTURE, design
//! doc §4) — each a generalized, from-scratch reimplementation of a
//! principle named in the relevant design doc, not a copy of any
//! reference project's code.

mod aggregator;
pub mod config;
mod forecast_quality;
mod source;
mod sources;
mod stats;

pub use aggregator::{aggregate, try_aggregate, AggregateError, AggregatorConfig};
pub use config::{AlphaConfig, ConfigError, EconConfig, NewsConfig, WeatherConfig};
pub use forecast_quality::{ForecastQualityTracker, ReliabilityPoint};
pub use source::AlphaSource;
pub use sources::{
    CorrelatedAssetSource, EconNowcastSource, NewsSentimentSource, OracleResolutionSource,
    OrderFlowImbalanceSource, ReferenceDistanceSource, WeatherEnsembleSource,
};
pub use stats::{
    beta_binomial_posterior, clamp_probability, normal_cdf, normal_pdf, prob_exceeds, PROB_EPSILON,
};
