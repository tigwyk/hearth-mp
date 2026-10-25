extends SceneTree
## Typed-mirror runtime test against the generated classes (plan Task 7 Step 4).
##
## Run after `hearth codegen` has emitted `res://hearth/` (the generated output is
## committed, so normally nothing to do first). It loads the generated classes at
## run time instead of `preload` so running before codegen fails with an
## actionable message rather than a parse error.
##
##   hearth codegen --godot examples/tictactoe-godot/hearth --manifest-path examples/tictactoe/Cargo.toml
##   godot --headless --path examples/tictactoe-godot -s tests/typed_runtime.gd
##
## The generated tic-tac-toe schema (`Board { cells, marks, turn, status }`) has
## no object-typed field or map of objects, so the object-in-map signal path is
## covered by the standing `tests/typed_object_path.gd`; every other assertion
## here is the Task 6 Step 1 list.

const Patch = preload("res://addons/hearth/hearth_patch.gd")
const GENERATED_DIR := "res://hearth/"
const BOARD_FILE := "tic_tac_toe_board.gd"
const STATUS_FILE := "tic_tac_toe_status.gd"
const INTENTS_FILE := "tic_tac_toe_intents.gd"

var _events: Array = []
var _step := ""
var _failed := false


func _init() -> void:
	var board_script = load(GENERATED_DIR + BOARD_FILE)
	var status_script = load(GENERATED_DIR + STATUS_FILE)
	var intents_script = load(GENERATED_DIR + INTENTS_FILE)
	if board_script == null or status_script == null or intents_script == null:
		printerr(
			"typed_runtime: generated classes missing under res://hearth/. Run: "
			+ "hearth codegen --godot examples/tictactoe-godot/hearth --manifest-path examples/tictactoe/Cargo.toml"
		)
		quit(1)
		return

	_step = "generated metadata"
	_expect(
		String(board_script.SCHEMA_HASH).length() == 16,
		"generated state root carries a 16-char SCHEMA_HASH"
	)
	_expect(String(status_script.WON) == "Won", "generated enum carries the WON constant")
	_expect(
		intents_script.place(4) == {"Place": {"cell": 4}},
		"generated intents class builds the Place intent"
	)
	var board = board_script.new()
	var raw: Dictionary = _initial_raw()
	board._hearth_load(raw)
	_expect(board.turn is String, "turn is a typed String")
	_expect(board.cells is HearthList, "cells is a HearthList")
	_expect(board.marks is HearthMap, "marks is a HearthMap")
	_expect(board.status is HearthEnum, "status is a HearthEnum")
	if _failed:
		return
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
	_expect(board.marks.size() == 2, "typed marks size is 2")
	_expect(int(board.marks.get_item("p2")) == 2, "typed marks[p2] == 2")
	if _failed:
		return

	# set on an existing map value: item_changed, map instance kept.
	_step = "set marks[p1]"
	var marks_before = board.marks
	op = {"op": "set", "path": ["marks", "p1"], "value": 2}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "item_changed")
	_expect(String(event.get("map", "")) == "marks", "item_changed on marks")
	_expect(String(event.get("key", "")) == "p1", "item_changed key is p1")
	_expect(int(event.get("value")) == 2, "item_changed value is 2")
	_expect(board.marks == marks_before, "map instance kept across reload")
	if _failed:
		return

	# del on a map key emits item_removed.
	_step = "del marks[p2]"
	op = {"op": "del", "path": ["marks", "p2"]}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "item_removed")
	_expect(String(event.get("map", "")) == "marks", "item_removed on marks")
	_expect(String(event.get("key", "")) == "p2", "removed key is p2")
	_expect(not board.marks.has("p2"), "typed marks[p2] is gone")
	if _failed:
		return

	# list_splice on cells emits exactly one changed, list instance kept.
	_step = "list_splice cells"
	var cells_before = board.cells
	op = {"op": "list_splice", "path": ["cells"], "index": 0, "delete": 1, "insert": [1]}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "changed")
	_expect(String(event.get("list", "")) == "cells", "changed on cells")
	_expect(int(board.cells.at(0)) == 1, "typed cells[0] == 1")
	_expect(board.cells == cells_before, "list instance kept across reload")
	if _failed:
		return

	# set on a leaf field emits exactly one field signal.
	_step = "set turn"
	op = {"op": "set", "path": ["turn"], "value": "p2"}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "turn_changed")
	_expect(String(event.get("value", "")) == "p2", "turn_changed value is p2")
	if _failed:
		return

	# set inside an externally tagged enum: status_changed with variant Won.
	_step = "set status Won"
	var status_before = board.status
	op = {"op": "set", "path": ["status"], "value": {"Won": "p1"}}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "status_changed")
	var status = event.get("value")
	_expect(status is HearthEnum, "status value is a HearthEnum")
	_expect(String(status.variant) == "Won", "variant is Won, got %s" % String(status.variant))
	_expect(String(status.value) == "p1", "newtype value is p1, got %s" % String(status.value))
	_expect(board.status == status_before, "enum instance kept across reload")
	if _failed:
		return

	# set inside the enum's newtype payload (path ["status","Won"]) still emits
	# exactly one status_changed.
	_step = "set status.Won"
	op = {"op": "set", "path": ["status", "Won"], "value": "p2"}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "status_changed")
	status = event.get("value")
	_expect(String(status.variant) == "Won", "variant is Won")
	_expect(String(status.value) == "p2", "newtype value is p2")
	if _failed:
		return

	# Unit variant: status_changed with value null.
	_step = "set status Draw"
	op = {"op": "set", "path": ["status"], "value": "Draw"}
	raw = _apply(raw, op)
	HearthObject.route(board, raw, op)
	events = _take()
	_expect(events.size() == 1, "want 1 signal, got %s" % _fmt(events))
	event = _event(events, 0, "status_changed")
	status = event.get("value")
	_expect(String(status.variant) == "Draw", "variant is Draw")
	_expect(status.value == null, "unit variant value is null")
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
	var cells_reset = board.cells
	var marks_reset = board.marks
	var status_reset = board.status
	var reset_raw := {
		"cells": [1, 0, 0, 0, 0, 0, 0, 0, 0],
		"marks": {"p1": 2},
		"turn": "p1",
		"status": "Waiting",
	}
	_events.clear()
	board._hearth_load(reset_raw)
	events = _take()
	_expect(events.size() == 2, "want 2 signals, got %s" % _fmt(events))
	event = _event(events, 0, "turn_changed")
	_expect(String(event.get("value", "")) == "p1", "turn_changed value is p1")
	if _failed:
		return
	event = _event(events, 1, "status_changed")
	status = event.get("value")
	_expect(status is HearthEnum, "reset status is a HearthEnum")
	_expect(String(status.variant) == "Waiting", "reset variant is Waiting")
	_expect(board.cells == cells_reset, "list instance survives a reset")
	_expect(board.marks == marks_reset, "map instance survives a reset")
	_expect(board.status == status_reset, "enum instance survives a reset")
	if _failed:
		return

	print("typed_runtime: ok")
	quit(0)


func _initial_raw() -> Dictionary:
	return {
		"cells": [0, 0, 0, 0, 0, 0, 0, 0, 0],
		"marks": {"p1": 1},
		"turn": "p1",
		"status": "Waiting",
	}


func _connect(board) -> void:
	board.turn_changed.connect(_on_field_changed.bind("turn", board))
	board.status_changed.connect(_on_field_changed.bind("status", board))
	board.cells.changed.connect(_on_list_changed.bind("cells"))
	board.marks.item_added.connect(_on_map_added.bind("marks"))
	board.marks.item_removed.connect(_on_map_removed.bind("marks"))
	board.marks.item_changed.connect(_on_map_changed.bind("marks"))


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
