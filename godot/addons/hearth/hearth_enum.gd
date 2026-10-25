class_name HearthEnum
extends RefCounted
## Typed wire enum.
##
## Enums use serde's external tagging on the wire: a unit variant is its bare
## name (`"Draw"`), every other variant is `{ "<Variant>": payload }`. A
## generated subclass lists the variants and their shapes in `_hearth_meta()`.

## The variant name exactly as it appears on the wire, for example `"Won"`.
var variant: String = ""
## `null` for unit variants; the decoded newtype/tuple/struct payload otherwise.
var value = null


## Reload from the wire value (a String or a one-key Dictionary).
func _hearth_load(raw) -> void:
	var meta: Dictionary = get_script()._hearth_meta()
	var types: Dictionary = meta.get("types", {})
	var name := ""
	var payload = null
	if typeof(raw) == TYPE_STRING:
		name = raw
	elif typeof(raw) == TYPE_DICTIONARY and (raw as Dictionary).size() == 1:
		var keys: Array = (raw as Dictionary).keys()
		name = String(keys[0])
		payload = (raw as Dictionary)[keys[0]]
	else:
		return
	variant = name
	var shape = null
	for entry in meta.get("variants", []):
		if String(entry.get("name", "")) == name:
			shape = entry.get("shape")
			break
	value = _decode_shape(shape, payload, types)


static func _decode_shape(shape, payload, types: Dictionary):
	if shape == null:
		return null
	if typeof(shape) == TYPE_STRING:
		return null if String(shape) == "unit" else payload
	if typeof(shape) != TYPE_DICTIONARY:
		return payload
	var spec: Dictionary = shape
	if spec.has("newtype"):
		return HearthObject._decode(spec["newtype"], payload, types)
	if spec.has("tuple"):
		var out := []
		var item_types: Array = spec["tuple"]
		var payload_items: Array = payload if typeof(payload) == TYPE_ARRAY else []
		for i in range(item_types.size()):
			var item = payload_items[i] if i < payload_items.size() else null
			out.append(HearthObject._decode(item_types[i], item, types))
		return out
	if spec.has("struct"):
		var out := {}
		var payload_dict: Dictionary = payload if typeof(payload) == TYPE_DICTIONARY else {}
		for field in spec["struct"]:
			var key := String(field.get("name", ""))
			out[key] = HearthObject._decode(field.get("ty"), payload_dict.get(key), types)
		return out
	return payload
