# hearth-mp-godot

Hearth v0.1 client for Godot 4 as a GDExtension (`HearthNative`) plus the
`addons/hearth` autoload that hides the backend choice.

## Build and install

The prebuilt bindings mean building needs no Godot binary:

```powershell
cargo build -p hearth-mp-godot --release
# Copy the cdylib where the .gdextension expects it (bin/ is gitignored):
Copy-Item target/release/hearth_mp_godot.dll godot/addons/hearth/bin/windows/
```

Library paths per platform (also listed in `godot/addons/hearth/hearth.gdextension`):

| Platform | File |
| --- | --- |
| Windows x86_64 | `godot/addons/hearth/bin/windows/hearth_mp_godot.dll` |
| Linux x86_64 | `godot/addons/hearth/bin/linux/libhearth_mp_godot.so` |
| macOS | `godot/addons/hearth/bin/macos/libhearth_mp_godot.dylib` |

The extension is built against the Godot 4.3 API (`api-4-3` feature) so it loads
on Godot >= 4.3; the `hearth.gdextension` declares
`compatibility_minimum = "4.3"`.

A CLI-only project must be imported once so Godot records the extension:

```powershell
godot --headless --import --path examples/tictactoe-godot
```

## Tests

`cargo test -p hearth-mp-godot` runs the engine-free JSON <-> Variant tests. The
`Variant` side of that bridge needs a running Godot, so the tests drive the same
tree walk through a plain Rust representation (see `src/convert.rs`).

The end-to-end smoke test lives in `examples/tictactoe-godot/tests/smoke.gd` and
needs the Task 9 server (`wrangler dev`).
