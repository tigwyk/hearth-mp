extends Node
## Pure-GDScript Hearth client for web exports: JSON text frames only.
##
## Same methods and signals as the `HearthNative` GDExtension class, so
## `hearth_client.gd` can swap the backend without changing game code.
##
## A dropped socket is retried with `hearth_reconnect.gd` until the grace window
## is spent. Intents that were in flight at the drop are reported
## (`intents_unconfirmed`) and never resent; queued intents wait for the next
## `JOIN_OK`.
##
## The state tree comes from `JSON.parse_string`, so all numbers are floats;
## patch paths are applied by `hearth_patch.gd` with the same semantics as the
## Rust reference.
##
## Ticked rooms also get snapshot interpolation: `Hearth.interpolator(paths)`
## returns a `hearth_interp.gd` instance that this backend feeds from every
## ticked `PATCH` (the optional `t` field is the server clock).

signal joined(player_id: String, seat: int)
signal state_reset()
signal patched(ops: Array)
signal acked(state_seq: int, intent_seqs: Array)
## A room event. Reserved `hearth.*` names never reach this signal; they are
## delivered through the dedicated signals below.
signal event(name: String, data: Variant)
## A dedicated-server handoff (reserved `hearth.handoff` event).
signal handoff(sid: String, endpoint: String, ticket: String)
## The handoff session is over (reserved `hearth.handoff_ended` event).
signal handoff_ended(sid: String, reason: String)
signal error(code: String, message: String, ref_seq: int)
signal disconnected(code: int, reason: String)
signal reconnecting(attempt: int, retry_in_ms: int)
signal reconnected()
signal intents_unconfirmed(seqs: Array)

const Patch = preload("res://addons/hearth/hearth_patch.gd")
const Reconnect = preload("res://addons/hearth/hearth_reconnect.gd")
const Interp = preload("res://addons/hearth/hearth_interp.gd")

const PROTOCOL_VERSION := 1
const MAX_MSGS_PER_SEC := 20
const MAX_INTENTS_PER_MSG := 32
const PING_INTERVAL_MS := 20000

var _peer: WebSocketPeer = null
var _url := ""
var _schema_hash := ""
var _opts: Dictionary = {}
var _state = {}
var _state_seq := 0
var _player_id := ""
var _next_intent_seq := 1
var _pending: Array = []
var _in_flight: Array[int] = []
var _awaiting_join := false
var _last_send_ms := -100000
var _last_ping_ms := 0
var _hello_sent := false
var _desynced := false
var _first_drop_ms := -1
var _attempt := 0
var _retry_at_ms := -1
var _resuming := false
var _interpolators: Array = []
var _last_patch_t := -1
## The last ticked PATCH tick; ticked patches repeat the persisted `seq`.
var _last_tick := -1


func connect_to_room(url: String, schema_hash: String, opts: Dictionary) -> void:
	_url = url
	_schema_hash = schema_hash
	_opts = opts
	_state = {}
	_state_seq = 0
	_player_id = ""
	_next_intent_seq = 1
	_pending.clear()
	_in_flight.clear()
	_desynced = false
	_awaiting_join = false
	_last_send_ms = -100000
	_last_patch_t = -1
	_last_tick = -1
	for interp in _interpolators:
		interp.reset()
	_cancel_reconnect()
	_open_socket()


func send_intent(intent) -> int:
	var seq := _next_intent_seq
	_next_intent_seq += 1
	_pending.append({"seq": seq, "intent": _whole_floats_to_ints(intent)})
	return seq


## JSON.stringify writes 3.0 as `3.0`, which the server's integer fields reject,
## and JSON.parse_string makes every state number a float. A whole float within
## ±2^53 goes out as an int, as the native backend sends it; float fields accept
## `3` just as well.
static func _whole_floats_to_ints(value: Variant) -> Variant:
	match typeof(value):
		TYPE_FLOAT:
			return int(value) if value == floorf(value) and absf(value) <= 9007199254740992.0 else value
		TYPE_ARRAY:
			var out := []
			for item in value:
				out.append(_whole_floats_to_ints(item))
			return out
		TYPE_DICTIONARY:
			var out := {}
			for key in value:
				out[key] = _whole_floats_to_ints(value[key])
			return out
	return value


func leave() -> void:
	_cancel_reconnect()
	if _peer != null and _peer.get_ready_state() == WebSocketPeer.STATE_OPEN:
		_send({"op": "LEAVE"})
		_peer.close()
	_peer = null


func get_state() -> Dictionary:
	return _state if _state is Dictionary else {}


func get_player_id() -> String:
	return _player_id


## Create an interpolator fed from every ticked `PATCH` from now on. `paths` is
## an Array of paths (Arrays of keys or list indices; a `"*"` segment matches
## every key at its level).
func create_interpolator(paths: Array) -> Object:
	var interp = Interp.new()
	for path in paths:
		interp.track(path)
	_interpolators.append(interp)
	return interp


func _feed_interpolators() -> void:
	if _interpolators.is_empty() or _last_patch_t < 0:
		return
	for interp in _interpolators:
		interp._on_patch(_state, _last_patch_t)


## Test seam: drop the socket locally and handle it like an abnormal (1006)
## close. Debug builds only.
func _debug_drop() -> void:
	if not OS.is_debug_build() or _peer == null:
		return
	_peer.close()
	_peer = null
	_handle_drop(1006, "")


func _process(_delta: float) -> void:
	var now := Time.get_ticks_msec()
	if _peer == null:
		if _retry_at_ms < 0 or now < _retry_at_ms:
			return
		if not _open_socket():
			_handle_drop(1006, "")
			return
	_peer.poll()
	match _peer.get_ready_state():
		WebSocketPeer.STATE_OPEN:
			if not _hello_sent:
				_hello_sent = true
				_send({
					"op": "HELLO",
					"protocol": PROTOCOL_VERSION,
					"schema_hash": _schema_hash,
					"opts": _opts,
				})
			while _peer.get_available_packet_count() > 0:
				var packet := _peer.get_packet()
				if not _peer.was_string_packet():
					_emit_error("BAD_FRAME", "JSON mode received a binary frame", -1)
					continue
				_handle_text(packet.get_string_from_utf8())
			if _peer != null and now - _last_ping_ms >= PING_INTERVAL_MS:
				_last_ping_ms = now
				_peer.send_text("hearth:ping")
			_flush(now)
		WebSocketPeer.STATE_CLOSED:
			var code := _peer.get_close_code()
			var reason := _peer.get_close_reason()
			_peer = null
			# A socket that never close-framed reports an invalid code.
			_handle_drop(code if code >= 1000 and code <= 4999 else 1006, reason)


## Close codes whose ERROR frame some WebSocket stacks (Godot's among them)
## drop when it arrives in the same read as the close (spec section 9).
const _CLOSE_ERRORS := {
	4000: "BAD_FRAME", 4001: "SCHEMA_MISMATCH", 4002: "ROOM_FULL", 4003: "ROOM_LOCKED",
	4005: "RATE_LIMITED", 4006: "FRAME_TOO_LARGE", 4007: "STATE_INCOMPATIBLE", 4010: "JOIN_REJECTED",
}
## Whether this socket delivered an ERROR frame.
var _saw_error := false


func _open_socket() -> bool:
	_retry_at_ms = -1
	_hello_sent = false
	_saw_error = false
	_last_ping_ms = Time.get_ticks_msec()
	_peer = WebSocketPeer.new()
	var error_code := _peer.connect_to_url(_url)
	if error_code != OK:
		_peer = null
		_emit_error("BAD_FRAME", "WebSocketPeer.connect_to_url failed (%d)" % error_code, -1)
		return false
	return true


func _handle_drop(code: int, reason: String) -> void:
	if not _saw_error and _CLOSE_ERRORS.has(code):
		_emit_error(_CLOSE_ERRORS[code], reason, -1)
	_awaiting_join = true
	_report_unconfirmed()
	if _first_drop_ms < 0:
		_first_drop_ms = Time.get_ticks_msec()
	var now := Time.get_ticks_msec()
	var retry_at: int = Reconnect.decide(code, _first_drop_ms, _attempt, now)
	if retry_at < 0:
		_cancel_reconnect()
		disconnected.emit(code, reason)
		return
	_resuming = not _player_id.is_empty()
	_retry_at_ms = retry_at
	reconnecting.emit(_attempt, retry_at - now)
	_attempt += 1


func _report_unconfirmed() -> void:
	if _in_flight.is_empty():
		return
	var seqs: Array = _in_flight.duplicate()
	seqs.sort()
	_in_flight.clear()
	intents_unconfirmed.emit(seqs)


func _cancel_reconnect() -> void:
	_first_drop_ms = -1
	_attempt = 0
	_retry_at_ms = -1
	_resuming = false


func _flush(now: int) -> void:
	if _awaiting_join or _pending.is_empty():
		return
	if now - _last_send_ms < int(1000 / MAX_MSGS_PER_SEC):
		return
	var batch := _pending.slice(0, mini(MAX_INTENTS_PER_MSG, _pending.size()))
	_pending = _pending.slice(batch.size())
	_last_send_ms = now
	for message in batch:
		_in_flight.append(int(message.get("seq", 0)))
	_send({"op": "INTENT", "intents": batch})


func _send(frame: Dictionary) -> void:
	if _peer == null:
		return
	var error_code := _peer.send_text(JSON.stringify(frame))
	if error_code != OK:
		_emit_error("BAD_FRAME", "WebSocketPeer.send_text failed (%d)" % error_code, -1)


func _handle_text(text: String) -> void:
	# Keepalive frames are not protocol frames.
	if text == "hearth:pong" or text == "hearth:ping":
		return
	var frame = JSON.parse_string(text)
	if typeof(frame) != TYPE_DICTIONARY:
		_emit_error("BAD_FRAME", "server sent invalid JSON", -1)
		return
	match String(frame.get("op", "")):
		"JOIN_OK":
			_player_id = String(frame.get("player_id", ""))
			_awaiting_join = false
			var was_resuming := _resuming
			_cancel_reconnect()
			joined.emit(_player_id, int(frame.get("seat", 0)))
			if was_resuming:
				reconnected.emit()
		"FULL_STATE":
			_state = frame.get("state", {})
			_state_seq = int(frame.get("seq", 0))
			_desynced = false
			# The state was replaced: old snapshots must not leak into it.
			_last_patch_t = -1
			_last_tick = -1
			for interp in _interpolators:
				interp.reset()
			state_reset.emit()
		"PATCH":
			_handle_patch(frame)
		"ACK":
			for seq in frame.get("intent_seqs", []):
				_in_flight.erase(int(seq))
			acked.emit(int(frame.get("state_seq", 0)), frame.get("intent_seqs", []))
		"EVENT":
			_emit_server_event(String(frame.get("name", "")), frame.get("data"))
		"ERROR":
			var ref_seq := -1
			if frame.get("ref_seq") != null:
				ref_seq = int(frame["ref_seq"])
				_in_flight.erase(ref_seq)
			_saw_error = true
			_emit_error(String(frame.get("code", "INTERNAL")), String(frame.get("message", "")), ref_seq)
		_:
			# A frame from a newer protocol revision: ignored, no error and no
			# close (spec/protocol-v1.md section 12).
			pass


func _emit_server_event(event_name: String, data: Variant) -> void:
	# Framework-owned names are reserved: they become dedicated signals and
	# never reach the generic `event` signal.
	if event_name == "hearth.handoff":
		var payload: Dictionary = data if typeof(data) == TYPE_DICTIONARY else {}
		handoff.emit(
			String(payload.get("sid", "")),
			String(payload.get("endpoint", "")),
			String(payload.get("ticket", "")),
		)
	elif event_name == "hearth.handoff_ended":
		var payload: Dictionary = data if typeof(data) == TYPE_DICTIONARY else {}
		handoff_ended.emit(String(payload.get("sid", "")), String(payload.get("reason", "")))
	elif not event_name.begins_with("hearth."):
		event.emit(event_name, data)


func _handle_patch(frame: Dictionary) -> void:
	var seq := int(frame.get("seq", 0))
	# Event-mode patches advance `seq`. Ticked patches carry a `tick` and repeat
	# the persisted `seq` until the next persist, so they are ordered by `tick`
	# instead. After a desync we wait for the next FULL_STATE.
	var tick = frame.get("tick")
	var stale := false
	if typeof(tick) == TYPE_INT or typeof(tick) == TYPE_FLOAT:
		tick = int(tick)
		stale = _last_tick >= 0 and tick <= _last_tick
		_last_tick = tick
	else:
		stale = seq <= _state_seq
	if _desynced or stale:
		return
	var ops = frame.get("ops", [])
	if typeof(ops) != TYPE_ARRAY:
		_desync("PATCH has no ops array")
		return
	var next_state = _state
	for op in ops:
		if typeof(op) != TYPE_DICTIONARY:
			_desync("patch op is not an object")
			return
		next_state = Patch.apply(next_state, op)
		if next_state == null:
			_desync("patch path does not apply")
			return
	_state = next_state
	_state_seq = seq
	# Ticked patches carry the server clock; feed interpolators before the
	# signal so game code can sample immediately in its `patched` handler.
	var patch_t = frame.get("t")
	if typeof(patch_t) == TYPE_INT or typeof(patch_t) == TYPE_FLOAT:
		_last_patch_t = int(patch_t)
		_feed_interpolators()
	patched.emit(ops)


func _desync(message: String) -> void:
	_desynced = true
	_emit_error("DESYNC", message, -1)


func _emit_error(code: String, message: String, ref_seq: int) -> void:
	error.emit(code, message, ref_seq)
