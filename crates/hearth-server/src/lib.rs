//! Hearth server runtime: the [`Room`] trait, [`Ctx`], the [`Host`] abstraction
//! and (with the `worker` feature) the Durable Object adapter.
//!
//! Room behaviour is pure Rust behind [`Host`], so every runtime test runs
//! natively in `hearth-test`.

pub mod ctx;
pub mod host;
pub mod room;
pub mod runtime;
pub mod schema;

#[cfg(feature = "worker")]
pub mod worker;

pub use ctx::Ctx;
pub use host::{Attachment, Host, Sql, SqlValue};
pub use room::{Client, JoinOpts, Leave, Mode, Room, RoomConfig, TimerKey};
pub use runtime::RoomRuntime;
pub use schema::{schema_doc, schema_hash, schema_hash_hex, SchemaDoc, HEARTH_SCHEMA_VERSION};

pub type Result<T> = anyhow::Result<T>;
