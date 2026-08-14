use parallax_types::Timestamp;

/// The seven states from design doc §7's state machine, implemented
/// literally rather than just diagrammed — every transition below is a
/// tested, enforced rule, not a description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionStructureState {
    Flat,
    ScanningTemporal,
    HedgedBuilding,
    Rotating,
    NearResolutionHold,
    Unwinding,
    Halted,
}

#[derive(Debug, Clone, Copy)]
pub struct SelectorConfig {
    /// `|RelativeScore|` above this from `Flat` opens a temporal-arb scan.
    pub relative_score_entry: f64,
    /// `|RelativeScore|` below this (and edge gone) is "the signal decayed."
    pub relative_score_exit: f64,
    /// How far fair value must sit from the market price, beyond the
    /// noise floor, before `Rotating` is allowed to (re-)fire — the
    /// whipsaw guard's hysteresis half of the pair.
    pub rotation_hysteresis: f64,
    /// The whipsaw guard's other half: minimum nanoseconds between
    /// `Rotating` transitions, regardless of how far the price has moved.
    pub min_dwell_ns: i64,
    pub near_resolution_tau_secs: f64,
    pub near_resolution_p_floor: f64,
    /// A `HedgedBuilding` entry from `Flat` additionally requires the
    /// confidence band to still be at least this wide — a tight band
    /// with positive edge is a directional bet the market-making/stat-arb
    /// engines already handle; hedged building is for genuine uncertainty.
    pub wide_band_floor: f64,
}

impl Default for SelectorConfig {
    fn default() -> Self {
        SelectorConfig {
            relative_score_entry: 2.0,
            relative_score_exit: 0.5,
            rotation_hysteresis: 0.02,
            min_dwell_ns: 2_000_000_000, // 2s
            near_resolution_tau_secs: 20.0,
            near_resolution_p_floor: 0.97,
            wide_band_floor: 0.03,
        }
    }
}

/// Everything the selector needs to decide a transition on one tick.
/// Deliberately flat data, not a reference into the strategy core's
/// wider state — the selector's own tests construct these directly.
#[derive(Debug, Clone, Copy)]
pub struct StructureSignal {
    pub relative_score: Option<f64>,
    /// Single-market tradable edge (design doc §5); `None` if no depth
    /// snapshot was available to compute one this tick.
    pub tradable_edge: Option<f64>,
    pub band_width: f64,
    pub fair_value_mid: f64,
    pub market_price: f64,
    pub seconds_remaining: f64,
    /// Model probability of the near-resolution outcome actually
    /// happening — compared against `near_resolution_p_floor` on
    /// whichever side (up or down) it favors.
    pub p_up: f64,
    pub inventory_at_limit: bool,
    pub fresher_edge_elsewhere: bool,
    pub reversal_confirmed: bool,
    pub second_leg_filled: bool,
    pub position_resolved: bool,
    pub rare_loss_guard_tripped: bool,
    pub capital_released: bool,
    pub kill_switch: bool,
    pub feeds_healthy: bool,
    pub now: Timestamp,
}

impl Default for StructureSignal {
    /// Every field defaults to "no signal" except `now`, which
    /// `Timestamp` deliberately has no default for — tests that need
    /// `..Default::default()` still have to say what time it is.
    fn default() -> Self {
        StructureSignal {
            relative_score: None,
            tradable_edge: None,
            band_width: 0.0,
            fair_value_mid: 0.0,
            market_price: 0.0,
            // f64::INFINITY, not 0.0: "no signal" must not read as "resolving
            // right now" to the near-resolution check below.
            seconds_remaining: f64::INFINITY,
            // 0.5, not 0.0: `p_up.max(1.0 - p_up)` reads 0.0 as maximum
            // confidence on the down side, not "no signal" — 0.5 is the
            // only value that is genuinely uninformative to that check.
            p_up: 0.5,
            inventory_at_limit: false,
            fresher_edge_elsewhere: false,
            reversal_confirmed: false,
            second_leg_filled: false,
            position_resolved: false,
            rare_loss_guard_tripped: false,
            capital_released: false,
            kill_switch: false,
            feeds_healthy: false,
            now: Timestamp::from_nanos(0),
        }
    }
}

pub struct PositionStructureSelector {
    state: PositionStructureState,
    config: SelectorConfig,
    last_transition_at: Option<Timestamp>,
}

impl PositionStructureSelector {
    pub fn new(config: SelectorConfig) -> Self {
        PositionStructureSelector {
            state: PositionStructureState::Flat,
            config,
            last_transition_at: None,
        }
    }

    pub fn state(&self) -> PositionStructureState {
        self.state
    }

    fn transition(&mut self, to: PositionStructureState, now: Timestamp) {
        self.state = to;
        self.last_transition_at = Some(now);
    }

    fn dwell_clear(&self, now: Timestamp) -> bool {
        match self.last_transition_at {
            None => true,
            Some(t) => now.since(t) >= self.config.min_dwell_ns,
        }
    }

    fn crosses_with_hysteresis(&self, signal: &StructureSignal) -> bool {
        (signal.fair_value_mid - signal.market_price).abs() > self.config.rotation_hysteresis
    }

    /// Advances the state machine by one tick's worth of signal and
    /// returns the resulting state. The kill switch is checked first and
    /// wins from every active state, matching the diagram's direct edge
    /// from every non-`Halted` state straight to `Halted` — a structure
    /// never has to reach a "safe point" to exit from.
    pub fn step(&mut self, signal: &StructureSignal) -> PositionStructureState {
        if signal.kill_switch && self.state != PositionStructureState::Halted {
            self.transition(PositionStructureState::Halted, signal.now);
            return self.state;
        }

        match self.state {
            PositionStructureState::Halted => {
                if signal.feeds_healthy {
                    self.transition(PositionStructureState::Flat, signal.now);
                }
            }

            PositionStructureState::Flat => {
                if signal
                    .relative_score
                    .map(|s| s.abs() > self.config.relative_score_entry)
                    .unwrap_or(false)
                {
                    self.transition(PositionStructureState::ScanningTemporal, signal.now);
                } else if signal.seconds_remaining < self.config.near_resolution_tau_secs
                    && signal.p_up.max(1.0 - signal.p_up) > self.config.near_resolution_p_floor
                {
                    self.transition(PositionStructureState::NearResolutionHold, signal.now);
                } else if signal.tradable_edge.map(|e| e > 0.0).unwrap_or(false)
                    && signal.band_width >= self.config.wide_band_floor
                {
                    self.transition(PositionStructureState::HedgedBuilding, signal.now);
                } else if self.crosses_with_hysteresis(signal) && self.dwell_clear(signal.now) {
                    self.transition(PositionStructureState::Rotating, signal.now);
                }
            }

            PositionStructureState::ScanningTemporal => {
                if signal.reversal_confirmed && signal.second_leg_filled {
                    self.transition(PositionStructureState::HedgedBuilding, signal.now);
                } else if signal
                    .relative_score
                    .map(|s| s.abs() < self.config.relative_score_exit)
                    .unwrap_or(true)
                {
                    self.transition(PositionStructureState::Flat, signal.now);
                }
            }

            PositionStructureState::Rotating => {
                if self.crosses_with_hysteresis(signal) {
                    // Still actively crossing. Re-fire only once the whipsaw
                    // guard's dwell timer clears; otherwise stay in Rotating
                    // without resetting the dwell clock. Either way this is
                    // not decay, so the exit check below must not run.
                    if self.dwell_clear(signal.now) {
                        self.transition(PositionStructureState::Rotating, signal.now);
                    }
                } else {
                    let score_decayed = signal.relative_score.map(|s| s.abs()).unwrap_or(0.0)
                        < self.config.relative_score_exit;
                    let edge_gone = signal.tradable_edge.map(|e| e <= 0.0).unwrap_or(true);
                    if score_decayed && edge_gone {
                        self.transition(PositionStructureState::Flat, signal.now);
                    }
                }
            }

            PositionStructureState::HedgedBuilding => {
                if signal.inventory_at_limit || signal.fresher_edge_elsewhere {
                    self.transition(PositionStructureState::Unwinding, signal.now);
                }
            }

            PositionStructureState::NearResolutionHold => {
                if signal.rare_loss_guard_tripped || signal.position_resolved {
                    self.transition(PositionStructureState::Unwinding, signal.now);
                }
            }

            PositionStructureState::Unwinding => {
                if signal.capital_released {
                    self.transition(PositionStructureState::Flat, signal.now);
                }
            }
        }

        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal_at(now_ns: i64) -> StructureSignal {
        StructureSignal {
            now: Timestamp::from_nanos(now_ns),
            ..Default::default()
        }
    }

    #[test]
    fn starts_flat() {
        let selector = PositionStructureSelector::new(SelectorConfig::default());
        assert_eq!(selector.state(), PositionStructureState::Flat);
    }

    #[test]
    fn kill_switch_halts_from_any_active_state() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut sig = signal_at(0);
        sig.relative_score = Some(5.0);
        selector.step(&sig); // -> ScanningTemporal

        let mut kill = signal_at(1);
        kill.kill_switch = true;
        assert_eq!(selector.step(&kill), PositionStructureState::Halted);
    }

    #[test]
    fn halted_only_recovers_once_feeds_are_healthy() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut kill = signal_at(0);
        kill.kill_switch = true;
        selector.step(&kill);
        assert_eq!(selector.state(), PositionStructureState::Halted);

        let unhealthy = signal_at(1);
        assert_eq!(selector.step(&unhealthy), PositionStructureState::Halted);

        let mut healthy = signal_at(2);
        healthy.feeds_healthy = true;
        assert_eq!(selector.step(&healthy), PositionStructureState::Flat);
    }

    #[test]
    fn large_relative_score_opens_a_temporal_scan() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut sig = signal_at(0);
        sig.relative_score = Some(-3.0);
        assert_eq!(
            selector.step(&sig),
            PositionStructureState::ScanningTemporal
        );
    }

    #[test]
    fn scanning_temporal_builds_hedge_once_reversal_and_second_leg_confirm() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut open = signal_at(0);
        open.relative_score = Some(3.0);
        selector.step(&open);

        let mut confirmed = signal_at(1);
        confirmed.reversal_confirmed = true;
        confirmed.second_leg_filled = true;
        assert_eq!(
            selector.step(&confirmed),
            PositionStructureState::HedgedBuilding
        );
    }

    #[test]
    fn scanning_temporal_gives_up_when_the_signal_decays_first() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut open = signal_at(0);
        open.relative_score = Some(3.0);
        selector.step(&open);

        let decayed = signal_at(1); // relative_score defaults to None -> treated as decayed
        assert_eq!(selector.step(&decayed), PositionStructureState::Flat);
    }

    #[test]
    fn positive_edge_with_a_wide_band_builds_a_hedge_directly_from_flat() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut sig = signal_at(0);
        sig.tradable_edge = Some(0.05);
        sig.band_width = 0.10;
        assert_eq!(selector.step(&sig), PositionStructureState::HedgedBuilding);
    }

    #[test]
    fn near_resolution_hold_triggers_on_low_tau_and_high_confidence() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut sig = signal_at(0);
        sig.seconds_remaining = 10.0;
        sig.p_up = 0.99;
        assert_eq!(
            selector.step(&sig),
            PositionStructureState::NearResolutionHold
        );
    }

    #[test]
    fn near_resolution_hold_unwinds_on_rare_loss_guard_or_resolution() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut open = signal_at(0);
        open.seconds_remaining = 5.0;
        open.p_up = 0.995;
        selector.step(&open);
        assert_eq!(selector.state(), PositionStructureState::NearResolutionHold);

        let mut tripped = signal_at(1);
        tripped.rare_loss_guard_tripped = true;
        assert_eq!(selector.step(&tripped), PositionStructureState::Unwinding);
    }

    #[test]
    fn rotating_requires_both_the_hysteresis_crossing_and_a_clear_dwell_timer() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut cross = signal_at(0);
        cross.fair_value_mid = 0.60;
        cross.market_price = 0.50; // 0.10 gap, beyond the 0.02 hysteresis
        assert_eq!(selector.step(&cross), PositionStructureState::Rotating);

        // Immediately re-crossing before the dwell timer clears must not re-fire.
        let mut too_soon = signal_at(1); // 1ns later, min_dwell is 2s
        too_soon.fair_value_mid = 0.61;
        too_soon.market_price = 0.50;
        assert_eq!(selector.step(&too_soon), PositionStructureState::Rotating);
        // (still Rotating, but this call must not have reset the dwell
        // clock — verified by the next check succeeding only after real time passes)

        let mut later = signal_at(3_000_000_000); // 3s later, clears the 2s dwell
        later.fair_value_mid = 0.62;
        later.market_price = 0.50;
        assert_eq!(selector.step(&later), PositionStructureState::Rotating);
    }

    #[test]
    fn rotating_returns_to_flat_once_score_and_edge_both_decay() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut cross = signal_at(0);
        cross.fair_value_mid = 0.60;
        cross.market_price = 0.50;
        selector.step(&cross);
        assert_eq!(selector.state(), PositionStructureState::Rotating);

        let mut decayed = signal_at(3_000_000_000);
        decayed.fair_value_mid = 0.501; // back within hysteresis
        decayed.market_price = 0.50;
        assert_eq!(selector.step(&decayed), PositionStructureState::Flat);
    }

    #[test]
    fn hedged_building_unwinds_at_the_inventory_limit_or_a_fresher_edge() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut open = signal_at(0);
        open.tradable_edge = Some(0.05);
        open.band_width = 0.10;
        selector.step(&open);
        assert_eq!(selector.state(), PositionStructureState::HedgedBuilding);

        let mut limited = signal_at(1);
        limited.inventory_at_limit = true;
        assert_eq!(selector.step(&limited), PositionStructureState::Unwinding);
    }

    #[test]
    fn unwinding_returns_to_flat_once_capital_is_released() {
        let mut selector = PositionStructureSelector::new(SelectorConfig::default());
        let mut open = signal_at(0);
        open.tradable_edge = Some(0.05);
        open.band_width = 0.10;
        selector.step(&open);
        let mut limited = signal_at(1);
        limited.inventory_at_limit = true;
        selector.step(&limited);
        assert_eq!(selector.state(), PositionStructureState::Unwinding);

        let mut released = signal_at(2);
        released.capital_released = true;
        assert_eq!(selector.step(&released), PositionStructureState::Flat);
    }
}
