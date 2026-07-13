//! Field-level validators for client-supplied numeric request parameters.
//!
//! The REST DTOs speak `f64` for JSON ergonomics, but the domain works in
//! `Positive` / `Decimal`. These helpers perform the fallible `f64 -> typed`
//! conversion at the DTO boundary, rejecting non-finite (`NaN`/`±inf`), negative,
//! or out-of-domain values with a [`ChainError::Validation`] that names the
//! offending field — instead of panicking via `pos_or_panic!` or silently zeroing
//! bad input via `unwrap_or_default`.
//!
//! They are shared by every conversion that ingests raw request floats:
//! `TryFrom<CreateSessionRequest>`, `TryFrom<ApiWalkType>`, and the PATCH merge path.

use crate::api::rest::models::ApiTimeFrame;
use crate::utils::ChainError;
use optionstratlib::utils::TimeFrame;
use positive::Positive;
use rust_decimal::Decimal;

/// Rejects a non-finite (`NaN` or `±inf`) user-supplied float.
///
/// This is checked before any conversion so the error message can point at the
/// raw request field rather than surfacing an opaque downstream conversion error.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when `value` is not finite.
#[must_use = "the validated value must be used"]
pub(crate) fn finite_field(field: &str, value: f64) -> Result<f64, ChainError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ChainError::Validation {
            field: field.to_string(),
            reason: format!("must be a finite number, got {value}"),
        })
    }
}

/// Validates and converts a request float into a non-negative [`Positive`] (`>= 0`).
///
/// Rejects `NaN`/`±inf` and negative values.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when the value is non-finite or negative.
#[must_use = "the validated value must be used"]
pub(crate) fn positive_field(field: &str, value: f64) -> Result<Positive, ChainError> {
    let value = finite_field(field, value)?;
    Positive::new(value).map_err(|e| ChainError::Validation {
        field: field.to_string(),
        reason: e.to_string(),
    })
}

/// Validates and converts a request float into a strictly positive [`Positive`] (`> 0`).
///
/// Like [`positive_field`] but additionally rejects exactly zero — used for values
/// where zero is structurally invalid (e.g. a `dt` time step or a `strike_interval`).
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when the value is non-finite, negative, or zero.
#[must_use = "the validated value must be used"]
pub(crate) fn strictly_positive_field(field: &str, value: f64) -> Result<Positive, ChainError> {
    let positive = positive_field(field, value)?;
    if positive == Positive::ZERO {
        return Err(ChainError::Validation {
            field: field.to_string(),
            reason: format!("must be greater than zero, got {value}"),
        });
    }
    Ok(positive)
}

/// Validates and converts a request float into a [`Decimal`].
///
/// Rejects `NaN`/`±inf` and any value `Decimal` cannot represent, instead of the
/// previous `unwrap_or_default` that silently coerced bad input to zero.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when the value is non-finite or not
/// representable as a `Decimal`.
#[must_use = "the validated value must be used"]
pub(crate) fn decimal_field(field: &str, value: f64) -> Result<Decimal, ChainError> {
    let value = finite_field(field, value)?;
    Decimal::try_from(value).map_err(|e| ChainError::Validation {
        field: field.to_string(),
        reason: e.to_string(),
    })
}

/// Validates and converts a request float into a [`Decimal`] constrained to
/// `[min, max]` (inclusive) — used for bounded parameters such as an
/// autocorrelation coefficient in `[-1, 1]`.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when the value is non-finite, outside the
/// range, or not representable as a `Decimal`.
#[must_use = "the validated value must be used"]
pub(crate) fn bounded_decimal_field(
    field: &str,
    value: f64,
    min: f64,
    max: f64,
) -> Result<Decimal, ChainError> {
    let value = finite_field(field, value)?;
    if !(min..=max).contains(&value) {
        return Err(ChainError::Validation {
            field: field.to_string(),
            reason: format!("must be within [{min}, {max}], got {value}"),
        });
    }
    decimal_field(field, value)
}

/// Validates and converts a request [`ApiTimeFrame`] into a [`TimeFrame`].
///
/// The named variants convert infallibly; a `Custom(periods_per_year)` value is
/// user input and must be a finite, strictly positive number — the infallible
/// `From` impl would panic on a negative value.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when a `Custom` periods-per-year value is
/// non-finite, negative, or zero.
#[must_use = "the validated value must be used"]
pub(crate) fn time_frame_field(field: &str, value: ApiTimeFrame) -> Result<TimeFrame, ChainError> {
    if let ApiTimeFrame::Custom(periods) = value {
        let validated = strictly_positive_field(field, periods)?;
        return Ok(TimeFrame::Custom(validated));
    }
    Ok(value.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_frame_field_accepts_named_variants() {
        assert!(matches!(
            time_frame_field("time_frame", ApiTimeFrame::Day),
            Ok(TimeFrame::Day)
        ));
    }

    #[test]
    fn test_time_frame_field_accepts_valid_custom() {
        assert!(time_frame_field("time_frame", ApiTimeFrame::Custom(52.0)).is_ok());
    }

    #[test]
    fn test_time_frame_field_rejects_negative_custom() {
        match time_frame_field("time_frame", ApiTimeFrame::Custom(-1.0)) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "time_frame"),
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn test_time_frame_field_rejects_nan_custom() {
        assert!(matches!(
            time_frame_field("time_frame", ApiTimeFrame::Custom(f64::NAN)),
            Err(ChainError::Validation { .. })
        ));
    }

    #[test]
    fn test_finite_field_accepts_finite() {
        assert_eq!(finite_field("x", 1.5).expect("finite is accepted"), 1.5);
    }

    #[test]
    fn test_finite_field_rejects_nan() {
        let err = finite_field("volatility", f64::NAN);
        match err {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "volatility"),
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn test_finite_field_rejects_infinity() {
        assert!(matches!(
            finite_field("risk_free_rate", f64::INFINITY),
            Err(ChainError::Validation { .. })
        ));
    }

    #[test]
    fn test_positive_field_rejects_negative() {
        match positive_field("initial_price", -1.0) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "initial_price"),
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn test_positive_field_accepts_zero() {
        assert_eq!(
            positive_field("dividend_yield", 0.0).expect("zero is a valid Positive"),
            Positive::ZERO
        );
    }

    #[test]
    fn test_strictly_positive_field_rejects_zero() {
        match strictly_positive_field("strike_interval", 0.0) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "strike_interval");
                assert!(reason.contains("greater than zero"));
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn test_decimal_field_rejects_nan() {
        assert!(matches!(
            decimal_field("skew_slope", f64::NAN),
            Err(ChainError::Validation { .. })
        ));
    }

    #[test]
    fn test_bounded_decimal_field_rejects_out_of_range() {
        match bounded_decimal_field("autocorrelation", 1.5, -1.0, 1.0) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "autocorrelation"),
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn test_bounded_decimal_field_accepts_within_range() {
        assert!(bounded_decimal_field("autocorrelation", 0.5, -1.0, 1.0).is_ok());
    }
}
