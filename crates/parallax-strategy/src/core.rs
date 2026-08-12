use crate::calibration::Calibrator;
use parallax_book::ConsolidatedBook;
use parallax_types::{CanonicalContractId, EngineId, FairValue, OrderIntent, Timestamp, VenueId};
use std::collections::HashMap;

/// Everything a strategy engine is allowed to see when deciding whether
/// to propose an order. This is the architectural enforcement point
/// described in design doc §8: there is no field here for another
/// party's orders, wallet activity, or observed flow — only PARALLAX's
/// own alpha output, the consolidated book, its own inventory, and its
/// own calibration state. An engine cannot condition on what it cannot
/// see.
pub struct StrategyInput<'a> {
    pub contract: &'a CanonicalContractId,
    pub fair_value: &'a FairValue,
    pub book: &'a ConsolidatedBook,
    /// PARALLAX's own net position in this contract, per venue.
    pub inventory: &'a HashMap<VenueId, f64>,
    pub calibration: &'a Calibrator,
    pub now: Timestamp,
}

pub trait StrategyEngine {
    fn id(&self) -> EngineId;
    /// Propose zero or more orders for this tick. Proposals are not yet
    /// risk-checked — every element of the returned vec still has to
    /// clear `RiskGate::check_batch` before it may reach a venue.
    fn evaluate(&self, input: &StrategyInput) -> Vec<OrderIntent>;
}

/// Runs every registered engine against the same snapshot and returns
/// their combined proposals in a fixed priority order: liquidity sniping
/// first (the most time-sensitive — a stale-quote window closes fast),
/// then stat-arb (directional, less urgent), then market making (steady-
/// state ladder refresh). The order matters once the combined list goes
/// through `RiskGate::check_batch`: when two proposals would jointly
/// blow through a limit, the higher-priority one wins the shared budget.
pub struct StrategyCore {
    engines: Vec<Box<dyn StrategyEngine + Send + Sync>>,
}

impl StrategyCore {
    pub fn new(engines: Vec<Box<dyn StrategyEngine + Send + Sync>>) -> Self {
        StrategyCore { engines }
    }

    pub fn evaluate_all(&self, input: &StrategyInput) -> Vec<OrderIntent> {
        let mut out = Vec::new();
        for engine in &self.engines {
            out.extend(engine.evaluate(input));
        }
        out
    }
}
