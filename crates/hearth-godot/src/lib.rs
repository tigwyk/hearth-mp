//! Hearth Godot 4 client: a GDExtension exposing the native classes and the
//! JSON <-> `Variant` bridge.
//!
//! * [`HearthNative`] is the class Godot instantiates on desktop builds (via
//!   `ClassDB.instantiate("HearthNative")` in `hearth_client.gd`).
//! * [`HearthInterp`] backs `Hearth.interpolator(paths)` on desktop builds: a
//!   snapshot interpolator for ticked rooms, fed automatically from every
//!   ticked `PATCH`.
//! * [`convert`] is the JSON <-> `Variant` bridge, split so the tree walk is
//!   testable with `cargo test` on a machine without Godot.
//!
//! Build with `cargo build -p hearth-godot --release` and copy the library into
//! `godot/addons/hearth/bin/` (see this crate's README).

#![warn(missing_docs)]

pub mod convert;
// gdext's generated `#[var]` getters carry no docs.
#[allow(missing_docs)]
mod native;

pub use native::HearthNative;

/// Re-exported so [`predictor!`] expansions do not need an explicit
/// `hearth-client` dependency in the extension crate.
#[doc(hidden)]
pub use hearth_client;
/// Re-exported so [`predictor!`] expansions do not need an explicit
/// `hearth-protocol` dependency in the extension crate.
#[doc(hidden)]
pub use hearth_protocol;
/// Re-exported so [`predictor!`] expansions do not need `serde_json` in the
/// extension crate.
#[doc(hidden)]
pub use serde_json;

/// Tick rate assumed by prediction and interpolation (the ticked-mode ceiling).
pub const PREDICT_TICK_HZ: u8 = 20;

use godot::classes::{IRefCounted, RefCounted, Time};
use godot::prelude::*;
use hearth_client::{ClockSync, Interpolator};
use serde_json::Value;

use crate::convert::{json_to_variant, variant_to_json};

/// Tick rate assumed by [`HearthInterp`].
///
/// The wire protocol does not carry the room's `tick_hz`; the ticked-mode
/// ceiling (and every v0.4 sample room) is 20 Hz. At lower rates interpolation
/// still works, only the two-period render delay is smaller than ideal.
const INTERP_TICK_HZ: u8 = 20;

/// Render delay: two tick periods, so there is always a newer snapshot.
const INTERP_DELAY_MS: u64 = 2 * (1_000 / INTERP_TICK_HZ as u64);

/// Snapshot interpolator for ticked rooms; returned by `Hearth.interpolator`.
///
/// The autoload feeds it from every ticked `PATCH`; game code calls
/// [`HearthInterp::sample`] or [`HearthInterp::sample_v2`] once per frame.
#[derive(GodotClass)]
#[class(base = RefCounted)]
pub struct HearthInterp {
    base: Base<RefCounted>,
    clock: ClockSync,
    interp: Interpolator,
}

#[godot_api]
impl IRefCounted for HearthInterp {
    fn init(base: Base<RefCounted>) -> Self {
        Self {
            base,
            clock: ClockSync::new(),
            interp: Interpolator::new(INTERP_TICK_HZ),
        }
    }
}

#[godot_api]
impl HearthInterp {
    /// Interpolated value of `path` at the current render time. A number comes
    /// back as a `float`, a two-number array as a `Vector2`, and other values
    /// in their wire shape; `null` when the path has no value yet.
    #[func]
    fn sample(&self, path: VarArray) -> Variant {
        match self.sample_value(&path) {
            Some(value) => interpolated_to_variant(&value),
            None => Variant::nil(),
        }
    }

    /// Like [`HearthInterp::sample`], but always a `Vector2` (`Vector2.ZERO`
    /// when the path is missing or its value is not two numbers).
    #[func]
    fn sample_v2(&self, path: VarArray) -> Vector2 {
        match self.sample_value(&path) {
            Some(Value::Array(items)) if items.len() == 2 => {
                match (items[0].as_f64(), items[1].as_f64()) {
                    (Some(x), Some(y)) => Vector2::new(x as real, y as real),
                    _ => Vector2::ZERO,
                }
            }
            _ => Vector2::ZERO,
        }
    }
}

impl HearthInterp {
    /// Track a path; `*` segments match every key or list index at that level.
    pub(crate) fn track(&mut self, path: Vec<Value>) {
        self.interp.track(path);
    }

    /// Feed one ticked snapshot (called by `HearthNative` for every `PATCH`).
    pub(crate) fn on_patch(&mut self, state: &Value, server_t: u64) {
        self.clock
            .observe(server_t, Time::singleton().get_ticks_msec());
        self.interp.on_patch(state, server_t);
    }

    /// Drop snapshots and clock samples after a `FULL_STATE` replaced the
    /// state; the tracked paths survive.
    pub(crate) fn reset(&mut self) {
        self.clock = ClockSync::new();
        self.interp.clear();
    }

    /// Interpolate the tracked `path` at `server_now - two periods`.
    fn sample_value(&self, path: &VarArray) -> Option<Value> {
        let Ok(Value::Array(path)) = variant_to_json(&path.to_variant()) else {
            return None;
        };
        let server_now = self.clock.server_now(Time::singleton().get_ticks_msec())?;
        let render_t = server_now.saturating_sub(INTERP_DELAY_MS);
        self.interp.sample(&path, render_t)
    }
}

/// Convert an interpolated value to the public Godot shape: a number becomes a
/// `float`, a two-number array a `Vector2`, containers are converted
/// recursively.
fn interpolated_to_variant(value: &Value) -> Variant {
    if let Value::Array(items) = value {
        if items.len() == 2 {
            if let (Some(x), Some(y)) = (items[0].as_f64(), items[1].as_f64()) {
                return Vector2::new(x as real, y as real).to_variant();
            }
        }
    }
    interpolated_float_tree(value)
}

/// Recursively convert numbers to floats; other values keep their wire shape.
fn interpolated_float_tree(value: &Value) -> Variant {
    match value {
        Value::Number(number) => Variant::from(number.as_f64().unwrap_or(0.0)),
        Value::Array(items) => {
            let mut array = VarArray::new();
            for item in items {
                array.push(&interpolated_float_tree(item));
            }
            array.to_variant()
        }
        Value::Object(object) => {
            let mut dictionary = VarDictionary::new();
            for (key, item) in object {
                dictionary.set(key.as_str(), &interpolated_float_tree(item));
            }
            dictionary.to_variant()
        }
        other => json_to_variant(other),
    }
}

/// Generate a Godot predictor class for a [`hearth_client::Logic`]
/// implementation.
///
/// `$class` is the generated `RefCounted` class name and `$logic` is the type
/// implementing the trait. The class exposes:
///
/// - `input(seq, input)`: record one input and step the prediction immediately;
/// - `predicted_state() -> Dictionary`: the locally predicted wire state;
/// - `attach(hearth_node)`: wire it to a `Hearth`-style node with the `joined`,
///   `state_reset`, `patched`, `acked` and `intents_unconfirmed` signals.
///
/// The extension crate links the stock `hearth-godot` classes (their
/// `gdext_rust_init` entry point included) and calls the macro; do **not** add a
/// second `#[gdextension]` impl, that defines a duplicate `gdext_rust_init`
/// symbol. See `examples/arena-godot-ext`.
///
/// ```ignore
/// extern crate hearth_godot;
///
/// hearth_godot::predictor!(ArenaPredictor, arena_logic::Arena);
/// ```
#[macro_export]
macro_rules! predictor {
    ($class:ident, $logic:ty) => {
        #[derive(::godot::prelude::GodotClass)]
        #[class(base = RefCounted)]
        pub struct $class {
            base: ::godot::obj::Base<::godot::classes::RefCounted>,
            predictor: ::std::option::Option<$crate::hearth_client::Predictor<$logic>>,
            attached: ::std::option::Option<::godot::obj::Gd<::godot::classes::Node>>,
            /// The newest authoritative state as wire JSON.
            authoritative: $crate::serde_json::Value,
        }

        #[::godot::prelude::godot_api]
        impl ::godot::classes::IRefCounted for $class {
            fn init(base: ::godot::obj::Base<::godot::classes::RefCounted>) -> Self {
                Self {
                    base,
                    predictor: ::std::option::Option::None,
                    attached: ::std::option::Option::None,
                    authoritative: $crate::serde_json::Value::Null,
                }
            }
        }

        #[::godot::prelude::godot_api]
        impl $class {
            /// Wire the predictor to a `Hearth`-style node (the autoload or a
            /// client instance) and predict from here on.
            #[func]
            fn attach(&mut self, hearth_node: ::godot::obj::Gd<::godot::classes::Node>) {
                let mut hearth_node = hearth_node;
                let gd_self = ::godot::obj::WithBaseField::to_gd(self);
                let connections = [
                    ("joined", "_hearth_predictor_joined"),
                    ("state_reset", "_hearth_predictor_state_reset"),
                    ("patched", "_hearth_predictor_patched"),
                    ("acked", "_hearth_predictor_acked"),
                    ("intents_unconfirmed", "_hearth_predictor_unconfirmed"),
                ];
                for (signal, method) in connections {
                    let callable = ::godot::builtin::Callable::from_object_method(&gd_self, method);
                    let signal_name = ::godot::builtin::StringName::from(signal);
                    hearth_node.connect(&signal_name, &callable);
                }
                self.attached = ::std::option::Option::Some(hearth_node);
                if let ::std::option::Option::Some(me) = self.player_id() {
                    self.predictor = ::std::option::Option::Some(
                        $crate::hearth_client::Predictor::<$logic>::new(
                            me,
                            $crate::PREDICT_TICK_HZ,
                        ),
                    );
                    self.refresh_authoritative();
                    self.reconcile(&[]);
                }
            }

            /// Record input `seq` and step the prediction immediately.
            #[func]
            fn input(&mut self, seq: i64, input: ::godot::builtin::Variant) {
                let ::std::option::Option::Some(predictor) = self.predictor.as_mut() else {
                    return;
                };
                let ::std::result::Result::Ok(json) = $crate::convert::variant_to_json(&input)
                else {
                    return;
                };
                let ::std::result::Result::Ok(input) = $crate::serde_json::from_value::<
                    <$logic as $crate::hearth_client::Logic>::Input,
                >(json) else {
                    return;
                };
                let ::std::result::Result::Ok(seq) =
                    <u32 as ::std::convert::TryFrom<i64>>::try_from(seq)
                else {
                    return;
                };
                predictor.input(seq, input);
            }

            /// The locally predicted state, as the same Dictionary shape the
            /// wire state uses.
            #[func]
            fn predicted_state(&self) -> ::godot::builtin::VarDictionary {
                let ::std::option::Option::Some(predictor) = self.predictor.as_ref() else {
                    return ::godot::builtin::VarDictionary::new();
                };
                let ::std::result::Result::Ok(json) =
                    $crate::serde_json::to_value(predictor.predicted())
                else {
                    return ::godot::builtin::VarDictionary::new();
                };
                $crate::convert::json_to_variant(&json)
                    .try_to::<::godot::builtin::VarDictionary>()
                    .unwrap_or_else(|_| ::godot::builtin::VarDictionary::new())
            }

            #[func]
            fn _hearth_predictor_joined(
                &mut self,
                player_id: ::godot::builtin::GString,
                _seat: i32,
            ) {
                self.predictor =
                    ::std::option::Option::Some($crate::hearth_client::Predictor::<$logic>::new(
                        $crate::hearth_protocol::PlayerId(player_id.to_string()),
                        $crate::PREDICT_TICK_HZ,
                    ));
                self.refresh_authoritative();
                self.reconcile(&[]);
            }

            #[func]
            fn _hearth_predictor_state_reset(&mut self) {
                self.refresh_authoritative();
                self.reconcile(&[]);
            }

            #[func]
            fn _hearth_predictor_patched(&mut self, _ops: ::godot::builtin::VarArray) {
                self.refresh_authoritative();
            }

            #[func]
            fn _hearth_predictor_acked(
                &mut self,
                _state_seq: i64,
                intent_seqs: ::godot::builtin::VarArray,
            ) {
                let mut acked = ::std::vec::Vec::new();
                for index in 0..intent_seqs.len() {
                    if let ::std::result::Result::Ok(seq) = intent_seqs.at(index).try_to::<i64>() {
                        acked.push(seq as u32);
                    }
                }
                self.reconcile(&acked);
            }

            #[func]
            fn _hearth_predictor_unconfirmed(&mut self, seqs: ::godot::builtin::VarArray) {
                let ::std::option::Option::Some(predictor) = self.predictor.as_mut() else {
                    return;
                };
                let mut unconfirmed = ::std::vec::Vec::new();
                for index in 0..seqs.len() {
                    if let ::std::result::Result::Ok(seq) = seqs.at(index).try_to::<i64>() {
                        unconfirmed.push(seq as u32);
                    }
                }
                predictor.on_unconfirmed(&unconfirmed);
            }
        }

        impl $class {
            /// The node's joined player id, when it has one.
            fn player_id(&self) -> ::std::option::Option<$crate::hearth_protocol::PlayerId> {
                let node = self.attached.as_ref()?;
                let name = node
                    .get("player_id")
                    .try_to::<::godot::builtin::GString>()
                    .ok()?
                    .to_string();
                if name.is_empty() {
                    ::std::option::Option::None
                } else {
                    ::std::option::Option::Some($crate::hearth_protocol::PlayerId(name))
                }
            }

            /// Copy the client's current state view into `authoritative`.
            fn refresh_authoritative(&mut self) {
                let ::std::option::Option::Some(node) = self.attached.as_ref() else {
                    return;
                };
                for property in ["raw_state", "state"] {
                    let value = node.get(property);
                    if let ::std::result::Result::Ok(json) =
                        $crate::convert::variant_to_json(&value)
                    {
                        if !json.is_null() {
                            self.authoritative = json;
                            return;
                        }
                    }
                }
            }

            /// Rebuild the prediction from the newest authoritative state and
            /// the inputs the server has not acknowledged.
            fn reconcile(&mut self, acked: &[u32]) {
                let ::std::option::Option::Some(predictor) = self.predictor.as_mut() else {
                    return;
                };
                if self.authoritative.is_null() {
                    return;
                }
                predictor.on_authoritative(&self.authoritative, acked);
            }
        }
    };
}

struct HearthGodotExtension;

#[gdextension]
unsafe impl ExtensionLibrary for HearthGodotExtension {}
