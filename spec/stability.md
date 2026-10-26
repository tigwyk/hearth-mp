# Stability policy

From 1.0, Hearth follows [Semantic Versioning](https://semver.org/). This page
says what that promise covers and how it is enforced.

## Covered by semver

A breaking change to any of these needs a new **major** version:

- **The Rust public APIs** of the crates with a snapshot in `api/`
  (`api/<crate>.txt`).
- **Protocol v1** (`spec/protocol-v1.md`) and the conformance vectors in
  `conformance/vectors/`.
- **SchemaDoc v1** (`spec/schema-v1.md`).
- **The C ABI**: the symbols and struct layouts in `api/hearth_unity.h`.
- **The engine SDK surfaces**: the C# public surface in `api/unity-csharp.txt`
  and the GDScript public surface in `api/godot-gdscript.txt`.

Within a major version, the protocol and schema format change only additively
(`spec/protocol-v1.md` section 14, `spec/schema-v1.md` section 7). Enums and
config structs that are expected to grow are `#[non_exhaustive]`, so adding a
variant or a field is a minor change: match them with a wildcard arm and build
config structs with their constructors or `Default`.

## Not covered

- Items marked `#[doc(hidden)]`, including `__private` modules, macro support
  types and the `ffi::__impl_*` functions.
- Implementing `hearth_server::Host` outside the Hearth crates. `Host` is
  soft-sealed through `hearth_server::__private::Sealed`.
- Internal HTTP paths under `/__hearth/` and the admin routes.
- SQL table layouts inside a room's storage. They change only through
  migrations that the runtime applies itself.
- Log line formats and the text of error messages.
- The Wasm bundle size. `bundle-budget.toml` tracks it as a metric, and
  `scripts/check-stability.sh` fails when a target goes over its budget, but a
  release may raise a budget (see `docs/limits.md`, "Bundle size").

## MSRV

The minimum supported Rust version is `rust-version` in the workspace
`Cargo.toml`. It is raised only in minor releases, and at most to the stable
release two versions behind the newest (stable − 2).

## Deprecation

An item is deprecated with `#[deprecated]` (or the engine's equivalent) for at
least one minor release before it is removed in the next major release.

## Enforcement

`scripts/check-stability.sh` fails on any change to the snapshots: it runs
`cargo public-api` per crate, `cargo semver-checks` against the last release tag,
`cbindgen --verify` on the C header, and the C# and GDScript snapshot tests. An
intended change is accepted with `scripts/check-stability.sh --bless`, and the
snapshot diff is then part of the review.
