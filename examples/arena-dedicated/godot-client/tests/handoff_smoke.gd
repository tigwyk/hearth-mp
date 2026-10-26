extends SceneTree
## End-to-end handoff smoke test for the Godot SDK (plan v0.5 Task 4).
##
## Needs the Task 3 container setup: `wrangler dev` in examples/arena-dedicated
## with Cloudflare Containers enabled (wrangler refuses local containers on
## Windows, so this needs WSL or a container-capable host), and the
## arena-dedicated server image built. Status: written and `--check-only`
## loadable, but NOT run locally on Windows — it needs a Linux-hosted
## `wrangler dev`/CI or a deployed gateway. Then:
##
##   $env:HEARTH_URL = "http://127.0.0.1:8790"
##   godot --headless --path examples/arena-dedicated/godot-client \
##     -s tests/handoff_smoke.gd
##
## Two clients ready up in the lobby, receive `handoff`, connect with
## `connect_handoff`, receive the 30 Hz time broadcasts from the container
## server (and its gateway-redeemed identity), and see the session end
## (`handoff_ended` or `ended` in the lobby log).
##
## Set HEARTH_FORCE_WEB=1 to run the same scenario through the web backend
## (client-only check); transport and assertions are identical.
##
## Exit 0 or 1.

const HearthClient = preload("res://addons/hearth/hearth_client.gd")
const ROOM_TYPE := "arena"
## A first container build/start can take a while; the room's start timeout is 60 s.
const HANDOFF_TIMEOUT_MS := 120_000
## The match runs for 5 s at 30 Hz: 60 ticks is a safe lower bound (2 s).
const MATCH_TICKS_MIN := 60
const CONNECT_TIMEOUT_MS := 25_000
const ENDED_TIMEOUT_MS := 20_000
const TIMEOUT_SECONDS := 240.0

var _a: Node = null
var _b: Node = null
var _handoff_a := {}
var _handoff_b := {}
var _handoff_ended_a := {}
var _handoff_ended_b := {}
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
		url = "http://127.0.0.1:8790"
	# HEADLESS-FIRST: let the tree finish initialising before adding HTTPRequests.
	await process_frame
	var schema: String = await _fetch_schema(url)
	if schema.is_empty():
		_fail("could not read %s/schema/arena" % url)
		return

	_a = HearthClient.new()
	_b = HearthClient.new()
	var run_id := Time.get_ticks_usec()
	_a.device_id_override = "handoff-%d-a" % run_id
	_b.device_id_override = "handoff-%d-b" % run_id
	for client in [_a, _b]:
		client.server_url = url
		client.schema_hash = schema
		client.error.connect(_on_client_error.bind(client))
		client.handoff.connect(_on_handoff.bind(client))
		client.handoff_ended.connect(_on_handoff_ended.bind(client))
		root.add_child(client)
	await process_frame

	await _a.login_anonymous()
	await _b.login_anonymous()
	if _a.player_id.is_empty() or _b.player_id.is_empty():
		_fail("anonymous login failed")
		return

	var room: String = await _a.create_room(ROOM_TYPE)
	if room.is_empty():
		_fail("could not create an arena lobby")
		return
	_a.join(ROOM_TYPE, room)
	if not await _wait_until(func() -> bool: return _a.seat >= 0, 5000):
		_fail("client A did not join the lobby")
		return
	_b.join(ROOM_TYPE, room)
	if not await _wait_until(func() -> bool: return _b.seat >= 0, 5000):
		_fail("client B did not join the lobby")
		return
	# Once every connected player is ready, the lobby starts the handoff.
	_a.send_intent("Ready")
	_b.send_intent("Ready")
	print("ok - two clients ready in lobby %s" % room)

	if not await _wait_until(
		func() -> bool: return not _handoff_a.is_empty() and not _handoff_b.is_empty(),
		HANDOFF_TIMEOUT_MS
	):
		_fail("clients did not receive hearth.handoff within %d s" % (HANDOFF_TIMEOUT_MS / 1000))
		return
	for payload: Dictionary in [_handoff_a, _handoff_b]:
		var endpoint := String(payload.get("endpoint", ""))
		if not (endpoint.begins_with("ws://") or endpoint.begins_with("wss://")):
			_fail("the container handoff endpoint is not a WebSocket endpoint: %s" % endpoint)
			return
	print("ok - both clients received a handoff with a ws endpoint")

	var peer_a: WebSocketPeer = _a.connect_handoff(
		String(_handoff_a["endpoint"]), String(_handoff_a["ticket"])
	)
	var peer_b: WebSocketPeer = _b.connect_handoff(
		String(_handoff_b["endpoint"]), String(_handoff_b["ticket"])
	)
	if peer_a == null or peer_b == null:
		_fail("connect_handoff returned null")
		return

	# Both players must be in quickly: the container server starts its 5 s match
	# once it has two connections.
	var peers := {_a: peer_a, _b: peer_b}
	var opened := {_a: false, _b: false}
	var hello := {_a: "", _b: ""}
	var ticks := {_a: 0, _b: 0}
	var last_tick := {_a: -1, _b: -1}
	var monotonic := {_a: true, _b: true}
	var deadline := Time.get_ticks_msec() + CONNECT_TIMEOUT_MS
	while Time.get_ticks_msec() < deadline:
		var both_ready := true
		for client: Node in peers:
			var peer: WebSocketPeer = peers[client]
			peer.poll()
			var state := peer.get_ready_state()
			if state == WebSocketPeer.STATE_CONNECTING:
				both_ready = false
				continue
			if state == WebSocketPeer.STATE_CLOSED and not opened[client]:
				_fail("the handoff socket closed before it opened")
				return
			if state != WebSocketPeer.STATE_OPEN:
				both_ready = false
				continue
			opened[client] = true
			while peer.get_available_packet_count() > 0:
				var text := peer.get_packet().get_string_from_utf8()
				var message = JSON.parse_string(text)
				if typeof(message) != TYPE_DICTIONARY:
					_fail("the container server sent an invalid JSON message: %s" % text)
					return
				match String(message.get("type", "")):
					"hello":
						hello[client] = String(message.get("player_id", ""))
					"tick":
						var tick_t := int(message.get("t", -1))
						if tick_t <= int(last_tick[client]):
							monotonic[client] = false
						last_tick[client] = tick_t
						ticks[client] = int(ticks[client]) + 1
			if int(ticks[client]) < MATCH_TICKS_MIN or String(hello[client]).is_empty():
				both_ready = false
		if both_ready:
			break
		await process_frame
	if not (opened[_a] and opened[_b]):
		_fail("a handoff WebSocket did not reach OPEN")
		return
	if String(hello[_a]) != _a.player_id or String(hello[_b]) != _b.player_id:
		_fail(
			"the container identity is not the gateway-redeemed one (%s/%s)"
			% [hello[_a], hello[_b]]
		)
		return
	if int(ticks[_a]) < MATCH_TICKS_MIN or int(ticks[_b]) < MATCH_TICKS_MIN:
		_fail("not enough 30 Hz broadcasts (A %d, B %d)" % [ticks[_a], ticks[_b]])
		return
	if not monotonic[_a] or not monotonic[_b]:
		_fail("the broadcast time did not increase monotonically")
		return
	print("ok - container identity redeemed by the gateway (A %s, B %s)" % [hello[_a], hello[_b]])
	print("ok - 30 Hz time broadcasts received (A %d, B %d ticks)" % [ticks[_a], ticks[_b]])

	# The match reports `ended`, so the room sends `hearth.handoff_ended` (or at
	# least its lobby log records `ended`).
	if not await _wait_until(func() -> bool: return _session_ended(), ENDED_TIMEOUT_MS):
		_fail("the lobby did not end the handoff session")
		return
	print("ok - the handoff session ended")

	peer_a.close()
	peer_b.close()
	await process_frame
	for client: Node in [_a, _b]:
		client.leave()
		client.queue_free()
	_finished = true
	print("handoff_smoke: all checks passed")
	quit(0)


func _session_ended() -> bool:
	if not _handoff_ended_a.is_empty() or not _handoff_ended_b.is_empty():
		return true
	# The lobby's log is the fallback when the event is raced by the close.
	for client: Node in [_a, _b]:
		if client == null:
			continue
		var log = client.raw_state.get("log", [])
		if typeof(log) == TYPE_ARRAY and log.has("ended"):
			return true
	return false


func _on_handoff(sid: String, endpoint: String, ticket: String, client: Node) -> void:
	var payload := {"sid": sid, "endpoint": endpoint, "ticket": ticket}
	if client == _a:
		_handoff_a = payload
	else:
		_handoff_b = payload


func _on_handoff_ended(sid: String, reason: String, client: Node) -> void:
	var payload := {"sid": sid, "reason": reason}
	if client == _a:
		_handoff_ended_a = payload
	else:
		_handoff_ended_b = payload


func _on_client_error(code: String, message: String, ref_seq: int, client: Node) -> void:
	# DESYNC/BAD_FRAME etc. fail the smoke; transient reconnect errors do not.
	if code == "DESYNC" or code == "BAD_FRAME" or code == "HANDOFF_ENDPOINT":
		_fail("client error %s: %s" % [code, message])


func _fetch_schema(url: String) -> String:
	var http := HTTPRequest.new()
	root.add_child(http)
	var error := http.request(url.trim_suffix("/") + "/schema/" + ROOM_TYPE)
	if error != OK:
		http.queue_free()
		return ""
	var result: Array = await http.request_completed
	http.queue_free()
	if int(result[1]) != 200:
		return ""
	var parsed = JSON.parse_string(String(result[3].get_string_from_utf8()))
	if typeof(parsed) != TYPE_DICTIONARY:
		return ""
	return String(parsed.get("schema_hash", ""))


func _wait_until(predicate: Callable, timeout_ms: int) -> bool:
	var deadline := Time.get_ticks_msec() + timeout_ms
	while Time.get_ticks_msec() < deadline:
		if predicate.call():
			return true
		await process_frame
	return predicate.call()


func _fail(message: String) -> void:
	if _finished:
		return
	_finished = true
	printerr("handoff_smoke FAILED: " + message)
	quit(1)
