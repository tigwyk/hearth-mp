class_name HearthPatch
extends RefCounted
## Patch-op application for Hearth state trees.
##
## Semantics are identical to the Rust reference
## (`hearth_mp_protocol::patch::apply`); the conformance suite is
## `conformance/vectors/patch.json`.
##
## Keys of the state tree are always strings, and a path segment is a string
## (object key) or an integer (list index). Godot parses all JSON numbers as
## floats, so integral floats are accepted as indices.


## Apply one op to `state`.
##
## Returns the new state, or `null` on a bad path. A `set` with an empty path
## replaces the root, so callers must use the return value.
static func apply(state, op: Dictionary) -> Variant:
	var path: Array = op.get("path", [])
	match String(op.get("op", "")):
		"set":
			if path.is_empty():
				return op.get("value")
			var parent := _navigate(state, path.slice(0, path.size() - 1))
			if not parent[0] or not _set_child(parent[1], path[path.size() - 1], op.get("value")):
				return null
			return state
		"del":
			if path.is_empty():
				return null
			var parent := _navigate(state, path.slice(0, path.size() - 1))
			if not parent[0] or not _del_child(parent[1], path[path.size() - 1]):
				return null
			return state
		"map_insert":
			var target := _navigate(state, path)
			if not target[0] or typeof(target[1]) != TYPE_DICTIONARY:
				return null
			target[1][String(op.get("key", ""))] = op.get("value")
			return state
		"list_splice":
			var target := _navigate(state, path)
			if not target[0] or typeof(target[1]) != TYPE_ARRAY:
				return null
			var index := _index(op.get("index", 0))
			var delete := _index(op.get("delete", 0))
			var insert = op.get("insert", [])
			if (
				index < 0
				or delete < 0
				or index > target[1].size()
				or index + delete > target[1].size()
				or typeof(insert) != TYPE_ARRAY
			):
				return null
			for _i in range(delete):
				target[1].remove_at(index)
			for offset in range(insert.size()):
				target[1].insert(index + offset, insert[offset])
			return state
		_:
			return null


## Walk `path` from `state`. Returns `[found, value]`; a JSON `null` value is a
## valid result, so failure needs the separate flag.
static func _navigate(state, path: Array) -> Array:
	var cursor = state
	for segment in path:
		match typeof(cursor):
			TYPE_DICTIONARY:
				if typeof(segment) != TYPE_STRING or not cursor.has(segment):
					return [false, null]
				cursor = cursor[segment]
			TYPE_ARRAY:
				var index := _index(segment)
				if index < 0 or index >= cursor.size():
					return [false, null]
				cursor = cursor[index]
			_:
				return [false, null]
	return [true, cursor]


## Object keys are created when absent; list indices must already exist.
static func _set_child(parent, segment, value) -> bool:
	match typeof(parent):
		TYPE_DICTIONARY:
			if typeof(segment) != TYPE_STRING:
				return false
			parent[segment] = value
			return true
		TYPE_ARRAY:
			var index := _index(segment)
			if index < 0 or index >= parent.size():
				return false
			parent[index] = value
			return true
		_:
			return false


static func _del_child(parent, segment) -> bool:
	match typeof(parent):
		TYPE_DICTIONARY:
			if typeof(segment) != TYPE_STRING or not parent.has(segment):
				return false
			parent.erase(segment)
			return true
		TYPE_ARRAY:
			var index := _index(segment)
			if index < 0 or index >= parent.size():
				return false
			parent.remove_at(index)
			return true
		_:
			return false


## A valid list index, or -1. Accepts int and integral float (JSON numbers).
static func _index(segment) -> int:
	match typeof(segment):
		TYPE_INT:
			return int(segment) if int(segment) >= 0 else -1
		TYPE_FLOAT:
			var value: float = segment
			if value < 0.0 or value != floor(value):
				return -1
			return int(value)
		_:
			return -1
