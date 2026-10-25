extends SceneTree
## Typed-mirror runtime test (plan Task 6 Step 1).
##
## Applies raw patch ops with the v0.1 `HearthPatch`, routes them through
## `HearthObject.route` against hand-written Task 7-format fixture classes, and
## asserts the signals each op emits. No server and no network needed:
##
##   godot --headless --path examples/tictactoe-godot -s tests/typed_runtime.gd

const Patch = preload("res://addons/hearth/hearth_patch.gd")
const FixtureBoard = preload("res://tests/typed_fixture/typed_fixture_board.gd")

var _events: Array = []
var _step := ""
var _failed := false


func _init() -> void:
	var board = FixtureBoard.new()
	var raw: Dictionary = _initial_raw()
	board._hearth_load(raw)
	_connect(board)

	# map_insert on marks emits exactly one item_added.
	_step = "map_insert marks"
	var op := {"op": "map_insert", "path": ["marks"], "key": "p2", "value": 2}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	var events := _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	var event := _event(events, 0, "item_added")
	_expect(String(event.get("map", "")) == "marks", "signal came from marks")
	_expect(String(event.get("key", "")) == "p2", "added key is p2")
	_expect(int(event.get("value")) == 2, "added value is 2")
	_expect(int(board.marks.get_item("p2")) == 2, "typed marks[p2] == 2")
	if _failed:
		return

	# set on a field of a map-of-objects value emits the field signal on the
	# instance held before the reload, plus item_changed on the containing map.
	_step = "set players[p1].score"
	var player = board.players.get_item("p1")
	op = {"op": "set", "path": ["players", "p1", "score"], "value": 7}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 2, "want 2 signals, got %s" % _fmt(events))
	event = _event(events, 0, "score_changed")
	_expect(event.get("instance") == player, "score_changed on the held instance")
	_expect(int(event.get("value")) == 7, "score_changed value is 7")
	event = _event(events, 1, "item_changed")
	_expect(String(event.get("map", "")) == "players", "item_changed on players")
	_expect(String(event.get("key", "")) == "p1", "item_changed key is p1")
	_expect(event.get("value") == player, "item_changed value is the held instance")
	_expect(board.players.get_item("p1") == player, "map kept the instance across reload")
	if _failed:
		return

	# del on a map key emits item_removed.
	_step = "del marks[p1]"
	op = {"op": "del", "path": ["marks", "p1"]}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "item_removed")
	_expect(String(event.get("map", "")) == "marks", "item_removed on marks")
	_expect(String(event.get("key", "")) == "p1", "removed key is p1")
	_expect(not board.marks.has("p1"), "typed marks[p1] is gone")
	if _failed:
		return

	# list_splice on cells emits exactly one changed.
	_step = "list_splice cells"
	op = {"op": "list_splice", "path": ["cells"], "index": 0, "delete": 1, "insert": [1]}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "changed")
	_expect(String(event.get("list", "")) == "cells", "changed on cells")
	_expect(int(board.cells.at(0)) == 1, "typed cells[0] == 1")
	if _failed:
		return

	# set inside an externally tagged enum emits status_changed with variant Won.
	_step = "set status.Won"
	op = {"op": "set", "path": ["status", "Won"], "value": "p2"}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "status_changed")
	if _failed:
		return
	var status = event.get("value")
	_expect(status is HearthEnum, "status value is a HearthEnum")
	_expect(String(status.variant) == "Won", "variant is Won, got %s" % String(status.variant))
	_expect(String(status.value) == "p2", "newtype value is p2, got %s" % String(status.value))
	_expect(board.status == status, "enum instance kept across reload")
	if _failed:
		return

	# A reload with identical raw values emits no signals.
	_step = "identical reload"
	_events.clear()
	board._hearth_load(raw)
	events = _take()
	_expect(events.is_empty(), "want no signals, got %s" % _fmt(events))
	if _failed:
		return

	# A full-state reset emits only the signals for the parts that differ.
	_step = "state reset"
	var reset_raw := {
		"turn": "p2",
		"cells": [1, 0, 0, 0, 0, 0, 0, 0, 0],
		"marks": {"p2": 2},
		"players": {"p1": {"score": 7}},
		"status": "Draw",
	}
	_events.clear()
	board._hearth_load(reset_raw)
	events = _take()
	_expect(events.size() == 2, "want 2 signals, got %s" % _fmt(events))
	event = _event(events, 0, "turn_changed")
	_expect(String(event.get("value", "")) == "p2", "turn_changed value is p2")
	if _failed:
		return
	event = _event(events, 1, "status_changed")
	if _failed:
		return
	var reset_status = event.get("value")
	_expect(reset_status is HearthEnum, "reset status is a HearthEnum")
	_expect(String(reset_status.variant) == "Draw", "reset variant is Draw")
	_expect(board.players.get_item("p1") == player, "instances survive a reset")
	_expect(int(board.marks.get_item("p2")) == 2, "typed marks[p2] after reset")
	if _failed:
		return

	print("typed_runtime: ok")
	quit(0)


func _initial_raw() -> Dictionary:
	return {
		"turn": "p1",
		"cells": [0, 0, 0, 0, 0, 0, 0, 0, 0],
		"marks": {"p1": 1},
		"players": {"p1": {"score": 0}},
		"status": {"Won": "p1"},
	}


func _connect(board) -> void:
	board.turn_changed.connect(_on_field_changed.bind("turn", board))
	board.status_changed.connect(_on_field_changed.bind("status", board))
	board.cells.changed.connect(_on_list_changed.bind("cells"))
	board.marks.item_added.connect(_on_map_added.bind("marks"))
	board.marks.item_removed.connect(_on_map_removed.bind("marks"))
	board.marks.item_changed.connect(_on_map_changed.bind("marks"))
	board.players.item_added.connect(_on_map_added.bind("players"))
	board.players.item_removed.connect(_on_map_removed.bind("players"))
	board.players.item_changed.connect(_on_map_changed.bind("players"))
	var player = board.players.get_item("p1")
	player.score_changed.connect(_on_field_changed.bind("score", player))


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


func _expect(condition: bool, detail: String) -> void:
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
	printerr("typed_runtime FAILED (%s): %s" % [_step, reason])
	quit(1)


func _on_field_changed(value, wire: String, instance) -> void:
	_events.append({"signal": wire + "_changed", "value": value, "instance": instance})


func _on_map_added(key: String, value, map_name: String) -> void:
	_events.append({"signal": "item_added", "map": map_name, "key": key, "value": value})


func _on_map_removed(key: String, map_name: String) -> void:
	_events.append({"signal": "item_removed", "map": map_name, "key": key})


func _on_map_changed(key: String, value, map_name: String) -> void:
	_events.append({"signal": "item_changed", "map": map_name, "key": key, "value": value})


func _on_list_changed(list_name: String) -> void:
	_events.append({"signal": "changed", "list": list_name})
