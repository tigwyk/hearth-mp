extends SceneTree
## Reconnect smoke test (plan v0.2a Task 3 Step 4).
##
## Needs the sample server from the v0.1 plan (`cd examples/tictactoe;
## npx wrangler dev`) and the project imported once so the GDExtension is
## loaded. Two clients join the same room; X plays a move, then drops its socket
## mid-game and queues another move while disconnected. X must resume the same
## seat in place, and that queued move must still be applied.
##
##   $env:HEARTH_URL="http://127.0.0.1:8787"
##   $env:HEARTH_SCHEMA=(Invoke-RestMethod $env:HEARTH_URL/schema/tictactoe).schema_hash
##   godot --headless --path examples/tictactoe-godot -s tests/reconnect_smoke.gd
##
## Set HEARTH_FORCE_WEB=1 to run the same test through the pure-GDScript backend.

const HearthClient = preload("res://addons/hearth/hearth_client.gd")
const ROOM_TYPE := "tictactoe"
const TIMEOUT_SECONDS := 30.0
const WAIT_MS := 8000

var _a: Node = null
var _b: Node = null
var _acks := {}
var _reconnecting := 0
var _reconnected := false
var _gave_up := false
var _unconfirmed := []
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
	var schema := OS.get_environment("HEARTH_SCHEMA")
	if url.is_empty() or schema.is_empty():
		_fail("set HEARTH_URL and HEARTH_SCHEMA")
		return

	_a = HearthClient.new()
	_b = HearthClient.new()
	# Two clients on one machine must not share a device id.
	var run_id := Time.get_ticks_usec()
	for client in [_a, _b]:
		client.server_url = url
		client.schema_hash = schema
		client.device_id_override = "reconnect-%d-%s" % [run_id, "a" if client == _a else "b"]
		client.error.connect(_on_error)
		client.disconnected.connect(_on_disconnected)
		client.reconnecting.connect(_on_reconnecting)
		client.reconnected.connect(_on_reconnected)
		client.intents_unconfirmed.connect(_on_intents_unconfirmed)
		root.add_child(client)
	_a.acked.connect(_on_acked.bind(_a))
	_b.acked.connect(_on_acked.bind(_b))
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

	if not await _wait_until(func() -> bool: return String(_a.state.get("status", "")) == "Playing"):
		_fail("game did not start (status %s)" % _a.state.get("status"))
		return

	# First joiner is X. Read the mark map so a connection race cannot make the
	# test depend on join order.
	var marks: Dictionary = _a.state.get("marks", {})
	var x_client: Node = _a if int(marks.get(_a.player_id, 0)) == 1 else _b
	var o_client: Node = _b if x_client == _a else _a
	if int(marks.get(x_client.player_id, 0)) != 1:
		_fail("could not determine which client is X (marks %s)" % marks)
		return

	# X plays one move, then O moves so the queued move below is X's turn.
	for move in [[x_client, 0], [o_client, 3]]:
		var client: Node = move[0]
		var seq: int = client.send_intent({"Place": {"cell": move[1]}})
		if not await _wait_until(func() -> bool: return _has_ack(client, seq)):
			_fail("no ACK for intent seq %d" % seq)
			return
	if not await _wait_until(func() -> bool: return _cell(o_client, 0) == 1 and _cell(o_client, 3) == 2):
		_fail("both clients did not see the first two moves")
		return

	var seat_before: int = x_client.seat
	var player_before: String = x_client.player_id
	x_client._debug_drop()
	if not await _wait_until(func() -> bool: return _reconnecting > 0):
		_fail("no reconnecting signal after _debug_drop")
		return

	# The socket is gone: this intent must wait for the next JOIN_OK.
	var queued_seq: int = x_client.send_intent({"Place": {"cell": 1}})
	if queued_seq == 0:
		_fail("the queued intent could not be converted")
		return
	if not await _wait_until(func() -> bool: return _reconnected):
		_fail("no reconnected signal within %d ms" % WAIT_MS)
		return
	if not await _wait_until(func() -> bool: return _has_ack(x_client, queued_seq)):
		_fail("queued intent seq %d was not applied after the resume" % queued_seq)
		return
	if not await _wait_until(
		func() -> bool:
			return _cell(o_client, 0) == 1 and _cell(o_client, 1) == 1
	):
		_fail("O's board did not show both X moves (cells %s)" % JSON.stringify(o_client.state.get("cells")))
		return

	if x_client.seat != seat_before or x_client.player_id != player_before:
		_fail(
			"resume changed the seat: %d -> %d (player %s -> %s)"
			% [seat_before, x_client.seat, player_before, x_client.player_id]
		)
		return
	if _gave_up:
		_fail("disconnected fired: the client gave up on a recoverable drop")
		return
	if not _unconfirmed.is_empty():
		_fail("intents were reported unconfirmed although all were ACKed: %s" % str(_unconfirmed))
		return

	_finished = true
	print("reconnect_smoke: ok (room %s, seat %d, backend %s)" % [room, seat_before, _backend_name()])
	quit(0)


func _cell(client: Node, index: int) -> int:
	var cells: Array = client.state.get("cells", [])
	return int(cells[index]) if index < cells.size() else 0


func _on_acked(_state_seq: int, intent_seqs: Array, client: Node) -> void:
	_acks[client] = intent_seqs


func _has_ack(client: Node, seq: int) -> bool:
	# The JSON backend parses numbers as floats, so compare numerically.
	for acked_seq in _acks.get(client, []):
		if int(acked_seq) == seq:
			return true
	return false


func _on_reconnecting(attempt: int, retry_in_ms: int) -> void:
	_reconnecting += 1
	print("reconnect_smoke: reconnecting attempt %d in %d ms" % [attempt, retry_in_ms])


func _on_reconnected() -> void:
	_reconnected = true
	print("reconnect_smoke: reconnected")


func _on_intents_unconfirmed(seqs: Array) -> void:
	_unconfirmed = seqs
	print("reconnect_smoke: intents unconfirmed %s" % str(seqs))


func _on_disconnected(code: int, reason: String) -> void:
	_gave_up = true
	_fail("disconnected %d: %s" % [code, reason])


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
	printerr("reconnect_smoke FAILED: " + reason)
	quit(1)


func _backend_name() -> String:
	if OS.has_feature("web") or OS.get_environment("HEARTH_FORCE_WEB") == "1":
		return "web"
	return "native"
