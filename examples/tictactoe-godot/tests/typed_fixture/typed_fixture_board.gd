## Hand-written fixture for `tests/typed_runtime.gd`.
##
## Mirrors the exact class format `hearth codegen --godot` emits (plan Task 7)
## so the typed runtime can be tested before the generator exists. Deleted once
## the generated classes land in `examples/tictactoe-godot/hearth/`.
class_name TypedFixtureBoard
extends HearthObject

const SCHEMA_HASH := "0123456789abcdef"

signal turn_changed(value)
signal status_changed(value)

var turn: String
var cells: HearthList
var marks: HearthMap
var players: HearthMap
var status: TypedFixtureStatus


static func _hearth_meta() -> Dictionary:
	return {
		"fields": [
			{"gd": "turn", "wire": "turn", "ty": "player_id"},
			{"gd": "cells", "wire": "cells", "ty": {"list": "u8"}},
			{"gd": "marks", "wire": "marks", "ty": {"sync_map": ["player_id", "u8"]}},
			{"gd": "players", "wire": "players", "ty": {"sync_map": ["player_id", {"named": "Player"}]}},
			{"gd": "status", "wire": "status", "ty": {"named": "Status"}},
		],
		"types": {
			"Player": preload("res://tests/typed_fixture/typed_fixture_player.gd"),
			"Status": preload("res://tests/typed_fixture/typed_fixture_status.gd"),
		},
	}
