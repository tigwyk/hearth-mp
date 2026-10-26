extends SceneTree
## Run `HearthServer` (the headless dedicated-server helper) against the v0.5
## fleet e2e fixture.
##
## The runner (`examples/arena-dedicated/tests/run-server-helper.ps1`) starts
## `node e2e-fleet.mjs --print-session`, exports the session env and the two
## tickets, imports this project, then runs this script:
##
##   godot --headless --path examples/arena-dedicated/godot-client \
##     -s tests/server_helper.gd
##
## Without the session env it only asserts that `HearthServer.from_env()` is
## `null` (the process was not started by a Hearth allocator). With the env it
## redeems a ticket and reports started/player_joined/ended plus heartbeats.
## Exit 0 or 1.

const HearthServerScript = preload("res://addons/hearth/hearth_server.gd")
const TIMEOUT_SECONDS := 30.0

var _finished := false


func _initialize() -> void:
	_watchdog()
	await _run()


func _watchdog() -> void:
	await create_timer(TIMEOUT_SECONDS).timeout
	if not _finished:
		_fail("timed out after %.0f s" % TIMEOUT_SECONDS)


func _run() -> void:
	# Let the tree finish initialising before adding HTTPRequest nodes.
	await process_frame
	var server = HearthServerScript.from_env()
	if OS.get_environment("HEARTH_SID").is_empty():
		if server != null:
			_fail("HearthServer.from_env() should be null without the session env")
			return
		print("ok - from_env is null when the session env is missing")
		_pass()
		return
	if server == null:
		_fail("HearthServer.from_env() returned null with the session env set")
		return
	root.add_child(server)
	if server.sid != OS.get_environment("HEARTH_SID"):
		_fail("sid does not match HEARTH_SID")
		return
	print("ok - from_env reads the session env (%s)" % server.sid)

	var tickets: Array = JSON.parse_string(OS.get_environment("HEARTH_TICKETS"))
	if typeof(tickets) != TYPE_ARRAY or tickets.size() < 2:
		_fail("HEARTH_TICKETS must be a JSON array of two tickets")
		return

	var identity: Dictionary = await server.redeem(String(tickets[0]))
	if identity.is_empty():
		_fail("redeem returned {} for the first ticket")
		return
	var player_id := String(identity.get("player_id", ""))
	if player_id.is_empty() or String(identity.get("sid", "")) != server.sid:
		_fail("redeemed identity is malformed: %s" % str(identity))
		return
	print("ok - redeem returned the player identity (%s)" % player_id)

	# A single-use ticket cannot be replayed.
	if not (await server.redeem(String(tickets[0]))).is_empty():
		_fail("a used ticket was redeemed twice")
		return
	print("ok - a used ticket is rejected")

	if not await server.report("started"):
		_fail("report(started) failed")
		return
	print("ok - report(started) accepted")

	if not await server.report("player_joined", {"player": player_id}):
		_fail("report(player_joined) failed")
		return
	print("ok - report(player_joined) accepted")

	server.start_heartbeat(0.1)
	await create_timer(0.35).timeout
	if not server.heartbeat_running():
		_fail("heartbeat stopped before the session ended")
		return
	print("ok - heartbeat runs (timer child)")

	if not await server.report("ended", {"reason": "server_helper"}):
		_fail("report(ended) failed")
		return
	await create_timer(0.3).timeout
	if server.heartbeat_running():
		_fail("heartbeat kept running after report(ended)")
		return
	print("ok - report(ended) stopped the heartbeat")

	# The session is inactive now: the second ticket redeems as Gone ({}).
	if not (await server.redeem(String(tickets[1]))).is_empty():
		_fail("a ticket redeemed after the session ended")
		return
	print("ok - redeem after the session ended fails (Gone)")

	server.queue_free()
	_pass()


func _pass() -> void:
	_finished = true
	print("server_helper: ok")
	quit(0)


func _fail(message: String) -> void:
	_finished = true
	printerr("server_helper FAILED: " + message)
	quit(1)
