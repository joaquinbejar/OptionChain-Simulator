//! Tri-state PATCH field wrapper for partial-update request DTOs.
//!
//! A plain `Option<T>` cannot distinguish "field absent from the JSON body"
//! from "field present but explicitly `null`" — serde maps both to `None`. A
//! PATCH endpoint needs all three states:
//!
//! - **absent** → keep the current value unchanged;
//! - **null** → clear the value back to `None`;
//! - **value** → replace with the supplied value.
//!
//! [`Patch<T>`] carries that distinction:
//!
//! - Field *absence* is expressed with `#[serde(default)]` on the field, which
//!   deserializes to [`Patch::Absent`] (the `Default`).
//! - An explicit JSON `null` deserializes to [`Patch::Null`].
//! - Any other value deserializes to [`Patch::Value`].
//!
//! On serialize, [`Patch::Absent`] fields are omitted with
//! `#[serde(skip_serializing_if = "Patch::is_absent")]`, [`Patch::Null`]
//! serializes as `null`, and [`Patch::Value`] as the inner value.
//!
//! For the OpenAPI surface, fields typed `Patch<T>` are annotated
//! `#[schema(value_type = Option<T>)]` so the documented shape is the familiar
//! nullable value — the tri-state semantics are described in each field's doc.

use serde::de::{Deserialize, Deserializer};
use serde::ser::{Serialize, Serializer};

/// Tri-state wrapper distinguishing an absent JSON key, an explicit `null`, and
/// a present value in a partial-update (PATCH) request body.
///
/// See the [module documentation](self) for how each state maps to serde and to
/// the PATCH merge semantics (absent = keep, null = clear, value = replace).
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Patch<T> {
    /// The field was absent from the request body — keep the current value.
    #[default]
    Absent,
    /// The field was present and explicitly `null` — clear the value to `None`.
    Null,
    /// The field was present with a value — replace the current value.
    Value(T),
}

impl<T> Patch<T> {
    /// Returns `true` when the field was absent from the request body.
    ///
    /// Used as the `skip_serializing_if` predicate so absent fields are omitted
    /// from the serialized JSON.
    #[must_use]
    #[inline]
    pub fn is_absent(&self) -> bool {
        matches!(self, Patch::Absent)
    }
}

impl<'de, T> Deserialize<'de> for Patch<T>
where
    T: Deserialize<'de>,
{
    /// Deserializes a *present* key into [`Patch::Null`] (JSON `null`) or
    /// [`Patch::Value`] (any other value).
    ///
    /// Field absence is handled upstream by `#[serde(default)]` on the field,
    /// which yields [`Patch::Absent`]; this impl is only reached when the key is
    /// present, so it never produces `Absent`.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(|opt| match opt {
            None => Patch::Null,
            Some(value) => Patch::Value(value),
        })
    }
}

impl<T> Serialize for Patch<T>
where
    T: Serialize,
{
    /// Serializes [`Patch::Value`] as the inner value and both [`Patch::Null`]
    /// and [`Patch::Absent`] as JSON `null`.
    ///
    /// [`Patch::Absent`] is expected to be filtered out by
    /// `#[serde(skip_serializing_if = "Patch::is_absent")]`; if it is reached
    /// anyway, `null` is the closest neutral encoding.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Patch::Absent | Patch::Null => serializer.serialize_none(),
            Patch::Value(value) => serializer.serialize_some(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Holder {
        #[serde(default, skip_serializing_if = "Patch::is_absent")]
        value: Patch<f64>,
    }

    #[test]
    fn test_patch_deserialize_absent_key_is_absent() {
        let holder: Holder = serde_json::from_str("{}").expect("empty object deserializes");
        assert_eq!(holder.value, Patch::Absent);
    }

    #[test]
    fn test_patch_deserialize_explicit_null_is_null() {
        let holder: Holder =
            serde_json::from_value(json!({ "value": null })).expect("null deserializes");
        assert_eq!(holder.value, Patch::Null);
    }

    #[test]
    fn test_patch_deserialize_value_is_value() {
        let holder: Holder =
            serde_json::from_value(json!({ "value": 1.5 })).expect("value deserializes");
        assert_eq!(holder.value, Patch::Value(1.5));
    }

    #[test]
    fn test_patch_serialize_absent_is_omitted() {
        let holder = Holder {
            value: Patch::Absent,
        };
        let json = serde_json::to_value(&holder).expect("serializes");
        assert_eq!(json, json!({}));
    }

    #[test]
    fn test_patch_serialize_null_is_null() {
        let holder = Holder { value: Patch::Null };
        let json = serde_json::to_value(&holder).expect("serializes");
        assert_eq!(json, json!({ "value": null }));
    }

    #[test]
    fn test_patch_serialize_value_is_value() {
        let holder = Holder {
            value: Patch::Value(2.5),
        };
        let json = serde_json::to_value(&holder).expect("serializes");
        assert_eq!(json, json!({ "value": 2.5 }));
    }

    #[test]
    fn test_patch_default_is_absent() {
        assert_eq!(Patch::<u64>::default(), Patch::Absent);
        assert!(Patch::<u64>::default().is_absent());
    }

    #[test]
    fn test_patch_is_absent_only_for_absent() {
        assert!(Patch::<u64>::Absent.is_absent());
        assert!(!Patch::<u64>::Null.is_absent());
        assert!(!Patch::Value(1u64).is_absent());
    }
}
