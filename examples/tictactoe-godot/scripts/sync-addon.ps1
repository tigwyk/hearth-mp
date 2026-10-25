# Copies the single-source Hearth addon into this example project.
# godot/addons/hearth is the source of truth; this example only has a copy so it
# can be opened as a standalone Godot project. Re-run after changing the addon.
$ErrorActionPreference = "Stop"
$source = Join-Path $PSScriptRoot "..\..\..\godot\addons\hearth"
$target = Join-Path $PSScriptRoot "..\addons\hearth"
if (-not (Test-Path $source)) {
    throw "source addon not found: $source"
}
Remove-Item -Recurse -Force $target -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Split-Path $target) | Out-Null
Copy-Item -Recurse -Force $source $target
Write-Host "synced $source -> $target"
