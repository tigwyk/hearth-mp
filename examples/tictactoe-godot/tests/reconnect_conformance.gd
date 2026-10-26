extends SceneTree
## Reconnect policy conformance test (plan v0.2a Task 3 Step 1).
##
## Every case in the Rust fixture must decide identically through
## `hearth_reconnect.gd`. Run:
##
##   godot --headless --path examples/tictactoe-godot -s tests/reconnect_conformance.gd
##
## The fixture path is relative to the project directory (`../../conformance/vectors/...`),
## so this test needs no server and no network.

const Reconnect = preload("res://addons/hearth/hearth_reconnect.gd")
const FIXTURE := "../../conformance/vectors/reconnect.json"


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

	print("reconnect_conformance: %d cases passed" % passed)
	quit(0)


func _run_case(case: Dictionary) -> String:
	var actual: int = Reconnect.decide(
		int(case.get("code", 0)),
		int(case.get("first_drop_ms", 0)),
		int(case.get("attempt", 0)),
		int(case.get("now_ms", 0)),
	)
	var expect = case.get("expect")
	if typeof(expect) == TYPE_STRING:
		# JSON has no integers; every number arrives as a float.
		if String(expect) != "give_up":
			return "bad fixture expectation %s" % expect
		if actual != -1:
			return "expected give_up, got retry at %d" % actual
	elif typeof(expect) == TYPE_FLOAT or typeof(expect) == TYPE_INT:
		if actual != int(expect):
			return "expected retry at %d, got %d" % [int(expect), actual]
	else:
		return "unexpected expectation %s" % str(expect)
	return ""


func _fail(reason: String) -> void:
	printerr("reconnect_conformance FAILED: " + reason)
	quit(1)
