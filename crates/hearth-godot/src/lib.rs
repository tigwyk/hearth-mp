//! Hearth v0.1 Godot 4 client: a GDExtension node that drives
//! [`hearth_client::ClientCore`] over Godot's own `WebSocketPeer`.
//!
//! * [`HearthNative`] is the class Godot instantiates on desktop builds (via
//!   `ClassDB.instantiate("HearthNative")` in `hearth_client.gd`).
//! * [`convert`] is the JSON <-> `Variant` bridge, split so the tree walk is
//!   testable with `cargo test` on a machine without Godot.
//!
//! Build with `cargo build -p hearth-godot --release` and copy the library into
//! `godot/addons/hearth/bin/` (see this crate's README).

pub mod convert;
mod native;

pub use native::HearthNative;

use godot::prelude::{gdextension, ExtensionLibrary};

struct HearthGodotExtension;

#[gdextension]
unsafe impl ExtensionLibrary for HearthGodotExtension {}
