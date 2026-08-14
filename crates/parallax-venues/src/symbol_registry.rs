use parallax_types::{CanonicalContractId, Outcome, VenueId};
use std::collections::HashMap;
use std::sync::RwLock;

/// Maps a canonical contract (plus which outcome token/side is being
/// traded) to the symbol a specific venue actually lists it under.
/// Populated at subscribe time, once a venue's native listing has been
/// resolved into a `CanonicalContractSpec` (design doc review 1.6):
/// nothing upstream of this ever sends a canonical id as if it were a
/// venue symbol, and nothing here maps a canonical id *back* to a listing
/// until that mapping has actually been observed. `RwLock`, not `Mutex`:
/// this is read on every order submission and written only at subscribe
/// time, so reads should never contend with each other.
#[derive(Default)]
pub struct SymbolRegistry {
    listings: RwLock<HashMap<(VenueId, CanonicalContractId, Outcome), String>>,
}

impl SymbolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        venue: VenueId,
        contract: CanonicalContractId,
        outcome: Outcome,
        symbol: impl Into<String>,
    ) {
        self.listings
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert((venue, contract, outcome), symbol.into());
    }

    pub fn lookup(
        &self,
        venue: VenueId,
        contract: &CanonicalContractId,
        outcome: Outcome,
    ) -> Option<String> {
        self.listings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(venue, contract.clone(), outcome))
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.listings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> CanonicalContractId {
        CanonicalContractId("wx.temp.chicago.gt_869.2026-08-12.nws_official".into())
    }

    #[test]
    fn an_unregistered_contract_has_no_mapping() {
        let registry = SymbolRegistry::new();
        assert!(registry
            .lookup(VenueId::Kalshi, &contract(), Outcome::Yes)
            .is_none());
    }

    #[test]
    fn registration_is_per_venue_and_per_outcome() {
        let registry = SymbolRegistry::new();
        registry.register(
            VenueId::Kalshi,
            contract(),
            Outcome::Yes,
            "KXHIGHCHI-26AUG12-B87",
        );
        assert_eq!(
            registry.lookup(VenueId::Kalshi, &contract(), Outcome::Yes),
            Some("KXHIGHCHI-26AUG12-B87".to_string())
        );
        // Different outcome, same contract: not registered.
        assert!(registry
            .lookup(VenueId::Kalshi, &contract(), Outcome::No)
            .is_none());
        // Different venue, same contract/outcome: not registered.
        assert!(registry
            .lookup(VenueId::Polymarket, &contract(), Outcome::Yes)
            .is_none());
    }
}
