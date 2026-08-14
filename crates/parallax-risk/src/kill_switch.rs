use parallax_types::{CanonicalContractId, Timestamp, VenueId};
use std::collections::HashMap;

/// A single trip event: what tripped and when, so an operator paging in
/// off a kill switch has more to go on than "something is halted" (design
/// doc review 3.11).
#[derive(Debug, Clone, PartialEq)]
pub struct Trip {
    pub scope: TripScope,
    pub reason: String,
    pub tripped_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TripScope {
    Global,
    Venue(VenueId),
    Contract(CanonicalContractId),
}

/// Independent halt flags at three scopes. A global trip halts everything;
/// a venue trip halts only that venue (e.g. an error-rate spike on one
/// venue shouldn't stop quoting on a healthy one); a contract trip halts
/// just the instrument a staleness or model-sanity check flagged. Any
/// scope being tripped is sufficient to reject (design doc §10).
#[derive(Default)]
pub struct KillSwitch {
    global: Option<Trip>,
    per_venue: HashMap<VenueId, Trip>,
    per_contract: HashMap<CanonicalContractId, Trip>,
}

impl KillSwitch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trip_global(&mut self, reason: impl Into<String>) {
        self.global = Some(Trip {
            scope: TripScope::Global,
            reason: reason.into(),
            tripped_at: Timestamp::now(),
        });
    }

    pub fn trip_venue(&mut self, venue: VenueId, reason: impl Into<String>) {
        self.per_venue.insert(
            venue,
            Trip {
                scope: TripScope::Venue(venue),
                reason: reason.into(),
                tripped_at: Timestamp::now(),
            },
        );
    }

    pub fn trip_contract(&mut self, contract: CanonicalContractId, reason: impl Into<String>) {
        self.per_contract.insert(
            contract.clone(),
            Trip {
                scope: TripScope::Contract(contract),
                reason: reason.into(),
                tripped_at: Timestamp::now(),
            },
        );
    }

    /// Deliberately not called from anywhere in the trading path: a
    /// switch that resets itself re-enters the condition that tripped it.
    /// This exists for an operator to call explicitly, after confirming
    /// whatever tripped it is actually resolved.
    pub fn reset_all(&mut self) {
        self.global = None;
        self.per_venue.clear();
        self.per_contract.clear();
    }

    pub fn is_global_tripped(&self) -> bool {
        self.global.is_some()
    }

    pub fn reason_if_tripped(
        &self,
        venue: VenueId,
        contract: &CanonicalContractId,
    ) -> Option<String> {
        self.global
            .as_ref()
            .or_else(|| self.per_venue.get(&venue))
            .or_else(|| self.per_contract.get(contract))
            .map(|t| t.reason.clone())
    }

    /// Every currently-active trip, across all three scopes — the
    /// operator-facing view an alerting/ops layer needs to exist.
    pub fn active_trips(&self) -> Vec<Trip> {
        let mut trips: Vec<Trip> = Vec::new();
        trips.extend(self.global.clone());
        trips.extend(self.per_venue.values().cloned());
        trips.extend(self.per_contract.values().cloned());
        trips
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_trips_reports_every_scope_that_is_tripped() {
        let mut ks = KillSwitch::new();
        assert!(ks.active_trips().is_empty());

        ks.trip_venue(VenueId::Kalshi, "error rate");
        ks.trip_contract(CanonicalContractId("x".into()), "stale");
        assert_eq!(ks.active_trips().len(), 2);

        ks.trip_global("kill everything");
        assert_eq!(ks.active_trips().len(), 3);
    }

    #[test]
    fn reset_all_clears_every_scope() {
        let mut ks = KillSwitch::new();
        ks.trip_global("x");
        ks.trip_venue(VenueId::Kalshi, "y");
        ks.reset_all();
        assert!(ks.active_trips().is_empty());
        assert!(!ks.is_global_tripped());
    }
}
