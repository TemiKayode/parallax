//! The strategy core (design doc §8): three engines — market making,
//! stat-arb, and liquidity sniping — sharing one `StrategyInput` snapshot
//! whose type has no slot for another party's orders or flow, a
//! lightweight online calibration layer (§11), and the `StrategyCore`
//! that runs them in priority order ahead of the risk gate's batch check.

mod calibration;
mod core;
mod edge;
mod engines;
mod position_structure;
mod relative_value;

pub use calibration::{CalibrationEstimate, Calibrator, FillOutcome};
pub use core::{StrategyCore, StrategyEngine, StrategyInput};
pub use edge::{
    compute_tradable_edge, largest_profitable_size, EdgeConfig, EdgeContext, EdgeResult,
};
pub use engines::{
    LiquiditySnipingEngine, MarketMakingConfig, MarketMakingEngine, SnipingConfig, StatArbConfig,
    StatArbEngine,
};
pub use position_structure::{
    PositionStructureSelector, PositionStructureState, SelectorConfig, StructureSignal,
};
pub use relative_value::RelativeValueMonitor;
