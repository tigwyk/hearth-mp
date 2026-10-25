## Standing object-path fixture root; exercised by `tests/typed_object_path.gd`.
##
## The generated tic-tac-toe schema has no object-typed field or map of objects,
## so this tiny hand-written pair keeps the runtime's object-in-map contract
## covered after `tests/typed_fixture/` is deleted (approved plan deviation).
extends HearthObject

signal title_changed(value)

var title: String
var players: HearthMap


static func _hearth_meta() -> Dictionary:
	return {
		"fields": [
			{"gd": "title", "wire": "title", "ty": "string"},
			{"gd": "players", "wire": "players", "ty": {"sync_map": ["player_id", {"named": "Player"}]}},
		],
		"types": {
			"Player": preload("res://tests/typed_object_fixture/player_object.gd"),
		},
	}
