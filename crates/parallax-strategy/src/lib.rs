//! The strategy core (design doc §8): three engines — market making,
//! stat-arb, and liquidity sniping — sharing one `StrategyInput` snapshot
//! whose type has no slot for another party's orders or flow, a
//! lightweight online calibration layer (§11), and the `StrategyCore`
//! that runs them in priority order ahead of the risk gate's batch check.

mod calibration;
mod core;
mod engines;

pub use calibration::{CalibrationEstimate, Calibrator};
pub use core::{StrategyCore, StrategyEngine, StrategyInput};
pub use engines::{
    LiquiditySnipingEngine, MarketMakingConfig, MarketMakingEngine, SnipingConfig, StatArbConfig,
    StatArbEngine,
};
