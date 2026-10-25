# Hearth tic-tac-toe (Godot client)

Godot 4 client for the Hearth sample room in `../../examples/tictactoe`. Uses the
`Hearth` autoload: the native GDExtension on desktop, the pure-GDScript JSON
client on web exports.

## Run

```powershell
# 1. Sync the addon copy (godot/addons/hearth is the source of truth).
./scripts/sync-addon.ps1

# 2. Build and install the GDExtension (native backend only).
cargo build -p hearth-godot --release
Copy-Item ../../target/release/hearth_godot.dll ../../godot/addons/hearth/bin/windows/
./scripts/sync-addon.ps1

# 3. Start the sample server (examples/tictactoe/wrangler.toml) in another shell.
cd ../tictactoe; npx wrangler dev

# 4. Import once so Godot records the .gdextension, then open the project.
godot --headless --import --path .
godot --path .
```

Create a room in one window, then join it with a second window.

## Tests

```powershell
# Patch conformance against the Rust fixture: no server needed.
godot --headless --path . -s tests/patch_conformance.gd

# End-to-end smoke test: needs the sample server and its schema hash.
$env:HEARTH_URL = "http://127.0.0.1:8787"
$env:HEARTH_SCHEMA = (Invoke-RestMethod $env:HEARTH_URL/schema/tictactoe).schema_hash
godot --headless --path . -s tests/smoke.gd
# Same test through the pure-GDScript backend:
$env:HEARTH_FORCE_WEB = "1"; godot --headless --path . -s tests/smoke.gd
```
