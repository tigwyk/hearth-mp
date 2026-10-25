class_name HearthMap
extends RefCounted
## Typed wire map (Rust `Map`/`SyncMap`): String keys, typed values.
##
## Emits `item_added` for new keys, `item_removed` for gone keys, and
## `item_changed` for keys present in both whose raw value differs. Object
## values are reloaded in place, so a held `get_item(key)` instance survives.

signal item_added(key: String, value)
signal item_removed(key: String)
signal item_changed(key: String, value)

var _values: Dictionary = {}
var _raw: Dictionary = {}
var _value_ty = null
var _types: Dictionary = {}


## Reload from a wire Dictionary. `value_ty`/`types` are only needed on the
## first load; later reloads (and `route`) reuse them.
func _hearth_load(raw: Dictionary, value_ty = null, types: Dictionary = {}) -> void:
	if value_ty != null:
		_value_ty = value_ty
	if not types.is_empty():
		_types = types

	var removed: Array = []
	for key in _values.keys():
		if not raw.has(key):
			removed.append(key)
	for key in removed:
		_values.erase(key)
		_raw.erase(key)
		item_removed.emit(String(key))

	for key in raw.keys():
		var raw_value = raw[key]
		if _values.has(key):
			if _raw.has(key) and HearthObject._raw_equal(_raw[key], raw_value):
				continue
			var value = _values[key]
			if value is HearthObject and typeof(raw_value) == TYPE_DICTIONARY:
				(value as HearthObject)._hearth_load(raw_value)
			else:
				value = HearthObject._decode(_value_ty, raw_value, _types)
				_values[key] = value
			_raw[key] = HearthObject._copy_raw(raw_value)
			item_changed.emit(String(key), _values[key])
		else:
			var value = HearthObject._decode(_value_ty, raw_value, _types)
			_values[key] = value
			_raw[key] = HearthObject._copy_raw(raw_value)
			item_added.emit(String(key), value)


func get_item(key):
	return _values.get(String(key))


func has(key: String) -> bool:
	return _values.has(key)


func keys() -> Array:
	return _values.keys()


func size() -> int:
	return _values.size()
