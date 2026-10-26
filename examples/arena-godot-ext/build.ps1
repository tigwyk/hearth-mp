# Builds the arena GDExtension and installs it in the sample project.
#
# `examples/arena-godot-ext` is a cdylib that links the stock `hearth-mp-godot`
# classes (including their `gdext_rust_init` entry point) and adds the generated
# `ArenaPredictor` class. Copying it over the synced addon library keeps
# `hearth.gdextension` unchanged: same entry symbol, same class set, plus the
# predictor.
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Push-Location $root
try {
    & (Join-Path $root "examples\arena-godot\scripts\sync-addon.ps1")
    cargo build -p arena-godot-ext --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build -p arena-godot-ext --release failed"
    }
    $source = Join-Path $root "target\release\arena_godot.dll"
    $destination = Join-Path $root "examples\arena-godot\addons\hearth\bin\windows\hearth_mp_godot.dll"
    New-Item -ItemType Directory -Force (Split-Path $destination) | Out-Null
    Copy-Item $source $destination -Force
    Write-Host "installed arena GDExtension at $destination"
    Write-Host "run: godot --path examples/arena-godot  (HEARTH_URL=http://127.0.0.1:8787)"
}
finally {
    Pop-Location
}
