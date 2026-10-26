# Hearth tic-tac-toe (Godot client)

Godot 4 client for the Hearth sample room in `../../examples/tictactoe`. Uses the
`Hearth` autoload: the native GDExtension on desktop, the pure-GDScript JSON
client on web exports. The typed state classes under `hearth/` are generated from
the sample server crate, so the client's schema hash can never drift.

## Run

```powershell
# 1. Sync the addon copy (godot/addons/hearth is the source of truth).
./scripts/sync-addon.ps1

# 2. Build and install the GDExtension (native backend only).
cargo build -p hearth-mp-godot --release
Copy-Item ../../target/release/hearth_mp_godot.dll ../../godot/addons/hearth/bin/windows/
./scripts/sync-addon.ps1

# 3. Regenerate the typed state classes after every change to
#    examples/tictactoe/src/lib.rs (from the repo root). The committed `hearth/`
#    output is what the project and its tests use.
cargo run -p hearth-mp-cli -- codegen --godot examples/tictactoe-godot/hearth --manifest-path examples/tictactoe/Cargo.toml

# 4. Start the sample server (examples/tictactoe/wrangler.toml) in another shell.
cd ../tictactoe; npx wrangler dev

# 5. Import once so Godot records the .gdextension and the generated class
#    names, then open the project.
godot --headless --import --path .
godot --path .
```

Create a room in one window, then join it with a second window.

## Tests

```powershell
# Typed mirror against the committed generated classes: no server needed.
godot --headless --path . -s tests/typed_runtime.gd

# Object-in-map typed-runtime coverage (hand-written fixture, no codegen needed).
godot --headless --path . -s tests/typed_object_path.gd

# Patch and reconnect conformance against the Rust fixtures: no server needed.
godot --headless --path . -s tests/patch_conformance.gd
godot --headless --path . -s tests/reconnect_conformance.gd

# End-to-end smoke test: needs the sample server. The schema hash comes from the
# generated TicTacToeBoard, so only the URL is passed in.
$env:HEARTH_URL = "http://127.0.0.1:8787"
godot --headless --path . -s tests/smoke.gd
# Same test through the pure-GDScript backend:
$env:HEARTH_FORCE_WEB = "1"; godot --headless --path . -s tests/smoke.gd
```
