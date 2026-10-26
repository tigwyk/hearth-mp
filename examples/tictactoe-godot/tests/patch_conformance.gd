extends SceneTree
## Patch conformance test (plan Task 11 Step 1).
##
## Every case in the Rust fixture must apply identically through
## `hearth_patch.gd`. Run:
##
##   godot --headless --path examples/tictactoe-godot -s tests/patch_conformance.gd
##
## The fixture path is relative to the project directory (`../../conformance/vectors/...`),
## so this test needs no server and no network.

const Patch = preload("res://addons/hearth/hearth_patch.gd")
const FIXTURE := "../../conformance/vectors/patch.json"


func _init() -> void:
	var path := ProjectSettings.globalize_path("res://").path_join(FIXTURE).simplify_path()
	var text := FileAccess.get_file_as_string(path)
	if text.is_empty():
		_fail("cannot read fixture at " + path)
		return
	var cases = JSON.parse_string(text)
	if typeof(cases) != TYPE_ARRAY:
		_fail("fixture is not a JSON array: " + path)
		return
	if cases.is_empty():
		_fail("fixture has no cases: " + path)
		return

	var passed := 0
	for case in cases:
		var name := String(case.get("name", "<unnamed>"))
		var error := _run_case(case)
		if not error.is_empty():
			_fail("%s: %s" % [name, error])
			return
		passed += 1

	print("patch_conformance: %d cases passed" % passed)
	quit(0)


func _run_case(case: Dictionary) -> String:
	var want_error := bool(case.get("error", false))
	var state = case.get("before")
	var failed := false
	for op in case.get("ops", []):
		var next = Patch.apply(state, op)
		if next == null:
			failed = true
			break
		state = next
	if want_error:
		return "" if failed else "expected a bad path"
	if failed:
		return "unexpected bad path"
	if state != case.get("after"):
		return "state %s != expected %s" % [JSON.stringify(state), JSON.stringify(case.get("after"))]
	return ""


func _fail(reason: String) -> void:
	printerr("patch_conformance FAILED: " + reason)
	quit(1)
