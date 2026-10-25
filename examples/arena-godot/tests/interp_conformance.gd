extends SceneTree
## Snapshot-interpolation conformance (v0.4 Task 6 Step 1).
##
## Replays every case in `crates/hearth-client/tests/fixtures/interp_cases.json`
## through the pure-GDScript port `hearth_interp.gd` with the clock injected, so
## this test needs no GDExtension, no server and no network:
##
##   godot --headless --path examples/arena-godot -s tests/interp_conformance.gd
##
## Fixture case fields: `name`, `tick_hz`, `snapshots` ([server_t, value] pairs),
## `render_t`, `expect` (value or null). Optional `track`/`sample` paths default
## to `["value"]`; `track` may contain `"*"` wildcards.

const Interp = preload("res://addons/hearth/hearth_interp.gd")
const FIXTURE := "../../crates/hearth-client/tests/fixtures/interp_cases.json"
const DEFAULT_PATH := ["value"]


func _init() -> void:
	var fixture_path := ProjectSettings.globalize_path("res://").path_join(FIXTURE).simplify_path()
	var text := FileAccess.get_file_as_string(fixture_path)
	if text.is_empty():
		_fail("cannot read fixture at " + fixture_path)
		return
	var cases = JSON.parse_string(text)
	if typeof(cases) != TYPE_ARRAY:
		_fail("fixture is not a JSON array: " + fixture_path)
		return

	var passed := 0
	for case in cases:
		var name := String(case.get("name", "<unnamed>"))
		var error := _run_case(case)
		if not error.is_empty():
			_fail("%s: %s" % [name, error])
			return
		print("ok - " + name)
		passed += 1

	print("interp_conformance: %d cases passed" % passed)
	quit(0)


func _run_case(case: Dictionary) -> String:
	var interp = Interp.new(int(case.get("tick_hz", 20)))
	var track: Array = case.get("track", DEFAULT_PATH)
	var sample_path: Array = case.get("sample", track)
	interp.track(track)
	for pair in case.get("snapshots", []):
		interp._on_patch({"value": pair[1]}, int(pair[0]))

	# sample() renders two periods behind the synced server clock; inject the
	# clock so it renders exactly at the fixture's render_t.
	interp.now_ms = int(case["render_t"]) + 2 * interp.period_ms()
	var actual = interp.sample(sample_path)
	var expected = case.get("expect")
	if expected == null:
		if actual != null:
			return "expected null, got %s" % str(actual)
		return ""
	if not _matches(actual, expected):
		return "expected %s, got %s" % [str(expected), str(actual)]
	return ""


## Numeric comparison: JSON cannot tell 5 from 5.0, and lerped values are floats
## even when the inputs were integers. A two-number array may come back as a
## Vector2 from the public API.
func _matches(actual, expected) -> bool:
	if typeof(actual) == TYPE_VECTOR2 and typeof(expected) == TYPE_ARRAY and expected.size() == 2:
		return _close(actual.x, expected[0]) and _close(actual.y, expected[1])
	if _is_number(actual) and _is_number(expected):
		return _close(actual, expected)
	if typeof(actual) == TYPE_ARRAY and typeof(expected) == TYPE_ARRAY:
		if actual.size() != expected.size():
			return false
		for index in actual.size():
			if not _matches(actual[index], expected[index]):
				return false
		return true
	return actual == expected


func _is_number(value) -> bool:
	return typeof(value) == TYPE_INT or typeof(value) == TYPE_FLOAT


func _close(actual, expected) -> bool:
	var value := float(actual)
	var want := float(expected)
	return absf(value - want) <= 1e-9 * (1.0 + absf(want))


func _fail(reason: String) -> void:
	printerr("interp_conformance FAILED: " + reason)
	quit(1)
