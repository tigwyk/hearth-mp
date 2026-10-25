extends SceneTree
## Arena prediction smoke test (plan v0.4 Task 8 Step 3).
##
## Needs the arena sample gateway (`cd examples/arena; npx wrangler dev`) and the
## custom arena GDExtension (`pwsh -File examples/arena-godot-ext/build.ps1`):
##
##   $env:HEARTH_URL="http://127.0.0.1:8787"
##   godot --headless --path examples/arena-godot -s tests/predict_smoke.gd
##
## Client A injects 150 ms of one-way latency in both directions, client B none.
## Assertions:
##  - A's prediction moves on the first frame after the input, while the
##    authoritative position has not moved yet;
##  - after A stops, prediction matches the authoritative position within 0.01
##    within 300 ms of the last ACK (the ACK round trip is ~300 ms on its own);
##  - B blocking A's path makes A's prediction overlap B; once the server's
##    correction is ACKed, the prediction is within 0.05 within 200 ms;
##  - a raw `{"Move":{"dir":[5,0]}}` intent still moves A the normal one-tick
##    distance: the server clamps the direction.

const HearthClient = preload("res://addons/hearth/hearth_client.gd")
const ROOM_TYPE := "arena"
const TIMEOUT_SECONDS := 45.0
const LATENCY_MS := 150
const TICK_MS := 50

var _a: Node = null
var _b: Node = null
var _predictor = null
var _finished := false
var _last_seq := 0
var _acked_max := 0


func _initialize() -> void:
	_watchdog()
	await _run()


func _watchdog() -> void:
	await create_timer(TIMEOUT_SECONDS).timeout
	if not _finished:
		_fail("timed out after %.0f s" % TIMEOUT_SECONDS)


func _run() -> void:
	if OS.get_environment("HEARTH_FORCE_WEB") == "1":
		_fail("prediction needs the native backend; unset HEARTH_FORCE_WEB")
		return
	if not ClassDB.class_exists("ArenaPredictor"):
		_fail(
			"ArenaPredictor is not registered: run " +
			"`pwsh -File examples/arena-godot-ext/build.ps1` and import the project"
		)
		return
	var url := OS.get_environment("HEARTH_URL")
	if url.is_empty():
		_fail("set HEARTH_URL to the arena gateway, e.g. http://127.0.0.1:8787")
		return
	# Let the tree finish initialising before creating HTTPRequests.
	await process_frame

	var schema := await _fetch_schema(url)
	if schema.is_empty():
		_fail("could not read %s/schema/%s" % [url.rstrip("/"), ROOM_TYPE])
		return

	_a = HearthClient.new()
	_b = HearthClient.new()
	var run_id := Time.get_ticks_usec()
	_a.device_id_override = "predict-%d-a" % run_id
	_b.device_id_override = "predict-%d-b" % run_id
	for client in [_a, _b]:
		client.server_url = url
		client.schema_hash = schema
		client.error.connect(_on_client_error)
		root.add_child(client)
	_a.acked.connect(_on_a_acked)
	await process_frame

	# A gets 150 ms each way; the backend exists after the client's _ready.
	_a._backend.debug_latency_ms = LATENCY_MS

	await _a.login_anonymous()
	await _b.login_anonymous()
	if _a.player_id.is_empty() or _b.player_id.is_empty():
		_fail("anonymous login failed")
		return

	var room: String = await _a.create_room(ROOM_TYPE)
	if room.is_empty():
		_fail("could not create an arena room")
		return
	_a.join(ROOM_TYPE, room)
	if not await _wait_until(func() -> bool: return _a.seat >= 0, 5000):
		_fail("A did not join")
		return
	_b.join(ROOM_TYPE, room)
	if not await _wait_until(
		func() -> bool: return _avatar_count(_a) >= 2 and _avatar_count(_b) >= 2, 10000
	):
		_fail("the clients never saw both avatars")
		return

	_predictor = ClassDB.instantiate("ArenaPredictor")
	_predictor.attach(_a)
	await process_frame
	if _predicted() == null:
		_fail("predictor has no state after attach (player_id/raw_state missing?)")
		return

	await _check_first_frame_move()
	if _finished:
		return
	await _check_converges_after_stop()
	if _finished:
		return
	await _check_blocked_path()
	if _finished:
		return
	await _check_raw_direction_clamped()
	if _finished:
		return

	print("predict_smoke: all checks passed")
	_finished = true
	quit(0)


# --- checks ---------------------------------------------------------------


func _check_first_frame_move() -> void:
	var predicted_before = _predicted()
	var authoritative_before = _authoritative()
	if predicted_before == null or authoritative_before == null:
		_fail("no position before the first input")
		return
	_send_move([1, 0])
	# `input()` steps the prediction synchronously; no frame has passed, so the
	# server cannot have answered yet.
	var predicted_after = _predicted()
	var authoritative_after = _authoritative()
	if predicted_after == null or authoritative_after == null:
		_fail("no position after the first input")
		return
	if predicted_after.x <= predicted_before.x + 0.1:
		_fail(
			"prediction did not move on the first frame (x %.3f -> %.3f)"
			% [predicted_before.x, predicted_after.x]
		)
		return
	if authoritative_after.x > authoritative_before.x + 0.001:
		_fail("the authoritative state moved before the %d ms round trip" % LATENCY_MS)
		return
	print(
		"ok - prediction moved on the first frame (x %.3f -> %.3f, authoritative still %.3f)"
		% [predicted_before.x, predicted_after.x, authoritative_after.x]
	)


func _check_converges_after_stop() -> void:
	# Nine more ticks while holding right: ten ticks is 2 units at 20 Hz.
	for _tick in 9:
		await create_timer(TICK_MS / 1000.0).timeout
		_send_move([1, 0])
	_send_move([0, 0])
	var stopped_at := Time.get_ticks_msec()
	if not await _wait_until(func() -> bool: return _acked_max >= _last_seq, 3000):
		_fail("the last input was never acknowledged (%d of %d)" % [_acked_max, _last_seq])
		return
	var acked_at := Time.get_ticks_msec()
	if not await _wait_until(func() -> bool: return _converged(0.01), 300):
		_fail("prediction did not match the authoritative position within 300 ms of the last ACK")
		return
	print(
		"ok - prediction converged to the authoritative position (%.0f ms after stop, %.0f ms after the last ACK)"
		% [acked_at - stopped_at, Time.get_ticks_msec() - acked_at]
	)


func _check_blocked_path() -> void:
	var overlapped := false
	var max_divergence := 0.0
	# A starts about 3 units left of B; 30 ticks would move it 6 units, so it
	# runs into B and keeps pushing into the contact.
	for _tick in 30:
		_send_move([1, 0])
		await create_timer(TICK_MS / 1000.0).timeout
		var predicted = _predicted()
		var authoritative = _authoritative()
		var other = _pos_of(_a.raw_state, _b.player_id)
		if predicted != null and authoritative != null:
			max_divergence = maxf(max_divergence, (Vector2(predicted) - Vector2(authoritative)).length())
		if predicted != null and other != null and (Vector2(predicted) - Vector2(other)).length() < 1.0:
			overlapped = true
	_send_move([0, 0])
	if not await _wait_until(func() -> bool: return _acked_max >= _last_seq, 3000):
		_fail("the last blocking input was never acknowledged")
		return
	var acked_at := Time.get_ticks_msec()
	if not await _wait_until(func() -> bool: return _converged(0.05), 200):
		_fail("prediction was not within 0.05 of the authoritative position within 200 ms of the last ACK")
		return
	if not overlapped:
		_fail("A never overlapped B, so the contact correction was not exercised")
		return
	if max_divergence <= 0.05:
		_fail("A's prediction never diverged from the authoritative state")
		return
	print(
		"ok - blocked path: prediction overlapped B, diverged %.2f units, corrected %.0f ms after the last ACK"
		% [max_divergence, Time.get_ticks_msec() - acked_at]
	)


func _check_raw_direction_clamped() -> void:
	# The clamp is a server-side behaviour: drop the injected latency (after
	# letting the queued frames drain) so the measurement is one clean tick.
	_a._backend.debug_latency_ms = 0
	await create_timer(0.5).timeout
	# Move back left, away from B, and let the prediction catch up.
	for _tick in 25:
		_send_move([-1, 0])
		await create_timer(TICK_MS / 1000.0).timeout
	_send_move([0, 0])
	if not await _wait_until(func() -> bool: return _acked_max >= _last_seq, 3000):
		_fail("the last separating input was never acknowledged")
		return
	# Wait until the authoritative position has been still for a bit: an ACK can
	# arrive in the same frame as (or just before) the patch that carries its
	# movement, so the position needs to settle before measuring one tick.
	if not await _wait_stable(2000):
		_fail("A's authoritative position never settled before the raw move")
		return
	var before = _authoritative()
	if before == null:
		_fail("no authoritative position before the raw move")
		return
	# Server-only: no predictor.input, so this checks the server's clamp, not the
	# prediction.
	if _a.send_intent({"Move": {"dir": [5, 0]}}) <= 0:
		_fail("send_intent rejected the raw move")
		return
	var deadline := Time.get_ticks_msec() + 3000
	var after = null
	while Time.get_ticks_msec() < deadline:
		await process_frame
		var now = _authoritative()
		if now != null and absf(now.x - before.x) > 0.02:
			after = now
			break
	if after == null:
		_fail("the server never moved A for the raw [5,0] intent")
		return
	var delta: float = after.x - before.x
	if delta < 0.1 or delta > 0.35:
		_fail(
			"the server moved %.2f units for a clamped [5,0] direction, expected about 0.2 (before %.2f, after %.2f)"
			% [delta, before.x, after.x]
		)
		return
	print("ok - raw [5,0] direction moved A %.2f units, the normal one-tick distance" % delta)


# --- helpers --------------------------------------------------------------


func _on_client_error(code: String, message: String, _ref_seq: int) -> void:
	_fail("client error %s: %s" % [code, message])


func _on_a_acked(_state_seq: int, intent_seqs: Array) -> void:
	for seq in intent_seqs:
		_acked_max = maxi(_acked_max, int(seq))


func _send_move(dir: Array) -> void:
	var seq: int = _a.send_intent({"Move": {"dir": dir}})
	if seq <= 0:
		return
	_last_seq = seq
	if _predictor != null:
		_predictor.input(seq, {"dir": dir})


func _converged(tolerance: float) -> bool:
	var predicted = _predicted()
	var authoritative = _authoritative()
	if predicted == null or authoritative == null:
		return false
	return (Vector2(predicted) - Vector2(authoritative)).length() <= tolerance


func _predicted() -> Variant:
	if _predictor == null:
		return null
	return _pos_of(_predictor.predicted_state(), _a.player_id)


func _authoritative() -> Variant:
	return _pos_of(_a.raw_state, _a.player_id)


func _pos_of(state: Dictionary, player_id: String) -> Variant:
	var avatars = state.get("avatars")
	if typeof(avatars) != TYPE_DICTIONARY:
		return null
	var avatar = avatars.get(player_id)
	if typeof(avatar) != TYPE_DICTIONARY:
		return null
	var pos = avatar.get("pos")
	if typeof(pos) == TYPE_ARRAY and pos.size() == 2:
		return Vector2(float(pos[0]), float(pos[1]))
	return null


func _avatar_count(client: Node) -> int:
	var avatars = client.raw_state.get("avatars")
	if typeof(avatars) != TYPE_DICTIONARY:
		return 0
	return avatars.size()


func _wait_until(predicate: Callable, timeout_ms: int) -> bool:
	var deadline := Time.get_ticks_msec() + timeout_ms
	while Time.get_ticks_msec() < deadline:
		if predicate.call():
			return true
		await process_frame
	return predicate.call()


## Whether A's authoritative position has been still for ~100 ms.
func _wait_stable(timeout_ms: int) -> bool:
	var deadline := Time.get_ticks_msec() + timeout_ms
	var last = _authoritative()
	var stable_since := Time.get_ticks_msec()
	while Time.get_ticks_msec() < deadline:
		await process_frame
		var now = _authoritative()
		if now == null:
			continue
		if last != null and (Vector2(now) - Vector2(last)).length() < 0.001:
			if Time.get_ticks_msec() - stable_since >= 100:
				return true
		else:
			stable_since = Time.get_ticks_msec()
		last = now
	return false


func _fetch_schema(url: String) -> String:
	var http := HTTPRequest.new()
	root.add_child(http)
	var error := http.request(url.rstrip("/") + "/schema/" + ROOM_TYPE)
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


func _fail(message: String) -> void:
	if _finished:
		return
	_finished = true
	printerr("predict_smoke FAILED: " + message)
	quit(1)
