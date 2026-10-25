extends SceneTree
## Provider-auth smoke test (plan v0.3b Task 6 Step 1).
##
## Needs the sample server with the JWT provider enabled (the Lead's live
## `wrangler dev` plus its local JWKS) and a token minted by the e2e helper. Run
## the minter from `examples/tictactoe`: `jwks.mjs` resolves its `.jwks-dev.json`
## key file against the current directory.
##
##   $env:HEARTH_TEST_JWT = (Set-Location examples/tictactoe; node jwks.mjs mint-jwt)
##   $env:HEARTH_URL = "http://127.0.0.1:8787"
##   godot --headless --path examples/tictactoe-godot -s tests/auth_smoke.gd
##
## The JWT client logs in (player id must be `jwt_`-namespaced and the display
## name must match the token's `name` claim), plays one game in a private room
## against an anonymous client, then a garbage token must fail with AUTH_REJECTED.
## The room is private on purpose: the JWT player id is stable across runs, so
## matchmaking state from an earlier run must not change the outcome (matchmaking
## has its own smoke test).
##
## Set HEARTH_FORCE_WEB=1 to run the same test through the pure-GDScript backend.

const HearthClient = preload("res://addons/hearth/hearth_client.gd")
const ROOM_TYPE := "tictactoe"
const TIMEOUT_SECONDS := 40.0
const WAIT_MS := 8000

var _a: Node = null  # JWT client
var _b: Node = null  # anonymous opponent
var _acks := {}
var _expected_error := ""
var _last_error := ""
var _failure := ""
var _finished := false


func _initialize() -> void:
	_watchdog()
	await _run()


func _watchdog() -> void:
	await create_timer(TIMEOUT_SECONDS).timeout
	if not _finished:
		_fail("timed out after %.0f s" % TIMEOUT_SECONDS)


func _run() -> void:
	var url := OS.get_environment("HEARTH_URL")
	if url.is_empty():
		_fail("set HEARTH_URL")
		return
	var jwt := OS.get_environment("HEARTH_TEST_JWT")
	if jwt.is_empty():
		_fail("set HEARTH_TEST_JWT to a token from: (cd examples/tictactoe; node jwks.mjs mint-jwt)")
		return
	var run_id := Time.get_ticks_usec()

	_a = HearthClient.new()
	_b = HearthClient.new()
	for client in [_a, _b]:
		client.server_url = url
		client.use_schema(TicTacToeBoard)
		client.error.connect(_on_error)
		root.add_child(client)
	_a.acked.connect(_on_acked.bind(_a))
	_b.acked.connect(_on_acked.bind(_b))
	# The anonymous opponent must not share the JWT client's device id.
	_b.device_id_override = "auth-%d-b" % run_id
	await process_frame

	if not await _a.login_jwt(jwt):
		if _failure != "":
			return
		_fail("login_jwt returned false")
		return
	if not _a.player_id.begins_with("jwt_"):
		_fail("JWT player id is not jwt_-namespaced: %s" % _a.player_id)
		return
	_ok("login_jwt accepted the token (player %s)" % _a.player_id)

	var claim_name := _jwt_name(jwt)
	if _a.display_name != claim_name:
		_fail("display_name is %s, the token's name claim is %s" % [_a.display_name, claim_name])
		return
	_ok('display_name matches the token name claim ("%s")' % claim_name)

	await _b.login_anonymous()
	if _failure != "":
		return
	if _b.player_id.is_empty() or not _b.player_id.begins_with("anon_"):
		_fail("anonymous opponent login failed (player %s)" % _b.player_id)
		return
	_ok("anonymous opponent logged in (player %s)" % _b.player_id)

	# Play one game in a private room: the JWT-authenticated token must work for
	# the socket connect and intents. A private room keeps this independent of
	# matchmaker state (the JWT player id is stable across runs).
	var room: String = await _b.create_room(ROOM_TYPE)
	if _failure != "":
		return
	if room.is_empty():
		_fail("create_room returned no room name")
		return
	_a.join(ROOM_TYPE, room)
	_b.join(ROOM_TYPE, room)
	if not await _wait_until(func() -> bool: return _variant(_a) == TicTacToeStatus.PLAYING):
		_fail("game did not start (status %s)" % _status_text(_a))
		return
	_ok("JWT client joined room %s and the game is playing" % room)

	# First joiner is X; read the marks map so a connection race cannot make the
	# test depend on join order.
	var x_client: Node = _a if _mark(_a, _a.player_id) == 1 else _b
	var o_client: Node = _b if x_client == _a else _a
	if _mark(x_client, x_client.player_id) != 1:
		_fail(
			"could not determine which client is X (marks a=%d b=%d)"
			% [_mark(_a, _a.player_id), _mark(_b, _b.player_id)]
		)
		return

	var moves := [[x_client, 0], [o_client, 3], [x_client, 1], [o_client, 4], [x_client, 2]]
	for move in moves:
		var client: Node = move[0]
		var seq: int = client.send_intent(TicTacToeIntents.place(move[1]))
		if not await _wait_until(func() -> bool: return _has_ack(client, seq)):
			_fail("no ACK for intent seq %d" % seq)
			return
	if not await _wait_until(
		func() -> bool:
			return _won(_a, x_client.player_id) and _won(_b, x_client.player_id)
	):
		_fail(
			"winning status did not reach both clients: a=%s b=%s expected=%s"
			% [_status_text(_a), _status_text(_b), x_client.player_id]
		)
		return
	_ok("played the game to a win as %s" % x_client.player_id)

	# A rejected provider credential is false plus AUTH_REJECTED, not a crash.
	_expected_error = "AUTH_REJECTED"
	var garbage_ok: bool = await _b.login_jwt("garbage")
	_expected_error = ""
	if garbage_ok:
		_fail('login_jwt("garbage") returned true')
		return
	if _last_error != "AUTH_REJECTED":
		_fail('login_jwt("garbage") emitted %s, expected AUTH_REJECTED' % _last_error)
		return
	_ok("login_jwt with a garbage token fails with AUTH_REJECTED")

	_finished = true
	print(
		"auth_smoke: ok (player %s, display_name %s, backend %s)"
		% [_a.player_id, '"%s"' % _a.display_name, _backend_name()]
	)
	quit(0)


## The configured name claim (`HEARTH_JWT_NAME_CLAIM`; `name` by default) of a
## compact JWS, read without verifying the signature (the test only needs to know
## what the server should have copied into display_name).
func _jwt_name(jwt: String) -> String:
	var claim := OS.get_environment("HEARTH_TEST_NAME_CLAIM")
	if claim.is_empty():
		claim = "name"
	var parts := jwt.split(".")
	if parts.size() != 3:
		return ""
	var payload := parts[1].replace("-", "+").replace("_", "/")
	while payload.length() % 4 != 0:
		payload += "="
	var parsed = JSON.parse_string(Marshalls.base64_to_utf8(payload))
	if typeof(parsed) != TYPE_DICTIONARY:
		return ""
	var name = parsed.get(claim)
	return "" if name == null else String(name)


func _status(client) -> TicTacToeStatus:
	var state = client.state
	if state == null or state is Dictionary:
		return null
	return state.status


func _variant(client) -> String:
	var status: TicTacToeStatus = _status(client)
	return "" if status == null else String(status.variant)


func _status_text(client) -> String:
	var status: TicTacToeStatus = _status(client)
	return "" if status == null else "%s(%s)" % [String(status.variant), String(status.value)]


func _won(client, winner_id: String) -> bool:
	var status: TicTacToeStatus = _status(client)
	return status != null and status.variant == TicTacToeStatus.WON and String(status.value) == winner_id


func _marks(client) -> Dictionary:
	var state = client.state
	if state == null or state is Dictionary or state.marks == null:
		return {}
	var marks := {}
	for player_id in state.marks.keys():
		marks[String(player_id)] = int(state.marks.get_item(player_id))
	return marks


func _mark(client, player_id: String) -> int:
	return int(_marks(client).get(player_id, 0))


func _on_acked(_state_seq: int, intent_seqs: Array, client: Node) -> void:
	_acks[client] = intent_seqs


func _has_ack(client: Node, seq: int) -> bool:
	# The JSON backend parses numbers as floats, so compare numerically.
	for acked_seq in _acks.get(client, []):
		if int(acked_seq) == seq:
			return true
	return false


func _wait_until(condition: Callable) -> bool:
	var deadline := Time.get_ticks_msec() + WAIT_MS
	while Time.get_ticks_msec() < deadline:
		if _failure != "":
			return false
		if condition.call():
			return true
		await create_timer(0.02).timeout
	return false


func _ok(description: String) -> void:
	print("ok - " + description)


func _on_error(code: String, message: String, ref_seq: int) -> void:
	# One expected failure (the garbage JWT) is part of the test, not a failure.
	if not _expected_error.is_empty() and code == _expected_error:
		_last_error = code
		return
	_fail("server error %s: %s (ref_seq %d)" % [code, message, ref_seq])


func _fail(reason: String) -> void:
	if _finished:
		return
	_finished = true
	printerr("auth_smoke FAILED: " + reason)
	quit(1)


func _backend_name() -> String:
	if OS.has_feature("web") or OS.get_environment("HEARTH_FORCE_WEB") == "1":
		return "web"
	return "native"
