//! The v2 simulation snapshot tape: its contract, its records, and its rows.
//!
//! Three files, one concern each. [`interface`] is the storage-agnostic trait
//! the session layer depends on; [`record`] holds the typed records that cross
//! it; [`model`] is the serialization boundary where a `Positive` becomes a
//! `Decimal(38, 28)` column. The ClickHouse implementation lives beside the
//! other repositories, in
//! [`crate::infrastructure::ClickHouseSnapshotRepository`].

pub(crate) mod interface;
pub(crate) mod model;
pub(crate) mod record;
