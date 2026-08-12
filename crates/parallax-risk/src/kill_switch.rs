use parallax_types::{CanonicalContractId, VenueId};
use std::collections::HashMap;

/// Independent halt flags at three scopes. A global trip halts everything;
/// a venue trip halts only that venue (e.g. an error-rate spike on one
/// venue shouldn't stop quoting on a healthy one); a contract trip halts
/// just the instrument a staleness or model-sanity check flagged. Any
/// scope being tripped is sufficient to reject (design doc §10).
#[derive(Default)]
pub struct KillSwitch {
    global: Option<String>,
    per_venue: HashMap<VenueId, String>,
    per_contract: HashMap<CanonicalContractId, String>,
}

impl KillSwitch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trip_global(&mut self, reason: impl Into<String>) {
        self.global = Some(reason.into());
    }

    pub fn trip_venue(&mut self, venue: VenueId, reason: impl Into<String>) {
        self.per_venue.insert(venue, reason.into());
    }

    pub fn trip_contract(&mut self, contract: CanonicalContractId, reason: impl Into<String>) {
        self.per_contract.insert(contract, reason.into());
    }

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
            .clone()
            .or_else(|| self.per_venue.get(&venue).cloned())
            .or_else(|| self.per_contract.get(contract).cloned())
    }
}
