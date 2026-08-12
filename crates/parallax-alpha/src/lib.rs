//! The alpha model layer (design doc §7): the `AlphaSource` plugin trait,
//! an inverse-variance-with-disagreement aggregator that turns many
//! sources' opinions into one `FairValue`, and four concrete sources —
//! weather ensemble, econ nowcast, news sentiment, and oracle resolution
//! — each a generalized, from-scratch reimplementation of a principle
//! named in the design doc, not a copy of any reference project's code.

mod aggregator;
mod source;
mod sources;
mod stats;

pub use aggregator::{aggregate, AggregatorConfig};
pub use source::AlphaSource;
pub use sources::{
    EconNowcastSource, NewsSentimentSource, OracleResolutionSource, WeatherEnsembleSource,
};
