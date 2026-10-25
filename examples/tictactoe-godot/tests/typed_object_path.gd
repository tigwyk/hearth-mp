extends SceneTree
## Standing test for the typed runtime's object path (approved deviation from
## Task 7's "delete `tests/typed_fixture/`" line).
##
## The generated tic-tac-toe sample (`Board { cells, marks, turn, status }`) has
## no object-typed field and no map of objects, so the retargeted
## `tests/typed_runtime.gd` cannot cover the runtime behaviour the spec's
## `players: SyncMap<PlayerId, Player>` pattern needs: a nested patch inside a
## map-held object must reload that instance in place and emit both the object's
## field signal and `item_changed` on the containing map. The minimal two-class
## fixture in `tests/typed_object_fixture/` keeps that contract covered.
## No server and no generated classes needed:
##
##   godot --headless --path examples/tictactoe-godot -s tests/typed_object_path.gd

const Patch = preload("res://addons/hearth/hearth_patch.gd")
const FixtureRoot = preload("res://tests/typed_object_fixture/root_object.gd")

var _events: Array = []
var _failed := false


func _init() -> void:
	var root_object = FixtureRoot.new()
	var raw: Dictionary = _initial_raw()
	root_object._hearth_load(raw)
	var player = root_object.players.get_item("a")
	root_object.title_changed.connect(_on_field_changed.bind("title", root_object))
	root_object.players.item_added.connect(_on_map_added.bind("players"))
	root_object.players.item_removed.connect(_on_map_removed.bind("players"))
	root_object.players.item_changed.connect(_on_map_changed.bind("players"))
	player.score_changed.connect(_on_field_changed.bind("score", player))

	# Nested set: field signal on the instance the map returned before the patch,
	# then item_changed on the containing map.
	var op := {"op": "set", "path": ["players", "a", "score"], "value": 7}
	raw = _apply(raw, op)
	HearthObject.route(root_object, raw, op)
	var events := _take()
	_check(events.size() == 2, "nested set should emit 2 signals, got %s" % _fmt(events))
	var event := _event(events, 0, "score_changed")
	_check(event.get("instance") == player, "score_changed on the map-returned instance")
	_check(int(event.get("value")) == 7, "score_changed value is 7")
	event = _event(events, 1, "item_changed")
	_check(String(event.get("map", "")) == "players", "item_changed on players")
	_check(String(event.get("key", "")) == "a", "item_changed key is a")
	_check(event.get("value") == player, "item_changed value is the held instance")
	_check(root_object.players.get_item("a") == player, "map kept the instance across reload")
	if _failed:
		return

	# An identical reload emits nothing.
	_events.clear()
	root_object._hearth_load(raw)
	events = _take()
	_check(events.is_empty(), "identical reload should emit nothing, got %s" % _fmt(events))
	if _failed:
		return

	# A full-state reset keeps the instance and emits only the signals for the
	# parts that differ (the object's score and its map entry; title unchanged).
	var reset_raw: Dictionary = {"title": "match", "players": {"a": {"score": 9}}}
	root_object._hearth_load(reset_raw)
	events = _take()
	_check(events.size() == 2, "reset should emit 2 signals, got %s" % _fmt(events))
	event = _event(events, 0, "score_changed")
	_check(event.get("instance") == player, "reset score_changed on the kept instance")
	_check(int(event.get("value")) == 9, "reset score_changed value is 9")
	event = _event(events, 1, "item_changed")
	_check(String(event.get("map", "")) == "players", "reset item_changed on players")
	_check(String(event.get("key", "")) == "a", "reset item_changed key is a")
	_check(root_object.players.get_item("a") == player, "reset kept the instance")
	if _failed:
		return

	print("typed_object_path: ok")
	quit(0)


func _initial_raw() -> Dictionary:
	return {"title": "match", "players": {"a": {"score": 1}}}


func _apply(raw: Dictionary, op: Dictionary) -> Dictionary:
	var next = Patch.apply(raw, op)
	if typeof(next) != TYPE_DICTIONARY:
		_fail("HearthPatch.apply failed for " + JSON.stringify(op))
		return raw
	return next


func _take() -> Array:
	var events := _events
	_events = []
	return events


func _event(events: Array, index: int, kind: String) -> Dictionary:
	if index >= events.size():
		_fail("missing %s signal, got %s" % [kind, _fmt(events)])
		return {}
	var event: Dictionary = events[index]
	if String(event.get("signal", "")) != kind:
		_fail("signal %d is %s, want %s, got %s" % [index, event.get("signal"), kind, _fmt(events)])
		return {}
	return event


func _check(condition: bool, detail: String) -> void:
	if not condition:
		_fail(detail)


func _fmt(events: Array) -> String:
	var names: Array = []
	for event in events:
		names.append(String(event.get("signal", "?")))
	return str(names)


func _fail(reason: String) -> void:
	if _failed:
		return
	_failed = true
	printerr("typed_object_path FAILED: " + reason)
	quit(1)


func _on_field_changed(value, wire: String, instance) -> void:
	_events.append({"signal": wire + "_changed", "value": value, "instance": instance})


func _on_map_added(key: String, value, map_name: String) -> void:
	_events.append({"signal": "item_added", "map": map_name, "key": key, "value": value})


func _on_map_removed(key: String, map_name: String) -> void:
	_events.append({"signal": "item_removed", "map": map_name, "key": key})


func _on_map_changed(key: String, value, map_name: String) -> void:
	_events.append({"signal": "item_changed", "map": map_name, "key": key, "value": value})
