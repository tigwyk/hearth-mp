//! Hearth server runtime: the [`Room`] and [`Service`] traits, [`Ctx`], the
//! [`Host`] abstraction and (with the `worker` feature) the Durable Object
//! adapter.
//!
//! Object behaviour is pure Rust behind [`Host`], so every runtime test runs
//! natively in `hearth-test`.

#![warn(missing_docs)]

pub mod admin;
pub mod allocator;
pub mod call;
/// The handler context rooms and services receive.
pub mod ctx;
pub mod fetcher;
pub mod handoff;
/// The host abstraction: what a runtime needs from its environment.
pub mod host;
mod outbox;
/// The `Room` trait and its configuration.
pub mod room;
/// The room runtime that drives a `Room` over a `Host`.
pub mod runtime;
pub mod schema;
pub mod service;
mod store;

#[cfg(feature = "worker")]
pub mod container;

#[cfg(feature = "worker")]
pub mod worker;

pub use admin::{Cause, InspectClient, InspectDoc, TailMsg};
pub use allocator::{
    validate_endpoint, verify_webhook, Allocator, ContainerAllocator, ContainerStarter,
    StartRequest, WebhookAllocator, DEFAULT_CONTAINER_PORT,
};
pub use call::{CallError, Callable, Envelope, Target};
#[cfg(feature = "worker")]
pub use container::{ContainerObject, ContainerRpc};
pub use ctx::Ctx;
pub use fetcher::{CannedFetcher, Fetcher, HttpReq, HttpResp};
pub use handoff::{
    AllocJob, HandoffError, HandoffEvent, HandoffRequest, Redeemed, ServerReport, SessionId,
    SessionStatus,
};
pub use hearth_protocol::PlayerId;
pub use host::{Attachment, Host, RoomName, RoomReport, Session, Sql, SqlValue, ANON_PROVIDER};
pub use room::{Client, JoinOpts, Leave, Mode, Room, RoomConfig, RoomRpc, TimerKey};
pub use runtime::RoomRuntime;
pub use schema::{schema_doc, schema_hash, schema_hash_hex, SchemaDoc, HEARTH_SCHEMA_VERSION};
pub use service::{PlayerService, Service, ServiceCtx, ServiceRuntime};

/// Not part of the public API (`spec/stability.md`).
#[doc(hidden)]
pub mod __private {
    /// Soft seal for [`crate::Host`]: only the Hearth crates implement it (the
    /// Worker adapter, the test harness, the conformance server).
    pub trait Sealed {}
}

/// The result type of room and service handlers.
pub type Result<T> = anyhow::Result<T>;
