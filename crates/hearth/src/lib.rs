//! Hearth: a Rust framework for Durable Object game servers.
//!
//! This facade re-exports everything a room crate needs, so the only
//! dependencies are `hearth`, `serde` and `worker`. A room is a struct
//! implementing [`Room`] with a `#[sync]` state, configured with
//! `#[hearth::room(...)]`:
//!
//! ```ignore
//! use hearth::{
//!     anyhow, Client, Ctx, HearthState, JoinOpts, Leave, PlayerId, Result, Room, SyncMap,
//! };
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(HearthState, Serialize, Deserialize, Default)]
//! #[hearth(crate = "hearth")] // follows the derive: it is a derive helper attribute
//! struct Match {
//!     #[sync] players: SyncMap<PlayerId, u8>,
//!     #[sync(owner)] hand: SyncMap<PlayerId, Vec<u8>>,
//! }
//!
//! #[derive(Deserialize)]
//! enum Intent { Move { cell: u8 } }
//!
//! #[derive(Default)]
//! struct CardMatch;
//!
//! #[hearth::room(mode = "event", max_clients = 2, binding = "CARD_MATCH")]
//! impl Room for CardMatch {
//!     type State = Match;
//!     type Intent = Intent;
//!
//!     async fn on_intent(
//!         &mut self,
//!         ctx: &mut Ctx<Self>,
//!         _who: &Client,
//!         intent: Intent,
//!     ) -> Result<()> {
//!         let Intent::Move { cell } = intent;
//!         if cell > 8 {
//!             anyhow::bail!("cell out of range");
//!         }
//!         Ok(())
//!     }
//!     // on_join / on_leave are required; on_timer / on_dispose have defaults.
//! }
//! ```
//!
//! [`anyhow`] is re-exported (and is the error type of [`Result`]) so room code
//! can `anyhow::bail!` / `anyhow::ensure!` without adding its own dependency.

pub use anyhow;
pub use hearth_admin as admin;
pub use hearth_admin::registry_object;
pub use hearth_matchmaker as matchmaker;
pub use hearth_matchmaker::matchmaker_object;
pub use hearth_protocol as protocol;
pub use hearth_protocol::PlayerId;
pub use hearth_server as server;
pub use hearth_server::container_object;
pub use hearth_server::{
    schema_doc, schema_hash, schema_hash_hex, CallError, Callable, Client, Ctx, Envelope, JoinOpts,
    Leave, PlayerService, Result, Room, RoomRpc, SchemaDoc, Service, ServiceCtx, Target, TimerKey,
};
pub use hearth_server_macros::{room, service};
pub use hearth_sync as sync;
pub use hearth_sync::{HearthState, Schema, SyncMap};
