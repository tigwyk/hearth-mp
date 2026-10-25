extends Node
## Hearth autoload (`Hearth`): the public client API for game code.
##
## Picks a backend at startup and forwards everything to it:
## - desktop/native builds use the `HearthNative` GDExtension class (MsgPack
##   over Godot's WebSocketPeer);
## - web exports (or `HEARTH_FORCE_WEB=1`) use `hearth_web_client.gd`, a
##   pure-GDScript JSON client so no native binary is needed in the browser.
##
## The backend is instantiated by class name on purpose: a web export never
## parses a `HearthNative` reference that does not exist there.

signal joined(player_id: String, seat: int)
signal state_reset()
signal patched(ops: Array)
signal acked(state_seq: int, intent_seqs: Array)
signal event(name: String, data: Variant)
signal error(code: String, message: String, ref_seq: int)
signal disconnected(code: int, reason: String)

## Gateway base URL, for example `http://127.0.0.1:8787`.
@export var server_url: String = "http://127.0.0.1:8787"
## 16-char hex schema hash of the room's state type (see `GET /schema/<type>`).
@export var schema_hash: String = ""

## Local view of the room state (sync fields only); `{}` before join.
var state: Dictionary = {}
## Verified player id from `JOIN_OK`; `""` before login.
var player_id: String = ""
## Assigned seat from `JOIN_OK`; `-1` before join.
var seat: int = -1
## Test seam: when set, `login_anonymous()` uses this as the device id instead of
## the platform/random one. Needed to run two clients on one machine (smoke test).
var device_id_override: String = ""

var _backend = null
var _token: String = ""

const _WEB_CLIENT = preload("res://addons/hearth/hearth_web_client.gd")
const _DEVICE_ID_PATH := "user://hearth_device_id"


func _ready() -> void:
	_backend = _create_backend()
	if _backend == null:
		return
	_backend.joined.connect(_on_joined)
	_backend.state_reset.connect(_on_state_reset)
	_backend.patched.connect(_on_patched)
	_backend.acked.connect(_on_acked)
	_backend.event.connect(_on_event)
	_backend.error.connect(_on_error)
	_backend.disconnected.connect(_on_disconnected)
	add_child(_backend)


func _create_backend() -> Node:
	if OS.has_feature("web") or OS.get_environment("HEARTH_FORCE_WEB") == "1":
		return _WEB_CLIENT.new()
	var instance = ClassDB.instantiate("HearthNative")
	if instance == null:
		push_error(
			"Hearth: HearthNative is not registered. Build the extension " +
			"(cargo build -p hearth-godot --release, copy the library into " +
			"addons/hearth/bin/) and import the project once."
		)
		return null
	return instance


## Anonymous login against the gateway. Awaits the HTTP round trip and stores
## the token and player id. Device identity: `OS.get_unique_id()`, else a random
## id persisted to `user://hearth_device_id`.
func login_anonymous() -> void:
	_token = ""
	player_id = ""
	var response: Dictionary = await _post("/auth/anonymous", JSON.stringify({"device_id": _device_id()}), false)
	if response.is_empty():
		return
	var parsed = JSON.parse_string(String(response.get("body", "")))
	if typeof(parsed) != TYPE_DICTIONARY or String(parsed.get("token", "")).is_empty():
		_emit_error("AUTH_FAILED", "POST /auth/anonymous did not return a token", -1)
		return
	_token = String(parsed["token"])
	player_id = String(parsed.get("player_id", ""))


## Mint a room name for `room_type`. Awaits the HTTP round trip and returns the
## name, or `""` on failure (an `error` signal is emitted).
func create_room(room_type: String) -> String:
	var response: Dictionary = await _post("/rooms/" + room_type, "", true)
	if response.is_empty():
		return ""
	var parsed = JSON.parse_string(String(response.get("body", "")))
	if typeof(parsed) != TYPE_DICTIONARY:
		_emit_error("BAD_RESPONSE", "POST /rooms/%s did not return JSON" % room_type, -1)
		return ""
	return String(parsed.get("room", ""))


## Connect to a room. `opts` is passed through to the room's `on_join`.
func join(room_type: String, room: String, opts := {}) -> void:
	if _backend == null:
		_emit_error("NO_BACKEND", "Hearth backend is not available", -1)
		return
	if _token.is_empty():
		_emit_error("NOT_LOGGED_IN", "call login_anonymous() first", -1)
		return
	_backend.connect_to_room(_room_url(room_type, room), schema_hash, opts)


## Queue one intent. Returns its client-assigned sequence number (>= 1), or 0
## when the intent could not be converted.
func send_intent(intent) -> int:
	if _backend == null:
		return 0
	return _backend.send_intent(intent)


## Send `LEAVE` and close the socket.
func leave() -> void:
	if _backend != null:
		_backend.leave()


func _room_url(room_type: String, room: String) -> String:
	var base := server_url.trim_suffix("/")
	if base.begins_with("https://"):
		base = "wss://" + base.substr(8)
	elif base.begins_with("http://"):
		base = "ws://" + base.substr(7)
	return base + "/rooms/" + room_type + "/" + room + "?token=" + _token


func _post(path: String, body: String, authenticated: bool) -> Dictionary:
	var request := HTTPRequest.new()
	add_child(request)
	var headers := PackedStringArray(["Content-Type: application/json"])
	if authenticated:
		headers.append("Authorization: Bearer " + _token)
	var error_code := request.request(server_url.trim_suffix("/") + path, headers, HTTPClient.METHOD_POST, body)
	if error_code != OK:
		request.queue_free()
		_emit_error("HTTP_FAILED", "HTTPRequest.request failed (%d)" % error_code, -1)
		return {}
	var completed: Array = await request.request_completed
	request.queue_free()
	var result: int = completed[0]
	var status: int = completed[1]
	var text: String = (completed[3] as PackedByteArray).get_string_from_utf8()
	if result != HTTPRequest.RESULT_SUCCESS:
		_emit_error("HTTP_FAILED", "HTTPRequest failed (result %d)" % result, -1)
		return {}
	if status < 200 or status >= 300:
		_emit_error("HTTP_%d" % status, text, -1)
		return {}
	return {"status": status, "body": text}


func _device_id() -> String:
	if not device_id_override.is_empty():
		return device_id_override
	var unique_id := OS.get_unique_id()
	if not unique_id.is_empty():
		return unique_id
	if FileAccess.file_exists(_DEVICE_ID_PATH):
		var saved := FileAccess.get_file_as_string(_DEVICE_ID_PATH).strip_edges()
		if not saved.is_empty():
			return saved
	var generated: String = Crypto.new().generate_random_bytes(16).hex_encode()
	var file := FileAccess.open(_DEVICE_ID_PATH, FileAccess.WRITE)
	if file != null:
		file.store_string(generated)
		file.close()
	return generated


func _refresh_state() -> void:
	if _backend != null:
		state = _backend.get_state()


func _on_joined(joined_player_id: String, joined_seat: int) -> void:
	player_id = joined_player_id
	seat = joined_seat
	joined.emit(joined_player_id, joined_seat)


func _on_state_reset() -> void:
	_refresh_state()
	state_reset.emit()


func _on_patched(ops: Array) -> void:
	_refresh_state()
	patched.emit(ops)


func _on_acked(state_seq: int, intent_seqs: Array) -> void:
	acked.emit(state_seq, intent_seqs)


func _on_event(name: String, data: Variant) -> void:
	event.emit(name, data)


func _on_error(code: String, message: String, ref_seq: int) -> void:
	error.emit(code, message, ref_seq)


func _on_disconnected(code: int, reason: String) -> void:
	disconnected.emit(code, reason)


func _emit_error(code: String, message: String, ref_seq: int) -> void:
	error.emit(code, message, ref_seq)
