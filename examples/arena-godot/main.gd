extends Node2D
## Hearth Arena prediction sample (plan v0.4 Task 8).
##
## Your own avatar is drawn from the predictor, so a key press moves it on the
## next frame. Other avatars come from the snapshot interpolator, so they stay
## smooth at 20 Hz. Contact between avatars is resolved by the server, which is
## what makes being blocked show the correction: the drawn position glides
## (15%/frame) to the corrected prediction, and snaps only above 2 units.
##
## Needs the arena gateway and the custom GDExtension:
##
##   cd examples/arena; npx wrangler dev
##   pwsh -File examples/arena-godot-ext/build.ps1
##   $env:HEARTH_URL="http://127.0.0.1:8787"
##   godot --path examples/arena-godot
##
## Set HEARTH_ROOM to join an existing room (a second window); otherwise this
## window creates one and prints its name. WASD or the arrow keys move.

const TICK_SECONDS := 1.0 / 20.0
const SMOOTH_PER_FRAME := 0.15
const SNAP_DISTANCE := 2.0
const UNITS_TO_PIXELS := 26.0
const ARENA_MAX := 20.0
const MARGIN := Vector2(20.0, 20.0)
const SELF_COLOR := Color(0.35, 0.85, 0.55)
const OTHER_COLOR := Color(0.9, 0.55, 0.25)
const COIN_COLOR := Color(0.95, 0.85, 0.3)
const ARENA_COLOR := Color(0.55, 0.6, 0.7)
const HUD_COLOR := Color(0.85, 0.88, 0.92)

var _predictor = null
var _interpolator = null
var _drawn_self := Vector2.ZERO
var _has_drawn := false
var _send_accum := 0.0
var _last_dir := Vector2i.ZERO
var _status := "starting"


func _ready() -> void:
	Hearth.error.connect(_on_error)
	Hearth.joined.connect(_on_joined, CONNECT_ONE_SHOT)
	await _connect()


func _process(delta: float) -> void:
	if _predictor != null:
		_send_accum += delta
		if _send_accum >= TICK_SECONDS:
			_send_accum -= TICK_SECONDS
			_send_move(_input_dir())
		_update_drawn()
	queue_redraw()


func _draw() -> void:
	var side := ARENA_MAX * UNITS_TO_PIXELS
	draw_rect(Rect2(MARGIN, Vector2(side, side)), ARENA_COLOR, false, 2.0)
	for coin in Hearth.raw_state.get("coins", []):
		var coin_pos = _to_vector(coin)
		if coin_pos != null:
			draw_circle(_to_point(coin_pos), 3.0, COIN_COLOR)
	var avatars = Hearth.raw_state.get("avatars", {})
	if typeof(avatars) == TYPE_DICTIONARY and _interpolator != null:
		for player_id in avatars:
			if player_id == Hearth.player_id:
				continue
			var other: Vector2 = _interpolator.sample_v2(["avatars", player_id, "pos"])
			draw_circle(_to_point(other), 0.4 * UNITS_TO_PIXELS, OTHER_COLOR)
			draw_string(
				ThemeDB.fallback_font,
				_to_point(other) + Vector2(10, -10),
				String(player_id).left(6),
				HORIZONTAL_ALIGNMENT_LEFT,
				-1,
				12,
				HUD_COLOR,
			)
	if _has_drawn:
		draw_circle(_to_point(_drawn_self), 0.5 * UNITS_TO_PIXELS, SELF_COLOR)
	var hud := "%s   (WASD / arrows move)" % _status
	draw_string(
		ThemeDB.fallback_font,
		Vector2(MARGIN.x, MARGIN.y + side + 26.0),
		hud,
		HORIZONTAL_ALIGNMENT_LEFT,
		-1,
		14,
		HUD_COLOR,
	)


func _connect() -> void:
	var url := OS.get_environment("HEARTH_URL")
	if not url.is_empty():
		Hearth.server_url = url
	_status = "logging in"
	await Hearth.login_anonymous()
	if Hearth.player_id.is_empty():
		_status = "login failed"
		return
	_status = "reading the schema"
	Hearth.schema_hash = await _fetch_schema()
	if Hearth.schema_hash.is_empty():
		_status = "could not read /schema/arena"
		return
	var room := OS.get_environment("HEARTH_ROOM")
	if room.is_empty():
		_status = "creating a room"
		room = await Hearth.create_room("arena")
		if room.is_empty():
			_status = "could not create a room"
			return
		print("arena room: ", room, "  (second window: HEARTH_ROOM=", room, ")")
	_status = "joining " + room
	Hearth.join("arena", room)


func _fetch_schema() -> String:
	var http := HTTPRequest.new()
	add_child(http)
	var error := http.request(Hearth.server_url.rstrip("/") + "/schema/arena")
	if error != OK:
		http.queue_free()
		return ""
	var response: Array = await http.request_completed
	http.queue_free()
	if int(response[1]) != 200:
		return ""
	var body = JSON.parse_string(String(response[3].get_string_from_utf8()))
	if typeof(body) != TYPE_DICTIONARY:
		return ""
	return String(body.get("schema_hash", ""))


func _on_joined(_player_id: String, _seat: int) -> void:
	_status = "joined"
	# RefCounted: hold the reference on the scene node to keep it alive.
	_predictor = ClassDB.instantiate("ArenaPredictor")
	_predictor.attach(Hearth)
	_interpolator = Hearth.interpolator([["avatars", "*", "pos"]])
	_has_drawn = false


func _on_error(code: String, message: String, _ref_seq: int) -> void:
	_status = "%s: %s" % [code, message]
	push_warning(_status)


func _input_dir() -> Vector2i:
	var dir := Vector2i.ZERO
	if Input.is_key_pressed(KEY_RIGHT) or Input.is_key_pressed(KEY_D):
		dir.x += 1
	if Input.is_key_pressed(KEY_LEFT) or Input.is_key_pressed(KEY_A):
		dir.x -= 1
	if Input.is_key_pressed(KEY_DOWN) or Input.is_key_pressed(KEY_S):
		dir.y += 1
	if Input.is_key_pressed(KEY_UP) or Input.is_key_pressed(KEY_W):
		dir.y -= 1
	return dir


func _send_move(dir: Vector2i) -> void:
	# A held key is one intent per tick; releasing sends a single stop.
	if dir == Vector2i.ZERO and _last_dir == Vector2i.ZERO:
		return
	_last_dir = dir
	var seq: int = Hearth.send_intent({"Move": {"dir": [dir.x, dir.y]}})
	if seq > 0:
		_predictor.input(seq, {"dir": [dir.x, dir.y]})


func _update_drawn() -> void:
	var predicted = _self_position(_predictor.predicted_state())
	if predicted == null:
		return
	if not _has_drawn:
		_drawn_self = predicted
		_has_drawn = true
		return
	var authoritative = _self_position(Hearth.raw_state)
	if authoritative != null and _drawn_self.distance_to(authoritative) > SNAP_DISTANCE:
		_drawn_self = predicted
	else:
		_drawn_self = _drawn_self.lerp(predicted, SMOOTH_PER_FRAME)


func _self_position(state: Dictionary) -> Variant:
	var avatars = state.get("avatars")
	if typeof(avatars) != TYPE_DICTIONARY:
		return null
	var avatar = avatars.get(Hearth.player_id)
	if typeof(avatar) != TYPE_DICTIONARY:
		return null
	return _to_vector(avatar.get("pos"))


func _to_vector(value) -> Variant:
	if typeof(value) == TYPE_ARRAY and value.size() == 2:
		return Vector2(float(value[0]), float(value[1]))
	if value is Vector2:
		return value
	return null


func _to_point(position: Vector2) -> Vector2:
	return MARGIN + Vector2(position.x, position.y) * UNITS_TO_PIXELS
