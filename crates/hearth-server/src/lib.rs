//! Hearth server runtime: the [`Room`] and [`Service`] traits, [`Ctx`], the
//! [`Host`] abstraction and (with the `worker` feature) the Durable Object
//! adapter.
//!
//! Object behaviour is pure Rust behind [`Host`], so every runtime test runs
//! natively in `hearth-test`.

pub mod admin;
pub mod allocator;
pub mod call;
pub mod ctx;
pub mod fetcher;
pub mod handoff;
pub mod host;
mod outbox;
pub mod room;
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

pub type Result<T> = anyhow::Result<T>;
