//! Hearth v0.4 arena GDExtension (prediction sample).
//!
//! The stock `hearth-mp-godot` classes and its `gdext_rust_init` entry point are
//! linked in as dependencies; this crate only adds the predictor class. Godot
//! loads this library in place of the stock addon library, and
//! `hearth.gdextension` keeps the same `gdext_rust_init` entry symbol, so
//! `HearthNative`/`HearthInterp` stay registered and `ArenaPredictor` joins
//! them.
//!
//! Build with `../arena-godot-ext/build.ps1`, which copies the result into the
//! sample project's synced addon.

// Link the stock extension: its classes register through godot-rust's
// inventory when the crate is linked, and it provides the extension entry
// point. No second `#[gdextension]` impl here: that would define a duplicate
// `gdext_rust_init` symbol.
extern crate hearth_mp_godot;

hearth_mp_godot::predictor!(ArenaPredictor, arena_logic::Arena);
