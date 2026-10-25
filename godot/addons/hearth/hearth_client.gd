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
##
## With `use_schema()` the autoload also keeps a typed state mirror (see
## `hearth_typed.gd`): `raw_state` is always the wire Dictionary while `state` is
## the typed root, so v0.1 game code that reads `state` as a Dictionary keeps
## working until it opts in.

signal joined(player_id: String, seat: int)
signal state_reset()
signal patched(ops: Array)
signal acked(state_seq: int, intent_seqs: Array)
signal event(name: String, data: Variant)
signal error(code: String, message: String, ref_seq: int)
## Fires only when the client gives up (deliberate or fatal close, or the
## reconnect grace window is spent). Recoverable drops emit `reconnecting`.
signal disconnected(code: int, reason: String)
## A retry is scheduled. `attempt` is 0-based; `retry_in_ms` is from now.
signal reconnecting(attempt: int, retry_in_ms: int)
## A retry joined the room again.
signal reconnected()
## Intents sent before a drop whose fate the server may or may not have decided.
## Reported once per drop, sorted ascending; never resent.
signal intents_unconfirmed(seqs: Array)

## Gateway base URL, for example `http://127.0.0.1:8787`.
@export var server_url: String = "http://127.0.0.1:8787"
## 16-char hex schema hash of the room's state type (see `GET /schema/<type>`).
@export var schema_hash: String = ""

## Raw view of the room state (sync fields only); `{}` before join. This is what
## v0.1 exposed as `state`; patches are applied to it in place by `HearthPatch`.
var raw_state: Dictionary = {}
## The typed state root once `use_schema()` has been called, otherwise
## `raw_state` (the v0.1 behaviour: a plain Dictionary).
var state = {}
## Verified player id from `JOIN_OK`; `""` before login.
var player_id: String = ""
## Display name from the last provider login; `""` when the provider gave none.
var display_name: String = ""
## Assigned seat from `JOIN_OK`; `-1` before join.
var seat: int = -1
## Test seam: when set, `login_anonymous()` uses this as the device id instead of
## the platform/random one. Needed to run two clients on one machine (smoke test).
var device_id_override: String = ""

var _backend = null
var _typed_root = null
var _token: String = ""

const _WEB_CLIENT = preload("res://addons/hearth/hearth_web_client.gd")
const _PATCH = preload("res://addons/hearth/hearth_patch.gd")
const _DEVICE_ID_PATH := "user://hearth_device_id"
## `POST /match` attempts per `quick_join`: the first try plus 3 retries.
const _MATCH_ATTEMPTS := 4
## How long `quick_join` waits for a matchmade room's JOIN_OK or ERROR.
const _JOIN_TIMEOUT_MS := 10000
## HTTP attempts per call when the gateway answers 429: the first try plus 2.
const _MAX_ATTEMPTS_PER_REQUEST := 3


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
	_backend.reconnecting.connect(_on_reconnecting)
	_backend.reconnected.connect(_on_reconnected)
	_backend.intents_unconfirmed.connect(_on_intents_unconfirmed)
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
	display_name = ""
	var response: Dictionary = await _post("/auth/anonymous", JSON.stringify({"device_id": _device_id()}), false)
	if response.is_empty():
		return
	var parsed = JSON.parse_string(String(response.get("body", "")))
	if typeof(parsed) != TYPE_DICTIONARY or String(parsed.get("token", "")).is_empty():
		_emit_error("AUTH_FAILED", "POST /auth/anonymous did not return a token", -1)
		return
	_token = String(parsed["token"])
	player_id = String(parsed.get("player_id", ""))


## Sign in with a signed JWT from the game's own backend (`POST /auth/jwt`).
func login_jwt(jwt: String) -> bool:
	return await _login("jwt", "jwt", jwt)


## Sign in with a Steam `GetAuthTicketForWebApi` session ticket, hex-encoded.
func login_steam(ticket_hex: String) -> bool:
	return await _login("steam", "ticket", ticket_hex)


## Sign in with an itch.io API key (`ITCHIO_API_KEY` by default).
func login_itch(api_key := OS.get_environment("ITCHIO_API_KEY")) -> bool:
	return await _login("itch", "api_key", api_key)


## Sign in with a Discord OAuth authorization code.
func login_discord(code: String) -> bool:
	return await _login("discord", "code", code)


## Shared provider login: exchange one external credential for the Hearth token
## every later call uses. Returns false and emits the mapped `error` code on
## failure (401 `AUTH_REJECTED`, 404 `AUTH_DISABLED`, 502 `AUTH_UPSTREAM`, or
## `RATE_LIMITED` once the HTTP helper's retries are used up).
func _login(provider: String, field: String, credential: String) -> bool:
	_token = ""
	player_id = ""
	display_name = ""
	var response: Dictionary = await _post(
		"/auth/" + provider,
		JSON.stringify({field: credential}),
		false,
		{401: "AUTH_REJECTED", 404: "AUTH_DISABLED", 502: "AUTH_UPSTREAM"},
	)
	if response.is_empty():
		return false
	var parsed = JSON.parse_string(String(response.get("body", "")))
	if typeof(parsed) != TYPE_DICTIONARY or String(parsed.get("token", "")).is_empty():
		_emit_error("AUTH_FAILED", "POST /auth/%s did not return a token" % provider, -1)
		return false
	_token = String(parsed["token"])
	player_id = String(parsed.get("player_id", ""))
	var name = parsed.get("display_name")
	display_name = "" if name == null else String(name)
	return true


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


## Quick-join a matchmade room: `POST /match/{room_type}`, then `join()` the
## room it returns. `pool` groups rooms that should be matched together. When the
## room answers `ROOM_FULL` before `joined`, it asks the matchmaker again up to 3
## times, then emits `error("MATCH_FAILED", ...)`. `opts` is passed through to the
## room's `on_join`.
func quick_join(room_type: String, pool := "default", opts := {}) -> void:
	for _attempt in _MATCH_ATTEMPTS:
		var response: Dictionary = await _post(
			"/match/" + room_type, JSON.stringify({"pool": pool}), true
		)
		if response.is_empty():
			return
		var parsed = JSON.parse_string(String(response.get("body", "")))
		var room := ""
		if typeof(parsed) == TYPE_DICTIONARY:
			room = String(parsed.get("room", ""))
		if room.is_empty():
			_emit_error("MATCH_FAILED", "POST /match/%s did not return a room" % room_type, -1)
			return
		var outcome := await _join_and_wait(room_type, room, opts)
		if outcome == "joined":
			return
		if outcome != "ROOM_FULL":
			# The room's own error was already emitted; only a timeout has none.
			if outcome == "TIMEOUT":
				_emit_error("MATCH_FAILED", "joining %s timed out" % room, -1)
			return
	_emit_error("MATCH_FAILED", "no room after %d match attempts" % _MATCH_ATTEMPTS, -1)


## The open rooms of one pool: `GET /lobby/{room_type}`. Each entry is
## `{"name", "players", "capacity", "metadata"}`; `[]` on failure (an `error`
## signal is emitted).
func list_rooms(room_type: String, pool := "default", limit := 20) -> Array:
	var response: Dictionary = await _http_get(
		"/lobby/%s?pool=%s&limit=%d" % [room_type, pool, limit], true
	)
	if response.is_empty():
		return []
	var parsed = JSON.parse_string(String(response.get("body", "")))
	if typeof(parsed) != TYPE_DICTIONARY or typeof(parsed.get("rooms")) != TYPE_ARRAY:
		_emit_error("BAD_RESPONSE", "GET /lobby/%s did not return a rooms array" % room_type, -1)
		return []
	var rooms: Array = []
	for room in parsed["rooms"]:
		if typeof(room) != TYPE_DICTIONARY:
			continue
		rooms.append({
			"name": String(room.get("name", "")),
			"players": int(room.get("players", 0)),
			"capacity": int(room.get("capacity", 0)),
			"metadata": room.get("metadata"),
		})
	return rooms


## The caller's own player data: `GET /me`. Returns the JSON view as a
## Dictionary, or `{}` on failure (an `error` signal is emitted, including
## `HTTP_501` when the player service is not configured).
func get_me() -> Dictionary:
	var response: Dictionary = await _http_get("/me", true)
	if response.is_empty():
		return {}
	var parsed = JSON.parse_string(String(response.get("body", "")))
	if typeof(parsed) != TYPE_DICTIONARY:
		_emit_error("BAD_RESPONSE", "GET /me did not return a JSON object", -1)
		return {}
	return parsed


## Connect to a room. `opts` is passed through to the room's `on_join`.
func join(room_type: String, room: String, opts := {}) -> void:
	if _backend == null:
		_emit_error("NO_BACKEND", "Hearth backend is not available", -1)
		return
	if _token.is_empty():
		_emit_error("NOT_LOGGED_IN", "call login_anonymous() first", -1)
		return
	_backend.connect_to_room(_room_url(room_type, room), schema_hash, opts)


## `join()` and wait for its first outcome: `"joined"`, the error code the room
## answered (`"ROOM_FULL"`, ...), or `"TIMEOUT"`.
func _join_and_wait(room_type: String, room: String, opts: Dictionary) -> String:
	var outcome := {"done": false, "code": ""}
	var on_joined := func(_joined_player_id: String, _joined_seat: int) -> void:
		outcome["done"] = true
	var on_error := func(code: String, _message: String, _ref_seq: int) -> void:
		outcome["done"] = true
		outcome["code"] = code
	joined.connect(on_joined)
	error.connect(on_error)
	join(room_type, room, opts)
	var deadline := Time.get_ticks_msec() + _JOIN_TIMEOUT_MS
	while not bool(outcome["done"]) and Time.get_ticks_msec() < deadline:
		await get_tree().process_frame
	joined.disconnect(on_joined)
	error.disconnect(on_error)
	if not bool(outcome["done"]):
		return "TIMEOUT"
	return "joined" if String(outcome["code"]).is_empty() else String(outcome["code"])


## Enable the typed state mirror (plan v0.2a Task 6). `root_class` is a
## code-generated state class: it extends `HearthObject` and carries the room's
## `SCHEMA_HASH` constant. Call it after `login_anonymous()`/`create_room()` and
## before `join()`; `state` becomes the typed root while `raw_state` keeps the
## plain wire Dictionary.
func use_schema(root_class: GDScript) -> void:
	schema_hash = String(root_class.SCHEMA_HASH)
	_typed_root = root_class.new()
	_typed_root._hearth_load(raw_state)
	state = _typed_root


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


## Test seam: simulate an abrupt socket drop (debug builds only).
func _debug_drop() -> void:
	if _backend != null:
		_backend._debug_drop()


func _room_url(room_type: String, room: String) -> String:
	var base := server_url.trim_suffix("/")
	if base.begins_with("https://"):
		base = "wss://" + base.substr(8)
	elif base.begins_with("http://"):
		base = "ws://" + base.substr(7)
	return base + "/rooms/" + room_type + "/" + room + "?token=" + _token


func _post(
	path: String, body: String, authenticated: bool, error_codes := {}
) -> Dictionary:
	return await _request(HTTPClient.METHOD_POST, path, body, authenticated, error_codes)


func _http_get(path: String, authenticated: bool) -> Dictionary:
	return await _request(HTTPClient.METHOD_GET, path, "", authenticated)


## One HTTP round trip. A 429 is retried at most twice, after the `Retry-After`
## seconds, and only once those are used up emits `RATE_LIMITED`; `error_codes`
## maps other HTTP statuses to the calling endpoint's `error` code
## (`HTTP_<status>` when absent), so callers like the provider logins can report
## `AUTH_REJECTED` instead of `HTTP_401`.
func _request(
	method: HTTPClient.Method,
	path: String,
	body: String,
	authenticated: bool,
	error_codes := {},
) -> Dictionary:
	for attempt in _MAX_ATTEMPTS_PER_REQUEST:
		var request := HTTPRequest.new()
		add_child(request)
		var headers := PackedStringArray(["Content-Type: application/json"])
		if authenticated:
			headers.append("Authorization: Bearer " + _token)
		var error_code := request.request(
			server_url.trim_suffix("/") + path, headers, method, body
		)
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
		if status == 429 and attempt + 1 < _MAX_ATTEMPTS_PER_REQUEST:
			await get_tree().create_timer(_retry_after_seconds(completed[2])).timeout
			continue
		if status < 200 or status >= 300:
			var code := "HTTP_%d" % status
			if status == 429:
				code = "RATE_LIMITED"
			elif error_codes.has(status):
				code = String(error_codes[status])
			_emit_error(code, text, -1)
			return {}
		return {"status": status, "body": text}
	return {}


## The whole seconds in a `Retry-After` response header (1 when absent/bad).
func _retry_after_seconds(headers: PackedStringArray) -> int:
	for header in headers:
		var separator := header.find(":")
		if separator > 0 and header.substr(0, separator).strip_edges().to_lower() == "retry-after":
			return maxi(1, int(header.substr(separator + 1).strip_edges()))
	return 1


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
	if _backend == null:
		return
	var backend_state = _backend.get_state()
	if typeof(backend_state) != TYPE_DICTIONARY:
		return
	# Keep an independent copy: `hearth_patch.gd` mutates the tree in place and
	# the web backend returns its live dictionary.
	raw_state = backend_state.duplicate(true)
	if _typed_root != null:
		_typed_root._hearth_load(raw_state)
	else:
		state = raw_state


func _on_joined(joined_player_id: String, joined_seat: int) -> void:
	player_id = joined_player_id
	seat = joined_seat
	joined.emit(joined_player_id, joined_seat)


func _on_state_reset() -> void:
	_refresh_state()
	state_reset.emit()


func _on_patched(ops: Array) -> void:
	for op in ops:
		var next_state = _PATCH.apply(raw_state, op)
		if typeof(next_state) != TYPE_DICTIONARY:
			# The backend already reported DESYNC; the next FULL_STATE resyncs us.
			break
		raw_state = next_state
		if _typed_root != null:
			HearthObject.route(state, raw_state, op)
	if _typed_root == null:
		state = raw_state
	patched.emit(ops)


func _on_acked(state_seq: int, intent_seqs: Array) -> void:
	acked.emit(state_seq, intent_seqs)


func _on_event(name: String, data: Variant) -> void:
	event.emit(name, data)


func _on_error(code: String, message: String, ref_seq: int) -> void:
	error.emit(code, message, ref_seq)


func _on_disconnected(code: int, reason: String) -> void:
	disconnected.emit(code, reason)


func _on_reconnecting(attempt: int, retry_in_ms: int) -> void:
	reconnecting.emit(attempt, retry_in_ms)


func _on_reconnected() -> void:
	reconnected.emit()


func _on_intents_unconfirmed(seqs: Array) -> void:
	intents_unconfirmed.emit(seqs)


func _emit_error(code: String, message: String, ref_seq: int) -> void:
	error.emit(code, message, ref_seq)
