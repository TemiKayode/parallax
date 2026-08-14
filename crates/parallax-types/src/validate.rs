//! Shared validation primitives (design doc: every trust boundary must
//! reject a non-finite or out-of-range value before it can reach a
//! comparison it would silently pass — IEEE-754 defines every comparison
//! against NaN as false, so an unvalidated NaN clears every `>` check a
//! risk limit relies on). Every domain type that can arrive from an
//! external boundary (a venue parse, a replay file, an alpha payload, an
//! HTTP body) implements `validate()` using these helpers.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}

pub fn finite(field: &'static str, value: f64) -> Result<(), ValidationError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(ValidationError {
            field,
            message: format!("must be finite, got {value}"),
        })
    }
}

/// In `[0.0, 1.0]` — PARALLAX's universal price/probability convention.
pub fn probability(field: &'static str, value: f64) -> Result<(), ValidationError> {
    finite(field, value)?;
    if (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(ValidationError {
            field,
            message: format!("must be a probability in [0,1], got {value}"),
        })
    }
}

pub fn positive(field: &'static str, value: f64) -> Result<(), ValidationError> {
    finite(field, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(ValidationError {
            field,
            message: format!("must be positive, got {value}"),
        })
    }
}

pub fn non_negative(field: &'static str, value: f64) -> Result<(), ValidationError> {
    finite(field, value)?;
    if value >= 0.0 {
        Ok(())
    } else {
        Err(ValidationError {
            field,
            message: format!("must be non-negative, got {value}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_and_infinity_fail_every_helper() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(finite("x", bad).is_err());
            assert!(probability("x", bad).is_err());
            assert!(positive("x", bad).is_err());
            assert!(non_negative("x", bad).is_err());
        }
    }

    #[test]
    fn probability_rejects_out_of_range_finite_values() {
        assert!(probability("x", 1.5).is_err());
        assert!(probability("x", -0.1).is_err());
        assert!(probability("x", 0.0).is_ok());
        assert!(probability("x", 1.0).is_ok());
    }

    #[test]
    fn positive_rejects_zero() {
        assert!(positive("x", 0.0).is_err());
        assert!(positive("x", 1e-12).is_ok());
    }
}
