extends RefCounted
## Pure-GDScript snapshot interpolation for ticked rooms (v0.4 Task 6).
##
## The port of `hearth_mp_client::Interpolator` + `ClockSync` used by the web
## backend (`hearth_web_client.gd`); `Hearth.interpolator(paths)` returns one of
## these on web builds and a native `HearthInterp` otherwise. Both expose the
## same `sample`/`sample_v2` API.
##
## A tracked path may contain a `"*"` segment (for example
## `["avatars", "*", "pos"]`), which matches every key or list index at that
## level: each snapshot records every concrete path.
##
## No `class_name`: the native `HearthInterp` class owns that global name, so
## this script is always preloaded by path.

## Retry-free defaults shared with the native wrapper.
const DEFAULT_TICK_HZ := 20

## Number of clock samples kept (the least-delayed one wins).
const CLOCK_SAMPLES := 32
## A newest sample further than this from the estimate is a server clock jump.
const CLOCK_JUMP_MS := 1000
## How long snapshots are kept, in server milliseconds.
const SNAPSHOT_WINDOW_MS := 1000
## A tracked path segment that matches every key/index at that level.
const WILDCARD := "*"

## Test seam: when >= 0 it overrides the synced server clock (the conformance
## test sets `render_t + 2 * period`). In normal use the synced clock is used.
var now_ms: int = -1

var _period_ms: int
## Tracked path patterns; `"*"` segments match every key at their level.
var _paths: Array = []
## Snapshots ordered by server time: {"t": int, "values": [[path, value], ...]}.
var _snapshots: Array = []
## Implied clock offsets in arrival order, at most CLOCK_SAMPLES.
var _offsets: Array = []


func _init(tick_hz: int = DEFAULT_TICK_HZ) -> void:
	_period_ms = int(1000 / maxi(tick_hz, 1))


## The tick period in milliseconds.
func period_ms() -> int:
	return _period_ms


## Track `path` in every snapshot from now on. Tracking a path twice is a no-op.
func track(path: Array) -> void:
	if not _paths.has(path):
		_paths.append(path.duplicate(true))


## Forget every snapshot and clock sample, keeping the tracked paths. Called
## when a FULL_STATE replaces the state so old snapshots cannot leak in.
func reset() -> void:
	_snapshots.clear()
	_offsets.clear()


## Record a snapshot of every tracked path from `state` (called for each PATCH).
func _on_patch(state: Dictionary, server_t: int) -> void:
	if not _snapshots.is_empty() and server_t <= int(_snapshots[_snapshots.size() - 1]["t"]):
		return
	_observe(server_t, Time.get_ticks_msec())
	var values: Array = []
	for pattern in _paths:
		_collect(state, pattern, 0, [], values)
	_snapshots.append({"t": server_t, "values": values})

	# Keep a one-second window, but never fewer than the two snapshots a bracket
	# needs: a stalled stream can leave a wider gap than a second.
	while _snapshots.size() > 2:
		if server_t - int(_snapshots[0]["t"]) <= SNAPSHOT_WINDOW_MS:
			break
		_snapshots.pop_front()


## Interpolate `path` at the current render time. Numbers come back as floats,
## a two-number array as a Vector2, other values as their wire shape.
func sample(path: Array) -> Variant:
	var raw := _sample_raw(path)
	if not raw[0]:
		return null
	return _to_output(raw[1])


## Like `sample`, but always a Vector2 (`Vector2.ZERO` when missing or not a
## two-number value).
func sample_v2(path: Array) -> Vector2:
	var raw := _sample_raw(path)
	if not raw[0]:
		return Vector2.ZERO
	var value = raw[1]
	if typeof(value) == TYPE_ARRAY and value.size() == 2:
		if _is_number(value[0]) and _is_number(value[1]):
			return Vector2(float(value[0]), float(value[1]))
	return Vector2.ZERO


## Sample as `[found, value]` so a missing path is distinct from a null value.
func _sample_raw(path: Array) -> Array:
	var render_t := _render_time()
	if render_t < 0:
		return [false, null]
	var tracked := false
	for pattern in _paths:
		if _pattern_matches(pattern, path):
			tracked = true
			break
	if not tracked or _snapshots.is_empty():
		return [false, null]

	var oldest: Dictionary = _snapshots[0]
	if render_t < int(oldest["t"]):
		return _value_in(oldest, path)
	var newest: Dictionary = _snapshots[_snapshots.size() - 1]
	if render_t >= int(newest["t"]):
		return _extrapolate(path, render_t)
	for index in range(_snapshots.size() - 1):
		var left: Dictionary = _snapshots[index]
		var right: Dictionary = _snapshots[index + 1]
		if render_t < int(right["t"]):
			return _bracket(left, right, path, render_t)
	return _extrapolate(path, render_t)


## Record every concrete path matched by `pattern` from `state`.
func _collect(state, pattern: Array, depth: int, prefix: Array, out: Array) -> void:
	if depth == pattern.size():
		out.append([prefix.duplicate(true), _copy(state)])
		return
	var segment = pattern[depth]
	if typeof(segment) == TYPE_STRING and segment == WILDCARD:
		match typeof(state):
			TYPE_DICTIONARY:
				for key in state:
					prefix.append(key)
					_collect(state[key], pattern, depth + 1, prefix, out)
					prefix.pop_back()
			TYPE_ARRAY:
				for index in state.size():
					prefix.append(index)
					_collect(state[index], pattern, depth + 1, prefix, out)
					prefix.pop_back()
	else:
		match typeof(state):
			TYPE_DICTIONARY:
				if typeof(segment) == TYPE_STRING and state.has(segment):
					prefix.append(segment)
					_collect(state[segment], pattern, depth + 1, prefix, out)
					prefix.pop_back()
			TYPE_ARRAY:
				var index := _path_index(segment)
				if index >= 0 and index < state.size():
					prefix.append(index)
					_collect(state[index], pattern, depth + 1, prefix, out)
					prefix.pop_back()


## The value recorded for the concrete `path` in `snapshot`, as `[found, value]`.
func _value_in(snapshot: Dictionary, path: Array) -> Array:
	for entry in snapshot["values"]:
		if entry[0] == path:
			return [true, entry[1]]
	return [false, null]


func _bracket(left: Dictionary, right: Dictionary, path: Array, render_t: int) -> Array:
	var from := _value_in(left, path)
	if not from[0]:
		return [false, null]
	var to := _value_in(right, path)
	if not to[0]:
		return [false, null]
	var span := int(right["t"]) - int(left["t"])
	if span <= 0:
		return [true, _copy(to[1])]
	var ratio := float(render_t - int(left["t"])) / float(span)
	var lerped := _lerp_values(from[1], to[1], ratio)
	if lerped[0]:
		return lerped
	return [true, _copy(from[1])]


## Sample past the newest snapshot: linear for at most one period, then hold.
func _extrapolate(path: Array, render_t: int) -> Array:
	var newest: Dictionary = _snapshots[_snapshots.size() - 1]
	var newest_value := _value_in(newest, path)
	if not newest_value[0]:
		return [false, null]
	if _snapshots.size() < 2:
		return [true, _copy(newest_value[1])]
	var previous: Dictionary = _snapshots[_snapshots.size() - 2]
	var previous_value := _value_in(previous, path)
	if not previous_value[0]:
		return [false, null]
	var target := mini(render_t, int(newest["t"]) + _period_ms)
	var span := int(newest["t"]) - int(previous["t"])
	if span <= 0:
		return [true, _copy(newest_value[1])]
	var ratio := float(target - int(previous["t"])) / float(span)
	var lerped := _lerp_values(previous_value[1], newest_value[1], ratio)
	if lerped[0]:
		return lerped
	return [true, _copy(newest_value[1])]


## Linear interpolation for numbers and equal-length number arrays, as
## `[ok, value]`. Anything else returns `[false, null]` so callers step.
func _lerp_values(from, to, ratio: float) -> Array:
	if ratio == 0.0:
		return [true, _copy(from)]
	if ratio == 1.0:
		return [true, _copy(to)]
	if _is_number(from) and _is_number(to):
		var start := float(from)
		return [true, start + (float(to) - start) * ratio]
	if typeof(from) == TYPE_ARRAY and typeof(to) == TYPE_ARRAY and from.size() == to.size():
		var out := []
		for index in from.size():
			if not (_is_number(from[index]) and _is_number(to[index])):
				return [false, null]
			out.append(float(from[index]) + (float(to[index]) - float(from[index])) * ratio)
		return [true, out]
	return [false, null]


## Whether a tracked `pattern` (which may contain `*`) covers `path`.
func _pattern_matches(pattern: Array, path: Array) -> bool:
	if pattern.size() != path.size():
		return false
	for index in pattern.size():
		var pattern_segment = pattern[index]
		if typeof(pattern_segment) == TYPE_STRING and pattern_segment == WILDCARD:
			continue
		if pattern_segment != path[index]:
			return false
	return true


## Convert a sampled value to the public Godot shape: numbers as floats, a
## two-number array as a Vector2, containers recursively.
func _to_output(value) -> Variant:
	if typeof(value) == TYPE_ARRAY:
		if value.size() == 2 and _is_number(value[0]) and _is_number(value[1]):
			return Vector2(float(value[0]), float(value[1]))
		return _float_tree(value)
	if typeof(value) == TYPE_DICTIONARY:
		return _float_tree(value)
	if _is_number(value):
		return float(value)
	return value


func _float_tree(value) -> Variant:
	match typeof(value):
		TYPE_INT, TYPE_FLOAT:
			return float(value)
		TYPE_ARRAY:
			var out := []
			for item in value:
				out.append(_float_tree(item))
			return out
		TYPE_DICTIONARY:
			var out := {}
			for key in value:
				out[key] = _float_tree(value[key])
			return out
		_:
			return value


## A valid list index for a path segment, or -1.
func _path_index(segment) -> int:
	match typeof(segment):
		TYPE_INT:
			return int(segment)
		TYPE_FLOAT:
			return int(segment) if segment == floor(segment) else -1
		_:
			return -1


func _is_number(value) -> bool:
	return typeof(value) == TYPE_INT or typeof(value) == TYPE_FLOAT


## Deep copy a snapshot value: patch application mutates the live state tree.
func _copy(value) -> Variant:
	if typeof(value) == TYPE_ARRAY or typeof(value) == TYPE_DICTIONARY:
		return value.duplicate(true)
	return value


func _observe(server_t: int, local_recv_ms: int) -> void:
	var offset := server_t - local_recv_ms
	if not _offsets.is_empty() and absi(offset - _max_offset()) > CLOCK_JUMP_MS:
		_offsets.clear()
	_offsets.append(offset)
	if _offsets.size() > CLOCK_SAMPLES:
		_offsets.remove_at(0)


func _max_offset() -> int:
	var best: int = _offsets[0]
	for offset in _offsets:
		best = maxi(best, int(offset))
	return best


## The estimated server "now", or -1 before the first clock sample.
func _server_now_ms() -> int:
	if now_ms >= 0:
		return now_ms
	if _offsets.is_empty():
		return -1
	return Time.get_ticks_msec() + _max_offset()


## Render two tick periods behind the server clock.
func _render_time() -> int:
	var server_now := _server_now_ms()
	if server_now < 0:
		return -1
	return maxi(0, server_now - 2 * _period_ms)
