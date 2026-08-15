//! `docs/GOING-LIVE.md` Stage 3, "continuous fee-model verification":
//!
//! > Do not verify the fee schedule once at launch. Assert on every fill
//! > that the fee the venue charged matches what your model predicted,
//! > and halt on a persistent mismatch. Venues change fees; a stale fee
//! > model turns a profitable strategy unprofitable with no error
//! > anywhere, which is precisely why it needs an assertion rather than a
//! > quarterly review.
//!
//! `FeeVerifier` takes `(modeled, realized)` fee pairs from the caller
//! rather than reading a fee off `OrderAck` directly. That's deliberate:
//! neither this repo nor `docs/GOING-LIVE.md` have verified whether
//! Kalshi/Polymarket report a fill's actual fee in the same response as
//! the fill itself or via a separate statement/fills endpoint — baking a
//! wire-shape assumption into a core, widely-used type like `AckStatus`
//! would be exactly the kind of unverified-endpoint guess every query
//! method in `parallax-venues` is deliberately not allowed to make.
//! Whoever wires this in live supplies the realized fee from wherever
//! it's actually confirmed to come from.

/// One fee check's result. A single mismatch (`Mismatch`) is not
/// actionable on its own — floating-point rounding, a fee the model
/// slightly under/over-estimates by a cent, is normal. `PersistentMismatch`
/// is what the doc means by "halt": the same divergence held across
/// `halt_after` consecutive fills, which stops looking like noise and
/// starts looking like a stale fee schedule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FeeCheckOutcome {
    Match,
    Mismatch {
        modeled: f64,
        realized: f64,
        consecutive: u32,
    },
    PersistentMismatch {
        modeled: f64,
        realized: f64,
        consecutive: u32,
    },
}

/// Tracks consecutive fee mismatches per venue and reports when they've
/// gone on long enough to stop being noise.
pub struct FeeVerifier {
    /// Absolute difference below which a modeled/realized pair still
    /// counts as a match — floating-point fee arithmetic (rate * qty *
    /// price * (1-price), then rounded to the venue's own unit) will
    /// never land on the *exact* same f64 as an independently-computed
    /// comparison without some slack.
    tolerance: f64,
    /// How many consecutive mismatches before `record` reports
    /// `PersistentMismatch` instead of `Mismatch`.
    halt_after: u32,
    consecutive_mismatches: u32,
}

impl FeeVerifier {
    pub fn new(tolerance: f64, halt_after: u32) -> Self {
        FeeVerifier {
            tolerance: tolerance.max(0.0),
            halt_after: halt_after.max(1),
            consecutive_mismatches: 0,
        }
    }

    /// Compares one fill's modeled fee against its realized (venue
    /// reported, or otherwise independently confirmed) fee. A match
    /// resets the consecutive-mismatch counter — this tracks a *streak*,
    /// not a lifetime total, so a single transient blip doesn't
    /// permanently poison every check after it.
    pub fn record(&mut self, modeled: f64, realized: f64) -> FeeCheckOutcome {
        let diff = (modeled - realized).abs();
        if diff <= self.tolerance {
            self.consecutive_mismatches = 0;
            return FeeCheckOutcome::Match;
        }
        self.consecutive_mismatches += 1;
        if self.consecutive_mismatches >= self.halt_after {
            FeeCheckOutcome::PersistentMismatch {
                modeled,
                realized,
                consecutive: self.consecutive_mismatches,
            }
        } else {
            FeeCheckOutcome::Mismatch {
                modeled,
                realized,
                consecutive: self.consecutive_mismatches,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_fee_within_tolerance_is_a_match() {
        let mut verifier = FeeVerifier::new(0.0001, 3);
        assert_eq!(verifier.record(1.2300, 1.2301), FeeCheckOutcome::Match);
    }

    #[test]
    fn a_single_mismatch_is_reported_but_does_not_halt() {
        let mut verifier = FeeVerifier::new(0.0001, 3);
        match verifier.record(1.00, 1.50) {
            FeeCheckOutcome::Mismatch { consecutive, .. } => assert_eq!(consecutive, 1),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_persistent_mismatch_halts_after_the_configured_streak() {
        let mut verifier = FeeVerifier::new(0.0001, 3);
        assert!(matches!(
            verifier.record(1.00, 1.50),
            FeeCheckOutcome::Mismatch { consecutive: 1, .. }
        ));
        assert!(matches!(
            verifier.record(1.00, 1.50),
            FeeCheckOutcome::Mismatch { consecutive: 2, .. }
        ));
        assert!(matches!(
            verifier.record(1.00, 1.50),
            FeeCheckOutcome::PersistentMismatch { consecutive: 3, .. }
        ));
    }

    #[test]
    fn a_match_in_between_resets_the_streak() {
        let mut verifier = FeeVerifier::new(0.0001, 3);
        verifier.record(1.00, 1.50); // mismatch, streak 1
        verifier.record(1.00, 1.50); // mismatch, streak 2
        assert_eq!(verifier.record(1.00, 1.00), FeeCheckOutcome::Match); // resets
        match verifier.record(1.00, 1.50) {
            FeeCheckOutcome::Mismatch { consecutive, .. } => assert_eq!(consecutive, 1),
            other => panic!("expected a fresh streak of 1, got {other:?}"),
        }
    }

    #[test]
    fn zero_or_negative_configuration_does_not_panic_or_loop() {
        let mut verifier = FeeVerifier::new(-1.0, 0);
        // tolerance clamped to >= 0.0, halt_after clamped to >= 1.
        match verifier.record(1.00, 1.00) {
            FeeCheckOutcome::Match => {}
            other => panic!("exact match should still match with clamped tolerance, got {other:?}"),
        }
        match verifier.record(1.00, 1.01) {
            FeeCheckOutcome::PersistentMismatch { consecutive: 1, .. } => {}
            other => panic!("halt_after=1 should halt immediately, got {other:?}"),
        }
    }
}
