use crate::kill_switch::KillSwitch;
use parallax_book::ConsolidatedBook;
use parallax_types::{
    CanonicalContractId, ClusterKey, OrderIntent, Position, Side, Timestamp, VenueId,
};
use std::collections::HashMap;

type PositionMap = HashMap<(VenueId, CanonicalContractId), Position>;

#[derive(Debug, Clone, PartialEq)]
pub enum RejectReason {
    KillSwitch { reason: String },
    NoMarketData,
    FeedStale { age_ns: i64, max_ns: i64 },
    ContractLimitExceeded { limit: f64, projected: f64 },
    ClusterLimitExceeded { limit: f64, projected: f64 },
    VenueLimitExceeded { limit: f64, projected: f64 },
    GrossLimitExceeded { limit: f64, projected: f64 },
}

#[derive(Debug, Clone, Copy)]
pub struct RiskLimits {
    pub max_abs_qty_per_contract: f64,
    /// Correlated contracts (design doc §10 — e.g. three temperature
    /// thresholds on the same city/date) share this budget instead of
    /// each getting the full per-contract limit independently.
    pub max_abs_qty_per_cluster: f64,
    pub max_gross_qty_per_venue: f64,
    pub max_gross_qty_total: f64,
    pub max_feed_staleness_ns: i64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        RiskLimits {
            max_abs_qty_per_contract: 500.0,
            max_abs_qty_per_cluster: 800.0,
            max_gross_qty_per_venue: 2_000.0,
            max_gross_qty_total: 3_000.0,
            max_feed_staleness_ns: 5_000_000_000, // 5s
        }
    }
}

/// The single arbitration point every strategy engine's proposed order
/// passes through before it may reach a venue adapter (design doc §8/§10).
/// Reference implementation: correctness-first, scanning the full
/// position map per check rather than the incrementally-maintained SIMD
/// aggregates a production hot path would use — see the module doc for
/// the intended optimization path.
pub struct RiskGate {
    limits: RiskLimits,
    positions: PositionMap,
    cluster_of: HashMap<CanonicalContractId, ClusterKey>,
    kill_switch: KillSwitch,
}

impl RiskGate {
    pub fn new(limits: RiskLimits) -> Self {
        RiskGate {
            limits,
            positions: HashMap::new(),
            cluster_of: HashMap::new(),
            kill_switch: KillSwitch::new(),
        }
    }

    pub fn kill_switch_mut(&mut self) -> &mut KillSwitch {
        &mut self.kill_switch
    }

    /// Ingestion/normalization calls this once per newly-seen canonical
    /// contract so the risk gate knows which cluster to net it into.
    /// Contracts never registered fall back to a singleton cluster keyed
    /// on their own id, which is always safe (just less netting) rather
    /// than a panic on an unregistered instrument.
    pub fn register_contract(&mut self, contract: CanonicalContractId, cluster: ClusterKey) {
        self.cluster_of.insert(contract, cluster);
    }

    fn cluster_of(&self, contract: &CanonicalContractId) -> ClusterKey {
        self.cluster_of
            .get(contract)
            .cloned()
            .unwrap_or_else(|| ClusterKey(contract.0.clone()))
    }

    pub fn position_qty(&self, venue: VenueId, contract: &CanonicalContractId) -> f64 {
        Self::qty_in(&self.positions, venue, contract)
    }

    fn qty_in(positions: &PositionMap, venue: VenueId, contract: &CanonicalContractId) -> f64 {
        positions
            .get(&(venue, contract.clone()))
            .map(|p| p.qty)
            .unwrap_or(0.0)
    }

    /// This contract's net position per venue — exactly the shape
    /// `StrategyInput::inventory` expects, and deliberately scoped to one
    /// contract rather than exposing the whole book (design doc §8: an
    /// engine sees its own inventory, nothing more).
    pub fn inventory_for(&self, contract: &CanonicalContractId) -> HashMap<VenueId, f64> {
        self.positions
            .iter()
            .filter(|((_, c), _)| c == contract)
            .map(|((v, _), p)| (*v, p.qty))
            .collect()
    }

    /// Read-only snapshot of every position, for reporting/PnL markout —
    /// not consumed by any trading-path logic.
    pub fn positions_snapshot(&self) -> Vec<(VenueId, CanonicalContractId, Position)> {
        self.positions
            .iter()
            .map(|((v, c), p)| (*v, c.clone(), p.clone()))
            .collect()
    }

    pub fn record_fill(
        &mut self,
        venue: VenueId,
        contract: &CanonicalContractId,
        signed_qty: f64,
        price: f64,
    ) {
        let entry = self
            .positions
            .entry((venue, contract.clone()))
            .or_insert_with(|| Position::flat(venue, contract.clone()));
        entry.apply_fill(signed_qty, price);
    }

    fn signed_delta(intent: &OrderIntent) -> f64 {
        match intent.side {
            Side::Buy => intent.size,
            Side::Sell => -intent.size,
        }
    }

    fn cluster_qty_after(
        &self,
        positions: &PositionMap,
        cluster: &ClusterKey,
        contract: &CanonicalContractId,
        venue: VenueId,
        delta: f64,
    ) -> f64 {
        let mut total = 0.0;
        let mut touched = false;
        for ((v, c), p) in positions.iter() {
            let same_slot = *v == venue && c == contract;
            if self.cluster_of(c) == *cluster {
                if same_slot {
                    total += p.qty + delta;
                    touched = true;
                } else {
                    total += p.qty;
                }
            }
        }
        if !touched {
            total += delta;
        }
        total
    }

    fn venue_gross_after(
        &self,
        positions: &PositionMap,
        venue: VenueId,
        contract: &CanonicalContractId,
        delta: f64,
    ) -> f64 {
        let mut total = 0.0;
        let mut touched = false;
        for ((v, c), p) in positions.iter() {
            if *v != venue {
                continue;
            }
            if c == contract {
                total += (p.qty + delta).abs();
                touched = true;
            } else {
                total += p.qty.abs();
            }
        }
        if !touched {
            total += delta.abs();
        }
        total
    }

    fn total_gross_after(
        &self,
        positions: &PositionMap,
        venue: VenueId,
        contract: &CanonicalContractId,
        delta: f64,
    ) -> f64 {
        let mut total = 0.0;
        let mut touched = false;
        for ((v, c), p) in positions.iter() {
            if *v == venue && c == contract {
                total += (p.qty + delta).abs();
                touched = true;
            } else {
                total += p.qty.abs();
            }
        }
        if !touched {
            total += delta.abs();
        }
        total
    }

    fn check_against(
        &self,
        positions: &PositionMap,
        intent: &OrderIntent,
        book: &ConsolidatedBook,
        now: Timestamp,
    ) -> Result<(), RejectReason> {
        if let Some(reason) = self
            .kill_switch
            .reason_if_tripped(intent.venue, &intent.contract)
        {
            return Err(RejectReason::KillSwitch { reason });
        }

        let fresh_tick = book
            .quotes(&intent.contract)
            .find(|t| t.venue == intent.venue);
        match fresh_tick {
            None => return Err(RejectReason::NoMarketData),
            Some(tick) => {
                let age = now.since(tick.receive_ts);
                if age > self.limits.max_feed_staleness_ns {
                    return Err(RejectReason::FeedStale {
                        age_ns: age,
                        max_ns: self.limits.max_feed_staleness_ns,
                    });
                }
            }
        }

        let delta = Self::signed_delta(intent);

        let projected_contract = Self::qty_in(positions, intent.venue, &intent.contract) + delta;
        if projected_contract.abs() > self.limits.max_abs_qty_per_contract {
            return Err(RejectReason::ContractLimitExceeded {
                limit: self.limits.max_abs_qty_per_contract,
                projected: projected_contract.abs(),
            });
        }

        let cluster = self.cluster_of(&intent.contract);
        let projected_cluster =
            self.cluster_qty_after(positions, &cluster, &intent.contract, intent.venue, delta);
        if projected_cluster.abs() > self.limits.max_abs_qty_per_cluster {
            return Err(RejectReason::ClusterLimitExceeded {
                limit: self.limits.max_abs_qty_per_cluster,
                projected: projected_cluster.abs(),
            });
        }

        let projected_venue_gross =
            self.venue_gross_after(positions, intent.venue, &intent.contract, delta);
        if projected_venue_gross > self.limits.max_gross_qty_per_venue {
            return Err(RejectReason::VenueLimitExceeded {
                limit: self.limits.max_gross_qty_per_venue,
                projected: projected_venue_gross,
            });
        }

        let projected_total_gross =
            self.total_gross_after(positions, intent.venue, &intent.contract, delta);
        if projected_total_gross > self.limits.max_gross_qty_total {
            return Err(RejectReason::GrossLimitExceeded {
                limit: self.limits.max_gross_qty_total,
                projected: projected_total_gross,
            });
        }

        Ok(())
    }

    /// The risk gate every `OrderIntent` must clear before reaching an
    /// execution adapter. `Ok(())` means the order may proceed exactly as
    /// proposed — this gate rejects, it does not resize.
    pub fn check(
        &self,
        intent: &OrderIntent,
        book: &ConsolidatedBook,
        now: Timestamp,
    ) -> Result<(), RejectReason> {
        self.check_against(&self.positions, intent, book, now)
    }

    /// Checks a batch of intents — typically one tick's proposals from
    /// market making, stat-arb, and sniping together — against a shared
    /// scratch copy of the position book, applying each accepted intent's
    /// delta before checking the next. This is what stops three engines
    /// that each individually clear a limit from collectively blowing
    /// through it in the same tick (design doc §8/§10). The scratch copy
    /// is discarded after the batch; real positions only move via
    /// `record_fill` once a venue actually confirms.
    pub fn check_batch(
        &self,
        intents: &[OrderIntent],
        book: &ConsolidatedBook,
        now: Timestamp,
    ) -> Vec<Result<(), RejectReason>> {
        let mut scratch = self.positions.clone();
        let mut results = Vec::with_capacity(intents.len());
        for intent in intents {
            let result = self.check_against(&scratch, intent, book, now);
            if result.is_ok() {
                let delta = Self::signed_delta(intent);
                let entry = scratch
                    .entry((intent.venue, intent.contract.clone()))
                    .or_insert_with(|| Position::flat(intent.venue, intent.contract.clone()));
                entry.qty += delta;
            }
            results.push(result);
        }
        results
    }
}
