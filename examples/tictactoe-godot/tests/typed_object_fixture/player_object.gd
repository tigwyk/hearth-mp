## Standing object-path fixture player; exercised by `tests/typed_object_path.gd`.
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
