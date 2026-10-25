//! Hearth v0.1 state sync: `SyncMap`, whole-view diffing, schema hashing and
//! owner-scoped views.
//!
//! The runtime builds each client's view of a state type as a JSON tree, then
//! sends the difference between the view before and after each handler pass as
//! `PatchOp`s.

mod diff;
mod map;
mod schema;

pub use diff::diff;
pub use hearth_protocol::{PlayerId, Value};
pub use hearth_sync_derive::{HearthState, Schema};
pub use map::SyncMap;
pub use schema::{fnv1a64, mix, HearthState, OwnerScoped, Schema};

/// Implementation details used by generated code. Not public API.
#[doc(hidden)]
pub mod __private {
    pub use serde_json::{Map, Value};

    /// Serialise a `#[sync]` field for the per-client view. State trees are
    /// JSON data by protocol, so a failure here is a programming error.
    pub fn to_value<T: serde::Serialize>(value: &T) -> Value {
        serde_json::to_value(value).expect("Hearth state fields must serialise as JSON")
    }
}
