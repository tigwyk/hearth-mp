class_name HearthObject
extends RefCounted
## Base class of the typed state mirror (plan Task 6).
##
## `hearth-mp codegen --godot` emits a thin subclass per Rust sync type: typed
## `var`s, one `signal <gd>_changed(value)` per leaf/enum/option field, and a
## static `_hearth_meta()`. This runtime reloads the typed tree from the raw
## wire state and emits only the signals whose values changed, so game code can
## hold typed instances and connect to per-field signals.
##
## A `TypeRef` in the metadata is the JSON shape the Rust schema exports: a
## string for primitive types, or a one-key Dictionary such as `{"list": "u8"}`,
## `{"sync_map": ["player_id", "u8"]}` or `{"named": "Status"}`. `Named(n)`
## resolves through `types[n]`, a Dictionary from the Rust type name to the
## generated class script.
##
## Deviation from the plan's File Structure (approved): GDScript allows one
## `class_name` per file, so the plan's single `hearth_typed.gd` cannot declare
## all four classes. This file keeps `HearthObject` (plus `route`); the other
## globals live in `hearth_map.gd`, `hearth_list.gd` and `hearth_enum.gd`.
## Generated subclasses still `extends HearthObject` and reference
## `HearthMap`/`HearthList`/`HearthEnum` by global class name.

const _KIND_LEAF := 0
const _KIND_NAMED := 1
const _KIND_MAP := 2
const _KIND_LIST := 3

var _fields: Array = []
var _fields_by_wire: Dictionary = {}
var _types: Dictionary = {}
var _raw: Dictionary = {}


func _init() -> void:
	var script = get_script()
	var meta: Dictionary = script._hearth_meta()
	_fields = meta.get("fields", [])
	_types = meta.get("types", {})
	for field in _fields:
		_fields_by_wire[String(field.get("wire", ""))] = field


## Reload this object from `raw` (a wire-shape Dictionary).
##
## Leaf, enum and option fields are assigned and emit `<gd>_changed` only when
## their raw value differs. Object fields are reloaded in place (the typed
## instance is kept). Containers reload themselves and emit their own signals.
func _hearth_load(raw: Dictionary) -> void:
	for field in _fields:
		var gd := String(field.get("gd", ""))
		var wire := String(field.get("wire", ""))
		var ty = field.get("ty")
		var raw_value = raw.get(wire)
		match _kind(ty):
			_KIND_MAP:
				var map = get(gd)
				if map == null:
					map = HearthMap.new()
					set(gd, map)
				(map as HearthMap)._hearth_load(
					raw_value if typeof(raw_value) == TYPE_DICTIONARY else {},
					_map_value_ty(ty),
					_types
				)
			_KIND_LIST:
				var list = get(gd)
				if list == null:
					list = HearthList.new()
					set(gd, list)
				(list as HearthList)._hearth_load(
					raw_value if typeof(raw_value) == TYPE_ARRAY else [],
					_list_element_ty(ty),
					_types
				)
			_KIND_NAMED:
				_hearth_load_named(gd, wire, ty, raw_value)
			_:
				if _raw.has(wire) and _raw_equal(_raw[wire], raw_value):
					continue
				var value = _decode(ty, raw_value, _types)
				set(gd, value)
				_raw[wire] = _copy_raw(raw_value)
				emit_signal(gd + "_changed", value)


## Reload one patch op into the typed tree (plan Task 6 `route`).
##
## The container path is `op.path` for `map_insert`/`list_splice` and
## `op.path[:-1]` for `set`/`del`. The typed tree is walked along it to the
## deepest HearthObject/HearthMap/HearthList node, which is reloaded from the
## matching raw subtree. Maps the walk passed through reload too, so a nested
## change emits `item_changed` on its containing map — the plan's wording only
## names the deepest node, but its Task 6 assertion requires that map signal.
static func route(root: HearthObject, raw_root: Dictionary, op: Dictionary) -> void:
	var path: Array = op.get("path", [])
	var container_path: Array = path
	var op_name := String(op.get("op", ""))
	if op_name == "set" or op_name == "del":
		container_path = path.slice(0, path.size() - 1)
	var node = root
	var raw: Variant = raw_root
	var map_ancestors: Array = []
	for segment in container_path:
		var child = null
		var raw_child = null
		var wire := String(segment)
		if node is HearthObject:
			var field = (node as HearthObject)._fields_by_wire.get(wire)
			if field == null or typeof(raw) != TYPE_DICTIONARY or not (raw as Dictionary).has(wire):
				break
			child = (node as HearthObject).get(String(field.get("gd", "")))
			raw_child = (raw as Dictionary)[wire]
		elif node is HearthMap:
			if typeof(raw) != TYPE_DICTIONARY or not (raw as Dictionary).has(wire):
				break
			if not (node as HearthMap).has(wire):
				break
			child = (node as HearthMap).get_item(wire)
			raw_child = (raw as Dictionary)[wire]
			map_ancestors.append({"node": node, "raw": raw})
		elif node is HearthList:
			var index := int(segment)
			if typeof(raw) != TYPE_ARRAY or index < 0 or index >= (raw as Array).size():
				break
			if index >= (node as HearthList).size():
				break
			child = (node as HearthList).at(index)
			raw_child = (raw as Array)[index]
		else:
			break
		if not (child is HearthObject or child is HearthMap or child is HearthList):
			break
		node = child
		raw = raw_child
	_reload(node, raw)
	for i in range(map_ancestors.size() - 1, -1, -1):
		var ancestor: Dictionary = map_ancestors[i]
		_reload(ancestor["node"], ancestor["raw"])


func _hearth_load_named(gd: String, wire: String, ty: Dictionary, raw_value) -> void:
	var node = get(gd)
	if node == null:
		node = _make_named(ty, _types)
	if node is HearthEnum:
		if _raw.has(wire) and _raw_equal(_raw[wire], raw_value):
			return
		(node as HearthEnum)._hearth_load(raw_value)
		set(gd, node)
		_raw[wire] = _copy_raw(raw_value)
		emit_signal(gd + "_changed", node)
	elif node is HearthObject:
		set(gd, node)
		if typeof(raw_value) == TYPE_DICTIONARY:
			(node as HearthObject)._hearth_load(raw_value)


static func _reload(node, raw) -> void:
	if node is HearthObject:
		(node as HearthObject)._hearth_load(raw if typeof(raw) == TYPE_DICTIONARY else {})
	elif node is HearthMap:
		(node as HearthMap)._hearth_load(raw if typeof(raw) == TYPE_DICTIONARY else {})
	elif node is HearthList:
		(node as HearthList)._hearth_load(raw if typeof(raw) == TYPE_ARRAY else [])


## Decode one wire value according to its `TypeRef`, building typed instances
## for `Named` types and containers.
static func _decode(ty, raw, types: Dictionary):
	match typeof(ty):
		TYPE_STRING:
			return _decode_leaf(String(ty), raw)
		TYPE_DICTIONARY:
			var spec: Dictionary = ty
			if spec.has("option"):
				if raw == null:
					return null
				return _decode(spec["option"], raw, types)
			if spec.has("list"):
				var list := HearthList.new()
				list._hearth_load(raw if typeof(raw) == TYPE_ARRAY else [], spec["list"], types)
				return list
			if spec.has("array"):
				var list := HearthList.new()
				list._hearth_load(raw if typeof(raw) == TYPE_ARRAY else [], spec["array"][0], types)
				return list
			if spec.has("map"):
				var map := HearthMap.new()
				map._hearth_load(raw if typeof(raw) == TYPE_DICTIONARY else {}, spec["map"][1], types)
				return map
			if spec.has("sync_map"):
				var map := HearthMap.new()
				map._hearth_load(raw if typeof(raw) == TYPE_DICTIONARY else {}, spec["sync_map"][1], types)
				return map
			if spec.has("tuple"):
				var out := []
				var item_types: Array = spec["tuple"]
				var raw_items: Array = raw if typeof(raw) == TYPE_ARRAY else []
				for i in range(item_types.size()):
					var item = raw_items[i] if i < raw_items.size() else null
					out.append(_decode(item_types[i], item, types))
				return out
			if spec.has("named"):
				var node = _make_named(spec, types)
				if node is HearthEnum:
					(node as HearthEnum)._hearth_load(raw)
				elif node is HearthObject and typeof(raw) == TYPE_DICTIONARY:
					(node as HearthObject)._hearth_load(raw)
				return node
	return raw


static func _decode_leaf(name: String, raw):
	match name:
		"bool":
			return bool(raw)
		"i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64":
			return int(raw)
		"f32", "f64":
			return float(raw)
		"string", "player_id":
			return "" if raw == null else String(raw)
		"unit":
			return null
	return raw


static func _make_named(spec: Dictionary, types: Dictionary):
	var script = types.get(String(spec.get("named", "")))
	if script is GDScript:
		return (script as GDScript).new()
	return null


static func _kind(ty) -> int:
	if typeof(ty) != TYPE_DICTIONARY:
		return _KIND_LEAF
	var spec: Dictionary = ty
	if spec.has("map") or spec.has("sync_map"):
		return _KIND_MAP
	if spec.has("list") or spec.has("array"):
		return _KIND_LIST
	if spec.has("named"):
		return _KIND_NAMED
	return _KIND_LEAF


static func _map_value_ty(ty: Dictionary):
	if ty.has("map"):
		return ty["map"][1]
	return ty["sync_map"][1]


static func _list_element_ty(ty: Dictionary):
	if ty.has("list"):
		return ty["list"]
	return ty["array"][0]


## A cached raw value must not alias the live raw state tree, which HearthPatch
## mutates in place.
static func _copy_raw(value):
	if typeof(value) == TYPE_DICTIONARY or typeof(value) == TYPE_ARRAY:
		return (value as Variant).duplicate(true)
	return value


## Compare two cached/raw values without tripping Godot's strict `==` on
## mismatched containers; ints and floats from the two wire encodings are equal.
static func _raw_equal(a, b) -> bool:
	var type_a := typeof(a)
	var type_b := typeof(b)
	if type_a != type_b:
		var a_number := type_a == TYPE_INT or type_a == TYPE_FLOAT
		var b_number := type_b == TYPE_INT or type_b == TYPE_FLOAT
		if a_number and b_number:
			return float(a) == float(b)
		return false
	return a == b
