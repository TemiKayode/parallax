use crate::kill_switch::KillSwitch;
use parallax_book::ConsolidatedBook;
use parallax_types::{
    CanonicalContractId, ClusterKey, OrderIntent, Position, Side, Timestamp, ValidationError,
    VenueId,
};
use std::collections::{BTreeSet, HashMap};

type PositionMap = HashMap<(VenueId, CanonicalContractId), Position>;
type ReservationMap = HashMap<(VenueId, CanonicalContractId), Reservation>;

#[derive(Debug, Clone, PartialEq)]
pub enum RejectReason {
    /// The gate has not yet been told what PARALLAX's real position is
    /// (design doc review 1.8) — an empty position map after a restart
    /// means "unknown," not "flat," and trading on unknown-as-zero
    /// re-enters a position the account already holds.
    NotReconciled,
    KillSwitch {
        reason: String,
    },
    NoMarketData,
    FeedStale {
        age_ns: i64,
        max_ns: i64,
    },
    /// The feed's `receive_ts` is in the future relative to `now` by more
    /// than can be explained by ordinary jitter — a negative age would
    /// otherwise pass every `age > max` staleness check and read as
    /// "permanently fresh" (design doc review 3.7).
    ClockSkew {
        skew_ns: i64,
    },
    /// The order's price is further from the venue's own touch than
    /// `max_price_through_book` allows — a fat-finger or runaway model
    /// guard, independent of every position-size limit below (design doc
    /// review 3.8).
    PriceThroughBook {
        touch: f64,
        price: f64,
        max_through: f64,
    },
    Invalid(ValidationError),
    ContractLimitExceeded {
        limit: f64,
        projected: f64,
    },
    ClusterLimitExceeded {
        limit: f64,
        projected: f64,
    },
    VenueLimitExceeded {
        limit: f64,
        projected: f64,
    },
    GrossLimitExceeded {
        limit: f64,
        projected: f64,
    },
    NotionalPerOrderExceeded {
        limit: f64,
        projected: f64,
    },
    NotionalVenueExceeded {
        limit: f64,
        projected: f64,
    },
    NotionalTotalExceeded {
        limit: f64,
        projected: f64,
    },
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
    /// Money-at-risk caps, in the same probability-space units as price
    /// (design doc review 3.9): a contract-count limit alone lets 500 @
    /// 2¢ and 500 @ 98¢ through identically, a 49x difference in actual
    /// money at risk.
    pub max_notional_per_order: f64,
    pub max_notional_per_venue: f64,
    pub max_notional_total: f64,
    /// How far an order's price may sit beyond the venue's own touch
    /// before it's refused outright, independent of size (design doc
    /// review 3.8).
    pub max_price_through_book: f64,
    /// How much realized-plus-unrealized loss this session may take,
    /// measured from `mark_to_market`'s first call, before the global
    /// kill switch auto-trips (design doc review 3.10/3.11).
    pub max_session_loss: f64,
    /// How far equity may fall from its session high-water mark before
    /// the same auto-trip fires — catches a slow bleed that never
    /// crosses `max_session_loss` from the *starting* equity.
    pub max_drawdown_from_peak: f64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        RiskLimits {
            max_abs_qty_per_contract: 500.0,
            max_abs_qty_per_cluster: 800.0,
            max_gross_qty_per_venue: 2_000.0,
            max_gross_qty_total: 3_000.0,
            max_feed_staleness_ns: 5_000_000_000, // 5s
            max_notional_per_order: 250.0,
            max_notional_per_venue: 1_000.0,
            max_notional_total: 1_500.0,
            max_price_through_book: 0.05,
            max_session_loss: 200.0,
            max_drawdown_from_peak: 150.0,
        }
    }
}

/// PARALLAX's own working (not-yet-filled) orders in one contract on one
/// venue, tracked separately for the buy and sell sides. A two-sided
/// ladder — 40 up, 40 down — is not zero risk just because the signed sum
/// is zero: in a fast market only one side fills, so both sides must be
/// counted against the limit independently (design doc review 1.2/1.3).
#[derive(Debug, Clone, Copy, Default)]
struct Reservation {
    buy_qty: f64,
    sell_qty: f64,
    buy_notional: f64,
    sell_notional: f64,
}

impl Reservation {
    /// The range this slot's position could realistically land in once
    /// every currently-working order (plus `extra_buy`/`extra_sell`, a
    /// hypothetical order being evaluated) resolves one way or the other.
    fn range(&self, filled_qty: f64, extra_buy: f64, extra_sell: f64) -> (f64, f64) {
        let low = filled_qty - self.sell_qty - extra_sell;
        let high = filled_qty + self.buy_qty + extra_buy;
        (low, high)
    }

    fn is_empty(&self) -> bool {
        self.buy_qty <= 0.0 && self.sell_qty <= 0.0
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
    reservations: ReservationMap,
    cluster_of: HashMap<CanonicalContractId, ClusterKey>,
    kill_switch: KillSwitch,
    reconciled: bool,
    session_start_equity: Option<f64>,
    peak_equity: f64,
}

impl RiskGate {
    /// Starts **unreconciled**: every order is refused until
    /// `mark_reconciled()` is called, because an empty position map after
    /// a restart means "unknown," not "flat" (design doc review 1.8). Use
    /// `new_presumed_flat` for the sim harness and tests, where flat
    /// really is the starting truth.
    pub fn new(limits: RiskLimits) -> Self {
        RiskGate {
            limits,
            positions: HashMap::new(),
            reservations: HashMap::new(),
            cluster_of: HashMap::new(),
            kill_switch: KillSwitch::new(),
            reconciled: false,
            session_start_equity: None,
            peak_equity: 0.0,
        }
    }

    /// For the backtest/sim harness and tests, where there is no real
    /// venue position to reconcile against and "flat" genuinely is the
    /// starting truth.
    pub fn new_presumed_flat(limits: RiskLimits) -> Self {
        let mut gate = Self::new(limits);
        gate.reconciled = true;
        gate
    }

    pub fn is_reconciled(&self) -> bool {
        self.reconciled
    }

    /// Marks the gate ready to trade. Call only after every open position
    /// and every working order has been fetched from each venue and
    /// loaded via `set_position`/`reserve`.
    pub fn mark_reconciled(&mut self) {
        self.reconciled = true;
    }

    /// Seeds one venue/contract slot's position from the venue's own
    /// report at startup. Rejects (and does not apply) a position that
    /// fails `Position::validate()` — a bad venue parse must not reach
    /// the book directly (design doc review 4.2).
    pub fn set_position(&mut self, position: Position) -> Result<(), ValidationError> {
        position.validate()?;
        self.positions
            .insert((position.venue, position.contract.clone()), position);
        Ok(())
    }

    pub fn kill_switch(&self) -> &KillSwitch {
        &self.kill_switch
    }

    /// Trips the global kill switch — every venue, every contract. Safe to
    /// call broadly: anything that detects a session-wide fault (a feed
    /// outage, an unexplained reconciliation mismatch) should be able to
    /// trip this without needing special authority.
    pub fn trip_global(&mut self, reason: impl Into<String>) {
        self.kill_switch.trip_global(reason);
    }

    pub fn trip_venue(&mut self, venue: VenueId, reason: impl Into<String>) {
        self.kill_switch.trip_venue(venue, reason);
    }

    pub fn trip_contract(&mut self, contract: CanonicalContractId, reason: impl Into<String>) {
        self.kill_switch.trip_contract(contract, reason);
    }

    /// Clears every tripped kill switch. Deliberately not named
    /// `reset` or exposed via a general mutable accessor: unlike tripping
    /// (which anything detecting a fault should be able to do), resetting
    /// is the one kill-switch operation that must never happen as a side
    /// effect of routine code — a switch that resets itself re-enters the
    /// condition that tripped it. This exists for an operator to call
    /// explicitly, after confirming out of band that whatever tripped it
    /// is actually resolved, and it is never called from anywhere in the
    /// trading path in this codebase.
    pub fn operator_reset_kill_switches(&mut self) {
        self.kill_switch.reset_all();
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

    /// This contract's net exposure per venue — filled position *plus*
    /// live reservations, netted (`+reserved_buy - reserved_sell`), so a
    /// strategy engine sees what it is actually on the hook for and stops
    /// re-proposing quotes it already has working (design doc review 1.2).
    pub fn inventory_for(&self, contract: &CanonicalContractId) -> HashMap<VenueId, f64> {
        let mut out: HashMap<VenueId, f64> = self
            .positions
            .iter()
            .filter(|((_, c), _)| c == contract)
            .map(|((v, _), p)| (*v, p.qty))
            .collect();
        for ((v, c), r) in self.reservations.iter() {
            if c != contract || r.is_empty() {
                continue;
            }
            *out.entry(*v).or_insert(0.0) += r.buy_qty - r.sell_qty;
        }
        out
    }

    /// Read-only snapshot of every position, for reporting/PnL markout —
    /// not consumed by any trading-path logic.
    pub fn positions_snapshot(&self) -> Vec<(VenueId, CanonicalContractId, Position)> {
        self.positions
            .iter()
            .map(|((v, c), p)| (*v, c.clone(), p.clone()))
            .collect()
    }

    /// Applies one signed fill against a position, charging `fee`, and
    /// returns the P&L this fill realized.
    pub fn record_fill(
        &mut self,
        venue: VenueId,
        contract: &CanonicalContractId,
        signed_qty: f64,
        price: f64,
        fee: f64,
    ) -> f64 {
        let entry = self
            .positions
            .entry((venue, contract.clone()))
            .or_insert_with(|| Position::flat(venue, contract.clone()));
        entry.apply_fill(signed_qty, price, fee)
    }

    /// Reserves an intent's full size against the exposure book. Call
    /// after the intent has cleared `check`/`check_batch` and before it
    /// is sent to the venue — the reservation, not the eventual fill, is
    /// what the risk gate must see immediately, since a resting order is
    /// real exposure the instant it is live (design doc review 1.2).
    pub fn reserve(&mut self, intent: &OrderIntent) {
        let key = (intent.venue, intent.contract.clone());
        let r = self.reservations.entry(key).or_default();
        match intent.side {
            Side::Buy => {
                r.buy_qty += intent.size;
                r.buy_notional += intent.risk_notional();
            }
            Side::Sell => {
                r.sell_qty += intent.size;
                r.sell_notional += intent.risk_notional();
            }
        }
    }

    /// Releases the *entire* remaining reservation for `intent` — call on
    /// a terminal ack (fully filled, rejected, or canceled).
    pub fn release(&mut self, intent: &OrderIntent) {
        self.reduce_reservation(intent, f64::INFINITY);
    }

    /// Releases up to `qty` of `intent`'s reservation — call on a partial
    /// fill, where only part of the working order's remaining exposure
    /// should come off the book. Whether an ack should trigger this at
    /// all depends on `leaves_order_working`: an IOC's unfilled remainder
    /// is canceled by the venue, not left resting, so it must be released
    /// in full even though the ack reads as "partially filled" (design
    /// doc review 4.3).
    pub fn reduce_reservation(&mut self, intent: &OrderIntent, qty: f64) {
        let Some(r) = self
            .reservations
            .get_mut(&(intent.venue, intent.contract.clone()))
        else {
            return;
        };
        let qty = qty.max(0.0);
        let notional_per_unit = if intent.size > 0.0 {
            intent.risk_notional() / intent.size
        } else {
            0.0
        };
        match intent.side {
            Side::Buy => {
                let reduce_qty = qty.min(r.buy_qty);
                r.buy_qty -= reduce_qty;
                r.buy_notional = (r.buy_notional - notional_per_unit * reduce_qty).max(0.0);
            }
            Side::Sell => {
                let reduce_qty = qty.min(r.sell_qty);
                r.sell_qty -= reduce_qty;
                r.sell_notional = (r.sell_notional - notional_per_unit * reduce_qty).max(0.0);
            }
        }
    }

    /// Whether an order's remaining size is still live at the venue after
    /// `status`, and so should stay reserved. A limit order's unfilled
    /// remainder rests; an IOC's does not — the venue cancels it
    /// immediately, so treating a `PartiallyFilled` IOC as "still working"
    /// bleeds reserved budget for the rest of the session (design doc
    /// review 4.3).
    pub fn leaves_order_working(
        order_type: parallax_types::OrderType,
        status: &parallax_types::AckStatus,
    ) -> bool {
        use parallax_types::{AckStatus, OrderType};
        match status {
            AckStatus::Accepted => true,
            AckStatus::PartiallyFilled { .. } => matches!(order_type, OrderType::Limit),
            AckStatus::Filled { .. } | AckStatus::Rejected { .. } | AckStatus::Canceled => false,
        }
    }

    /// Feeds current equity into the loss-budget/drawdown auto-trip. The
    /// *first* call establishes the session's starting equity and peak;
    /// every call after that measures loss-from-start and
    /// drawdown-from-peak against `RiskLimits`. A non-finite `equity` (a
    /// NaN average price propagating through, design doc review 4.2)
    /// trips the switch immediately rather than silently comparing false
    /// against every budget.
    pub fn mark_to_market(&mut self, equity: f64) {
        if !equity.is_finite() {
            self.kill_switch
                .trip_global("mark_to_market received non-finite equity");
            return;
        }
        match self.session_start_equity {
            None => {
                self.session_start_equity = Some(equity);
                self.peak_equity = equity;
            }
            Some(start) => {
                self.peak_equity = self.peak_equity.max(equity);
                let session_loss = start - equity;
                if session_loss > self.limits.max_session_loss {
                    self.kill_switch.trip_global(format!(
                        "session loss {session_loss:.4} exceeds max_session_loss {:.4}",
                        self.limits.max_session_loss
                    ));
                }
                let drawdown = self.peak_equity - equity;
                if drawdown > self.limits.max_drawdown_from_peak {
                    self.kill_switch.trip_global(format!(
                        "drawdown {drawdown:.4} from peak exceeds max_drawdown_from_peak {:.4}",
                        self.limits.max_drawdown_from_peak
                    ));
                }
            }
        }
    }

    fn signed_delta(intent: &OrderIntent) -> (f64, f64) {
        match intent.side {
            Side::Buy => (intent.size, 0.0),
            Side::Sell => (0.0, intent.size),
        }
    }

    fn reservation_at(
        &self,
        reservations: &ReservationMap,
        key: &(VenueId, CanonicalContractId),
    ) -> Reservation {
        reservations.get(key).copied().unwrap_or_default()
    }

    /// Every slot with a known position or reservation, plus `target` —
    /// the (venue, contract) pair under evaluation must be scanned even
    /// when it is a brand-new contract with no prior position or
    /// reservation at all, otherwise its hypothetical `extra_buy`/
    /// `extra_sell` would never be added to any aggregate in the first
    /// place.
    fn all_slot_keys(
        &self,
        positions: &PositionMap,
        reservations: &ReservationMap,
        target: (VenueId, &CanonicalContractId),
    ) -> BTreeSet<(VenueId, CanonicalContractId)> {
        let mut keys: BTreeSet<(VenueId, CanonicalContractId)> =
            positions.keys().cloned().collect();
        keys.extend(reservations.keys().cloned());
        keys.insert((target.0, target.1.clone()));
        keys
    }

    /// Single-slot worst case: `max(high, -low)` of the realizable range
    /// (design doc review 1.3).
    fn slot_worst_case(
        &self,
        positions: &PositionMap,
        reservations: &ReservationMap,
        venue: VenueId,
        contract: &CanonicalContractId,
        extra_buy: f64,
        extra_sell: f64,
    ) -> f64 {
        let filled = Self::qty_in(positions, venue, contract);
        let res = self.reservation_at(reservations, &(venue, contract.clone()));
        let (low, high) = res.range(filled, extra_buy, extra_sell);
        high.max(-low)
    }

    /// Sum of every slot's own worst case within `venue` (or, if `venue`
    /// is `None`, across all venues) — the target `(venue, contract)`
    /// slot gets the hypothetical `extra_buy`/`extra_sell` on top of its
    /// real state; every other slot uses its real state alone.
    fn gross_worst_case(
        &self,
        positions: &PositionMap,
        reservations: &ReservationMap,
        venue_filter: Option<VenueId>,
        target: (VenueId, &CanonicalContractId),
        extra_buy: f64,
        extra_sell: f64,
    ) -> f64 {
        let mut total = 0.0;
        for (v, c) in self.all_slot_keys(positions, reservations, target) {
            if let Some(vf) = venue_filter {
                if v != vf {
                    continue;
                }
            }
            let (eb, es) = if v == target.0 && &c == target.1 {
                (extra_buy, extra_sell)
            } else {
                (0.0, 0.0)
            };
            total += self.slot_worst_case(positions, reservations, v, &c, eb, es);
        }
        total
    }

    /// The cluster's own aggregate `[lowest, highest]` range, summed
    /// across every slot's *range* — not the sum of each slot's own
    /// worst-case magnitude, which is a different and smaller (and
    /// therefore unsafe) number: a slot whose buy leg is individually
    /// larger contributes a positive magnitude that can cancel exposure
    /// the true realizable worst case adds once every slot resolves
    /// independently (design doc review 4.1). `LessThan` contracts flip
    /// (negate and swap) their contribution before summing, so a long
    /// "temp > T" and a long "temp < T" in the same cluster net against
    /// each other rather than compounding as if they were the same bet;
    /// `Between` does not flip and does not net against anything —
    /// grossly conservative rather than wrong (design doc review 3.24).
    fn cluster_range(
        &self,
        positions: &PositionMap,
        reservations: &ReservationMap,
        cluster: &ClusterKey,
        target: (VenueId, &CanonicalContractId),
        extra_buy: f64,
        extra_sell: f64,
    ) -> (f64, f64) {
        let mut low_sum = 0.0;
        let mut high_sum = 0.0;
        for (v, c) in self.all_slot_keys(positions, reservations, target) {
            if self.cluster_of(&c) != *cluster {
                continue;
            }
            let (eb, es) = if v == target.0 && &c == target.1 {
                (extra_buy, extra_sell)
            } else {
                (0.0, 0.0)
            };
            let filled = Self::qty_in(positions, v, &c);
            let res = self.reservation_at(reservations, &(v, c.clone()));
            let (low, high) = res.range(filled, eb, es);
            let flips = c
                .direction()
                .map(|d| d.exposure_sign() < 0.0)
                .unwrap_or(false);
            if flips {
                low_sum += -high;
                high_sum += -low;
            } else {
                low_sum += low;
                high_sum += high;
            }
        }
        (low_sum, high_sum)
    }

    fn position_risk_notional(qty: f64, avg_price: f64) -> f64 {
        if qty >= 0.0 {
            qty * avg_price
        } else {
            -qty * (1.0 - avg_price)
        }
    }

    /// Total notional at risk within `venue` (or, if `None`, across every
    /// venue): filled positions valued at their own average price, plus
    /// every live reservation's notional at the price it was reserved at.
    fn notional_total(
        &self,
        positions: &PositionMap,
        reservations: &ReservationMap,
        venue_filter: Option<VenueId>,
    ) -> f64 {
        let mut total = 0.0;
        for ((v, _), p) in positions.iter() {
            if venue_filter.is_some_and(|vf| *v != vf) {
                continue;
            }
            total += Self::position_risk_notional(p.qty, p.avg_price);
        }
        for ((v, _), r) in reservations.iter() {
            if venue_filter.is_some_and(|vf| *v != vf) {
                continue;
            }
            total += r.buy_notional + r.sell_notional;
        }
        total
    }

    #[allow(clippy::too_many_arguments)]
    fn check_against(
        &self,
        positions: &PositionMap,
        reservations: &ReservationMap,
        intent: &OrderIntent,
        book: &ConsolidatedBook,
        now: Timestamp,
    ) -> Result<(), RejectReason> {
        if !self.reconciled {
            return Err(RejectReason::NotReconciled);
        }

        intent.validate().map_err(RejectReason::Invalid)?;

        if let Some(reason) = self
            .kill_switch
            .reason_if_tripped(intent.venue, &intent.contract)
        {
            return Err(RejectReason::KillSwitch { reason });
        }

        let fresh_tick = book
            .quotes(&intent.contract)
            .find(|t| t.venue == intent.venue);
        let tick = match fresh_tick {
            None => return Err(RejectReason::NoMarketData),
            Some(tick) => {
                let age = now.since(tick.receive_ts);
                // A negative age means the feed's timestamp is in the
                // future relative to `now` — clock skew, not freshness.
                // Left unchecked this passes every `age > max` staleness
                // test unconditionally (design doc review 3.7).
                if age < 0 {
                    return Err(RejectReason::ClockSkew { skew_ns: -age });
                }
                if age > self.limits.max_feed_staleness_ns {
                    return Err(RejectReason::FeedStale {
                        age_ns: age,
                        max_ns: self.limits.max_feed_staleness_ns,
                    });
                }
                tick
            }
        };

        let touch = match intent.side {
            Side::Buy => tick.ask,
            Side::Sell => tick.bid,
        };
        let through = (intent.price - touch).abs();
        // Only the direction that makes the order *more* aggressive than
        // the touch is a runaway-model risk; a price that already crosses
        // conservatively (a marketable limit at or better than the touch)
        // is not what this collar guards against.
        let is_aggressive_direction = match intent.side {
            Side::Buy => intent.price > touch,
            Side::Sell => intent.price < touch,
        };
        if is_aggressive_direction && through > self.limits.max_price_through_book {
            return Err(RejectReason::PriceThroughBook {
                touch,
                price: intent.price,
                max_through: self.limits.max_price_through_book,
            });
        }

        if intent.risk_notional() > self.limits.max_notional_per_order {
            return Err(RejectReason::NotionalPerOrderExceeded {
                limit: self.limits.max_notional_per_order,
                projected: intent.risk_notional(),
            });
        }

        let (extra_buy, extra_sell) = Self::signed_delta(intent);
        let target = (intent.venue, &intent.contract);

        // Every count/notional axis below rejects only when the proposed
        // order would make things worse (`projected > limit`) *and* the
        // axis wasn't already over the limit before this order
        // (`projected > current`) — an order that reduces an
        // already-over-limit exposure must have a way through, since
        // limits exist to bound exposure, not trap it (design doc review
        // 4.5).

        let contract_current = self.slot_worst_case(
            positions,
            reservations,
            intent.venue,
            &intent.contract,
            0.0,
            0.0,
        );
        let contract_projected = self.slot_worst_case(
            positions,
            reservations,
            intent.venue,
            &intent.contract,
            extra_buy,
            extra_sell,
        );
        if contract_projected > self.limits.max_abs_qty_per_contract
            && contract_projected > contract_current
        {
            return Err(RejectReason::ContractLimitExceeded {
                limit: self.limits.max_abs_qty_per_contract,
                projected: contract_projected,
            });
        }

        let cluster = self.cluster_of(&intent.contract);
        let (cur_low, cur_high) =
            self.cluster_range(positions, reservations, &cluster, target, 0.0, 0.0);
        let cluster_current = cur_high.max(-cur_low);
        let (proj_low, proj_high) = self.cluster_range(
            positions,
            reservations,
            &cluster,
            target,
            extra_buy,
            extra_sell,
        );
        let cluster_projected = proj_high.max(-proj_low);
        if cluster_projected > self.limits.max_abs_qty_per_cluster
            && cluster_projected > cluster_current
        {
            return Err(RejectReason::ClusterLimitExceeded {
                limit: self.limits.max_abs_qty_per_cluster,
                projected: cluster_projected,
            });
        }

        let venue_current = self.gross_worst_case(
            positions,
            reservations,
            Some(intent.venue),
            target,
            0.0,
            0.0,
        );
        let venue_projected = self.gross_worst_case(
            positions,
            reservations,
            Some(intent.venue),
            target,
            extra_buy,
            extra_sell,
        );
        if venue_projected > self.limits.max_gross_qty_per_venue && venue_projected > venue_current
        {
            return Err(RejectReason::VenueLimitExceeded {
                limit: self.limits.max_gross_qty_per_venue,
                projected: venue_projected,
            });
        }

        let total_current = self.gross_worst_case(positions, reservations, None, target, 0.0, 0.0);
        let total_projected =
            self.gross_worst_case(positions, reservations, None, target, extra_buy, extra_sell);
        if total_projected > self.limits.max_gross_qty_total && total_projected > total_current {
            return Err(RejectReason::GrossLimitExceeded {
                limit: self.limits.max_gross_qty_total,
                projected: total_projected,
            });
        }

        let venue_notional_current =
            self.notional_total(positions, reservations, Some(intent.venue));
        let venue_notional_projected = venue_notional_current + intent.risk_notional();
        if venue_notional_projected > self.limits.max_notional_per_venue
            && venue_notional_projected > venue_notional_current
        {
            return Err(RejectReason::NotionalVenueExceeded {
                limit: self.limits.max_notional_per_venue,
                projected: venue_notional_projected,
            });
        }

        let total_notional_current = self.notional_total(positions, reservations, None);
        let total_notional_projected = total_notional_current + intent.risk_notional();
        if total_notional_projected > self.limits.max_notional_total
            && total_notional_projected > total_notional_current
        {
            return Err(RejectReason::NotionalTotalExceeded {
                limit: self.limits.max_notional_total,
                projected: total_notional_projected,
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
        self.check_against(&self.positions, &self.reservations, intent, book, now)
    }

    /// Checks a batch of intents — typically one tick's proposals from
    /// market making, stat-arb, and sniping together — against a shared
    /// scratch copy of the position *and reservation* book, applying each
    /// accepted intent's reservation before checking the next. This is
    /// what stops three engines that each individually clear a limit from
    /// collectively blowing through it in the same tick (design doc
    /// §8/§10). The scratch copy is discarded after the batch; real state
    /// only moves via `reserve`/`record_fill` once the caller actually
    /// submits.
    pub fn check_batch(
        &self,
        intents: &[OrderIntent],
        book: &ConsolidatedBook,
        now: Timestamp,
    ) -> Vec<Result<(), RejectReason>> {
        let mut positions_scratch = self.positions.clone();
        let mut reservations_scratch = self.reservations.clone();
        let mut results = Vec::with_capacity(intents.len());
        for intent in intents {
            let result =
                self.check_against(&positions_scratch, &reservations_scratch, intent, book, now);
            if result.is_ok() {
                let key = (intent.venue, intent.contract.clone());
                let r = reservations_scratch.entry(key.clone()).or_default();
                match intent.side {
                    Side::Buy => {
                        r.buy_qty += intent.size;
                        r.buy_notional += intent.risk_notional();
                    }
                    Side::Sell => {
                        r.sell_qty += intent.size;
                        r.sell_notional += intent.risk_notional();
                    }
                }
                positions_scratch
                    .entry(key)
                    .or_insert_with(|| Position::flat(intent.venue, intent.contract.clone()));
            }
            results.push(result);
        }
        results
    }
}
