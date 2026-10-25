extends SceneTree
## Matchmaking smoke test (plan v0.3a Task 6 Step 1).
##
## Needs the sample server (`cd examples/tictactoe; npx wrangler dev`) and the
## typed classes generated from the server crate:
##
##   hearth codegen --godot examples/tictactoe-godot/hearth --manifest-path examples/tictactoe/Cargo.toml
##
## Two clients quick-join one matchmade room, play to an X win, and their
## `GET /me` records must show the result. A third client quick-joins, then its
## lobby listing must show the room it was just seated in.
##
##   $env:HEARTH_URL="http://127.0.0.1:8787"
##   godot --headless --path examples/tictactoe-godot -s tests/matchmaking_smoke.gd
##
## Set HEARTH_FORCE_WEB=1 to run the same test through the pure-GDScript backend.
## The pool is unique per run: matchmaker rooms and reservations left behind by
## an earlier run live in their own pool and cannot change this run's outcome.

const HearthClient = preload("res://addons/hearth/hearth_client.gd")
const ROOM_TYPE := "tictactoe"
const TIMEOUT_SECONDS := 40.0
const WAIT_MS := 8000

var _a: Node = null
var _b: Node = null
var _c: Node = null
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
	var run_id := Time.get_ticks_usec()
	var pool := "smoke%d" % run_id

	_a = HearthClient.new()
	_b = HearthClient.new()
	_c = HearthClient.new()
	# Three clients on one machine must not share a device id, or the gateway
	# would assign them the same anonymous player.
	for client in [_a, _b, _c]:
		client.server_url = url
		client.use_schema(TicTacToeBoard)
		if not schema_override.is_empty():
			client.schema_hash = schema_override
		client.device_id_override = "mm-%d-%s" % [run_id, _name_of(client)]
		client.error.connect(_on_error)
		root.add_child(client)
	_a.acked.connect(_on_acked.bind(_a))
	_b.acked.connect(_on_acked.bind(_b))
	await process_frame

	for client in [_a, _b, _c]:
		await client.login_anonymous()
		if _failure != "":
			return
		if client.player_id.is_empty():
			_fail("login_anonymous returned no player id")
			return

	# The run's pool is fresh, so the lobby starts empty and anything listed
	# afterwards was minted by this run.
	var before: Array = await _c.list_rooms(ROOM_TYPE, pool)
	if not before.is_empty():
		_fail("lobby of the fresh pool %s is not empty: %s" % [pool, str(before)])
		return
	_ok("fresh pool %s has an empty lobby" % pool)

	await _a.quick_join(ROOM_TYPE, pool)
	if _failure != "":
		return
	_ok("client A quick-joined (seat %d)" % _a.seat)
	await _b.quick_join(ROOM_TYPE, pool)
	if _failure != "":
		return
	_ok("client B quick-joined (seat %d)" % _b.seat)
	if _a.seat < 0 or _b.seat < 0:
		_fail("quick_join returned without seating a client (seats %d, %d)" % [_a.seat, _b.seat])
		return

	# Both clients see the same match once the second player seats: that is the
	# proof the matchmaker put them in one room.
	if not await _wait_until(func() -> bool: return _variant(_a) == TicTacToeStatus.PLAYING):
		_fail("game did not start (status %s)" % _status_text(_a))
		return
	if not await _wait_until(func() -> bool: return _variant(_b) == TicTacToeStatus.PLAYING):
		_fail("game did not start on client B (status %s)" % _status_text(_b))
		return
	var marks_a: Dictionary = _marks(_a)
	var marks_b: Dictionary = _marks(_b)
	if marks_a.size() != 2 or marks_b.size() != 2:
		_fail("clients are not in one two-player room (marks %s / %s)" % [marks_a, marks_b])
		return
	_ok("both clients share the matchmade room and the game is playing")

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
	_ok("X (%s) won the matchmade game" % x_client.player_id)

	# A win records through `ctx.notify`, whose outbox delivery happens after the
	# winning pass: poll `/me` instead of racing it.
	if not await _wait_stats(x_client, "wins", 1):
		var winner_stats: Dictionary = await x_client.get_me()
		_fail("winner /me does not show wins == 1 (%s)" % str(winner_stats))
		return
	_ok("winner /me shows wins == 1")
	if not await _wait_stats(o_client, "losses", 1):
		var loser_stats: Dictionary = await o_client.get_me()
		_fail("loser /me does not show losses == 1 (%s)" % str(loser_stats))
		return
	_ok("loser /me shows losses == 1")

	# The third client cannot fit in the finished two-seat room, so quick-join
	# mints a fresh one; its own lobby listing must show that room.
	await _c.quick_join(ROOM_TYPE, pool)
	if _failure != "":
		return
	if _c.seat < 0:
		_fail("client C quick_join returned without seating it")
		return
	_ok("client C quick-joined (seat %d)" % _c.seat)
	var rooms: Array = await _c.list_rooms(ROOM_TYPE, pool)
	if rooms.size() != 1:
		_fail("lobby of pool %s has %d rooms, expected the third client's own one: %s" % [pool, rooms.size(), str(rooms)])
		return
	var own: Dictionary = rooms[0]
	if not String(own.get("name", "")).begins_with("m_"):
		_fail("lobby room name is not a matchmaker name: %s" % str(own))
		return
	if int(own.get("capacity", 0)) != 2:
		_fail("lobby room capacity is %d, expected 2" % int(own.get("capacity", 0)))
		return
	if int(own.get("players", 0)) > 1:
		_fail("lobby room has %d players, expected at most 1" % int(own.get("players", 0)))
		return
	_ok("list_rooms shows client C's own room (%s, players %d)" % [String(own.get("name", "")), int(own.get("players", 0))])

	_finished = true
	print(
		"matchmaking_smoke: ok (pool %s, winner %s, backend %s)"
		% [pool, x_client.player_id, _backend_name()]
	)
	quit(0)


func _name_of(client: Node) -> String:
	if client == _a:
		return "a"
	if client == _b:
		return "b"
	return "c"


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


## Poll `GET /me` until `field` reaches `want`; the stats commit after the game.
func _wait_stats(client: Node, field: String, want: int) -> bool:
	var deadline := Time.get_ticks_msec() + WAIT_MS
	while Time.get_ticks_msec() < deadline:
		if _failure != "":
			return false
		var stats: Dictionary = await client.get_me()
		if int(stats.get(field, 0)) == want:
			return true
		await create_timer(0.1).timeout
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
	_fail("server error %s: %s (ref_seq %d)" % [code, message, ref_seq])


func _fail(reason: String) -> void:
	if _finished:
		return
	_finished = true
	printerr("matchmaking_smoke FAILED: " + reason)
	quit(1)


func _backend_name() -> String:
	if OS.has_feature("web") or OS.get_environment("HEARTH_FORCE_WEB") == "1":
		return "web"
	return "native"
