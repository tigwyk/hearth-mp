extends SceneTree
## A server that accepts connections but never answers must not hang the client:
## login fails with HTTP_FAILED and a plain join() fails with TIMEOUT. No Hearth
## server needed; this script is the stuck server.
##
##   godot --headless --path examples/tictactoe-godot -s tests/stuck_server.gd
##
## Set HEARTH_FORCE_WEB=1 to run it through the pure-GDScript backend.

const HearthClient = preload("res://addons/hearth/hearth_client.gd")
const PORT := 18791

var _server := TCPServer.new()
var _held: Array = []  # accepted connections, kept open and silent
var _errors: Array = []


func _initialize() -> void:
	if _server.listen(PORT, "127.0.0.1") != OK:
		_fail("cannot listen on %d" % PORT)
		return
	var client = HearthClient.new()
	client.server_url = "http://127.0.0.1:%d" % PORT
	client.error.connect(func(code: String, _message: String, _ref_seq: int) -> void: _errors.append(code))
	client._http_timeout_s = 1.0
	client._join_timeout_ms = 1000
	root.add_child(client)
	await process_frame

	var started := Time.get_ticks_msec()
	client.login_anonymous()
	if not await _wait_for("HTTP_FAILED", 5000):
		_fail("login against a silent server did not fail with HTTP_FAILED (errors: %s)" % [_errors])
		return
	print("ok - login fails with HTTP_FAILED after %d ms" % (Time.get_ticks_msec() - started))

	client._token = "tok"
	started = Time.get_ticks_msec()
	client.join("tictactoe", "r1")
	if not await _wait_for("TIMEOUT", 5000):
		_fail("join against a silent server did not fail with TIMEOUT (errors: %s)" % [_errors])
		return
	print("ok - join fails with TIMEOUT after %d ms" % (Time.get_ticks_msec() - started))
	if _held.is_empty():
		_fail("the client never connected to the stuck server")
		return
	print("stuck_server: ok (%s)" % ("web" if OS.get_environment("HEARTH_FORCE_WEB") == "1" else "native"))
	quit(0)


## Accept and hold connections while waiting for `code` among the errors.
func _wait_for(code: String, timeout_ms: int) -> bool:
	var deadline := Time.get_ticks_msec() + timeout_ms
	while Time.get_ticks_msec() < deadline:
		while _server.is_connection_available():
			_held.append(_server.take_connection())
		if code in _errors:
			return true
		await process_frame
	return false


func _fail(reason: String) -> void:
	printerr("stuck_server FAILED: " + reason)
	quit(1)
