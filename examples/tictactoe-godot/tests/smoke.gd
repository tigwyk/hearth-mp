extends SceneTree
## End-to-end smoke test for the Hearth Godot client (plan Task 7 Step 5).
##
## Needs the sample server (`cd examples/tictactoe; npx wrangler dev`) and the
## typed classes generated from the server crate:
##
##   hearth codegen --godot examples/tictactoe-godot/hearth --manifest-path examples/tictactoe/Cargo.toml
##
## Two clients log in, join the same room, X wins the top row, and both typed
## state views must agree.
##
##   $env:HEARTH_URL="http://127.0.0.1:8787"
##   godot --headless --path examples/tictactoe-godot -s tests/smoke.gd
##
## Set HEARTH_FORCE_WEB=1 to run the same test through the pure-GDScript backend.
## The schema hash comes from the generated state class, so `HEARTH_SCHEMA` is no
## longer needed (it stays supported as an optional override).

const HearthClient = preload("res://addons/hearth/hearth_client.gd")
const ROOM_TYPE := "tictactoe"
const TIMEOUT_SECONDS := 20.0
const WAIT_MS := 8000

var _a: Node = null
var _b: Node = null
var _acks := {}
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
	var schema_override := OS.get_environment("HEARTH_SCHEMA")

	_a = HearthClient.new()
	_b = HearthClient.new()
	# Two clients on one machine must not share a device id, or the gateway
	# would assign both the same anonymous player and the second join would
	# replace the first socket.
	var run_id := Time.get_ticks_usec()
	for client in [_a, _b]:
		client.server_url = url
		client.use_schema(TicTacToeBoard)
		if not schema_override.is_empty():
			client.schema_hash = schema_override
		client.device_id_override = "smoke-%d-%s" % [run_id, "a" if client == _a else "b"]
		client.error.connect(_on_error)
		root.add_child(client)
	_a.acked.connect(_on_acked.bind(_a))
	_b.acked.connect(_on_acked.bind(_b))
	# Let the tree finish initialising before login: HTTPRequest needs its node
	# inside the tree.
	await process_frame

	await _a.login_anonymous()
	if _failure != "":
		return
	await _b.login_anonymous()
	if _failure != "":
		return
	if _a.player_id.is_empty() or _b.player_id.is_empty():
		_fail("login_anonymous returned no player id")
		return

	var room: String = await _a.create_room(ROOM_TYPE)
	if room.is_empty():
		_fail("create_room returned no room name")
		return
	_a.join(ROOM_TYPE, room)
	_b.join(ROOM_TYPE, room)

	if not await _wait_until(func() -> bool: return _variant(_a) == TicTacToeStatus.PLAYING):
		_fail("game did not start (status %s)" % _status_text(_a))
		return

	# First joiner is X. Read the typed marks map so a connection race cannot
	# make the test depend on join order.
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

	# Assert through the typed API on both mirrors: enum variant/value, the
	# typed marks map and the leaf turn.
	for client in [_a, _b]:
		var status: TicTacToeStatus = _status(client)
		if status == null or status.variant != TicTacToeStatus.WON:
			_fail("typed status is not Won on one client: %s" % _status_text(client))
			return
		if String(status.value) != String(x_client.player_id):
			_fail("typed winner is %s, expected %s" % [String(status.value), x_client.player_id])
			return
		if _mark(client, x_client.player_id) != 1 or _mark(client, o_client.player_id) != 2:
			_fail(
				"typed marks are wrong: x=%d o=%d"
				% [_mark(client, x_client.player_id), _mark(client, o_client.player_id)]
			)
			return
		if String(client.state.turn) != String(x_client.player_id):
			_fail("typed turn is %s, expected winner %s" % [String(client.state.turn), x_client.player_id])
			return

	_finished = true
	print("smoke: ok (room %s, winner %s, backend %s)" % [room, x_client.player_id, _backend_name()])
	quit(0)


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


func _mark(client, player_id: String) -> int:
	var state = client.state
	if state == null or state is Dictionary or state.marks == null:
		return 0
	return int(state.marks.get_item(player_id))


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


func _on_error(code: String, message: String, ref_seq: int) -> void:
	_fail("server error %s: %s (ref_seq %d)" % [code, message, ref_seq])


func _fail(reason: String) -> void:
	if _finished:
		return
	_finished = true
	printerr("smoke FAILED: " + reason)
	quit(1)


func _backend_name() -> String:
	if OS.has_feature("web") or OS.get_environment("HEARTH_FORCE_WEB") == "1":
		return "web"
	return "native"
