extends Node2D
## Minimal project shell for the v0.5 arena-dedicated handoff sample.
##
## The real clients live in the tests: two Hearth clients ready up, receive the
## `hearth.handoff` event and connect to the container server. Run them headless:
##
##   godot --headless --path . -s tests/handoff_smoke.gd


func _ready() -> void:
	print("Hearth v0.5 arena-dedicated client project")
	print("Handoff smoke test: godot --headless --path . -s tests/handoff_smoke.gd")
	print("Server helper test: pwsh -File ../tests/run-server-helper.ps1")
