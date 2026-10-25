class_name HearthList
extends RefCounted
## Typed wire list (Rust `Vec`/`List`/`Array`).
##
## Emits `changed` only when the wire value differs, then rebuilds the typed
## items from scratch (a list has no stable per-index identity in the protocol).

signal changed()

var _items: Array = []
var _raw: Array = []
var _element_ty = null
var _types: Dictionary = {}


## Reload from a wire Array. `element_ty`/`types` are only needed on the first
## load; later reloads (and `route`) reuse them.
func _hearth_load(raw: Array, element_ty = null, types: Dictionary = {}) -> void:
	if element_ty != null:
		_element_ty = element_ty
	if not types.is_empty():
		_types = types
	if HearthObject._raw_equal(_raw, raw):
		return
	_raw = raw.duplicate(true)
	_items = []
	for value in raw:
		_items.append(HearthObject._decode(_element_ty, value, _types))
	changed.emit()


func at(index: int):
	if index < 0 or index >= _items.size():
		return null
	return _items[index]


func size() -> int:
	return _items.size()


func to_array() -> Array:
	return _items.duplicate()
