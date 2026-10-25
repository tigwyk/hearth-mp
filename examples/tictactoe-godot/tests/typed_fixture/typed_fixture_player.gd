## Hand-written fixture for `tests/typed_runtime.gd`; see typed_fixture_board.gd.
class_name TypedFixturePlayer
extends HearthObject

signal score_changed(value)

var score: int


static func _hearth_meta() -> Dictionary:
	return {
		"fields": [
			{"gd": "score", "wire": "score", "ty": "u32"},
		],
		"types": {},
	}
