//! Room-level schema hash and the machine-readable [`SchemaDoc`] exported for
//! codegen.
//!
//! The hash covers both the state and the intent type, so a change to either
//! makes stale clients fail `HELLO` loudly instead of desyncing.

use std::collections::BTreeMap;

use hearth_sync::{mix, Registry, Schema, TypeDef, TypeRef};
use serde::{Deserialize, Serialize};

use crate::room::Room;

/// Version of the `SchemaDoc` format.
pub const HEARTH_SCHEMA_VERSION: u32 = 1;

/// One room's state and intent schema, ready for JSON export.
#[derive(Serialize, Deserialize)]
pub struct SchemaDoc {
    pub hearth_schema: u32,
    pub room: String,
    pub hash: String,
    pub state: String,
    pub intent: String,
    pub types: BTreeMap<String, TypeDef>,
}

/// The room's schema hash: the state hash mixed with the intent hash.
pub fn schema_hash<R: Room>() -> u64 {
    mix(<R::State as Schema>::HASH, <R::Intent as Schema>::HASH)
}

/// [`schema_hash`] as the 16-char lowercase hex exchanged in `HELLO`.
pub fn schema_hash_hex<R: Room>() -> String {
    format!("{:016x}", schema_hash::<R>())
}

/// Describe `R`'s state and intent types, including every named type they reach.
pub fn schema_doc<R: Room>(room_name: &str) -> SchemaDoc {
    let mut registry = Registry::default();
    let state = <R::State as Schema>::describe(&mut registry);
    let intent = <R::Intent as Schema>::describe(&mut registry);
    SchemaDoc {
        hearth_schema: HEARTH_SCHEMA_VERSION,
        room: room_name.to_owned(),
        hash: schema_hash_hex::<R>(),
        state: type_label(&state),
        intent: type_label(&intent),
        types: registry
            .types
            .into_iter()
            .map(|(name, (_hash, def))| (name, def))
            .collect(),
    }
}

/// The Rust type name for a named type; primitives keep their TypeRef spelling.
fn type_label(ty: &TypeRef) -> String {
    match ty {
        TypeRef::Named(name) => name.clone(),
        other => serde_json::to_string(other).expect("a TypeRef always serialises as JSON"),
    }
}
