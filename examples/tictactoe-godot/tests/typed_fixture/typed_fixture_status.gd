## Hand-written fixture for `tests/typed_runtime.gd`; see typed_fixture_board.gd.
class_name TypedFixtureStatus
extends HearthEnum

const WAITING := "Waiting"
const PLAYING := "Playing"
const WON := "Won"
const DRAW := "Draw"


static func _hearth_meta() -> Dictionary:
	return {
		"variants": [
			{"name": "Waiting", "shape": "unit"},
			{"name": "Playing", "shape": "unit"},
			{"name": "Won", "shape": {"newtype": "player_id"}},
			{"name": "Draw", "shape": "unit"},
		],
		"types": {},
	}
