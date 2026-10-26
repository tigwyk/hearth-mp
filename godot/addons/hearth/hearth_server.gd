class_name HearthServer
extends Node
## Headless dedicated-server helper for Hearth handoffs (v0.5).
##
## An allocator (a webhook fleet or the Cloudflare Containers adapter) starts a
## dedicated server with three values: the session id, the session secret and
## the report base URL. This helper turns them into the server side of the
## handoff protocol:
##
##   var server := HearthServer.from_env()
##   if server == null:
##       push_error("not started by a Hearth allocator")
##       return
##   add_child(server)                       # HTTPRequest needs the tree
##   var identity: Dictionary = await server.redeem(ticket)
##   await server.report("started")
##   server.start_heartbeat()
##   ...
##   await server.report("result", {"winner": identity["player_id"]})
##   await server.report("ended", {"reason": "match over"})
##
## A headless game server is a plain process, not a browser, so the HTTP calls
## use [HTTPRequest] rather than the Fetch API.

## The request header that carries the session secret.
const SECRET_HEADER := "X-Hearth-Session-Secret"
## Outbound HTTP calls give up after this long.
const HTTP_TIMEOUT_S := 10.0
## The default heartbeat cadence. The room's heartbeat timeout is 60 s.
const DEFAULT_HEARTBEAT_S := 20.0

## Session id (from `HEARTH_SID`).
var sid: String
## Plaintext session secret (from `HEARTH_SESSION_SECRET`); never sent to clients.
var secret: String
## `<public_url>/handoff/<sid>`, as the allocator reported it.
var report_url: String

var _heartbeat: Timer = null
var _stopped := false


func _init(sid_value := "", secret_value := "", report_url_value := "") -> void:
	sid = sid_value
	secret = secret_value
	report_url = report_url_value


## Read the allocator/container environment contract: `HEARTH_SID`,
## `HEARTH_SESSION_SECRET` and `HEARTH_REPORT_URL`. Returns `null` when any of
## them is missing, which means the process was not started by a Hearth
## allocator.
static func from_env() -> HearthServer:
	var sid_value := OS.get_environment("HEARTH_SID")
	var secret_value := OS.get_environment("HEARTH_SESSION_SECRET")
	var report_url_value := OS.get_environment("HEARTH_REPORT_URL")
	if sid_value.is_empty() or secret_value.is_empty() or report_url_value.is_empty():
		return null
	return HearthServer.new(sid_value, secret_value, report_url_value)


## Exchange a player's single-use ticket for that player's identity. Awaits the
## HTTP round trip and returns the `Redeemed` dictionary (`player_id`,
## `provider`, `display_name`, `claims`, `sid`); `{}` on any failure (the error
## is pushed; 403 also covers a used or expired ticket).
func redeem(ticket: String) -> Dictionary:
	var response: Dictionary = await _post("redeem", {"ticket": ticket})
	var status: int = response["status"]
	if status < 200 or status >= 300:
		push_error("HearthServer.redeem: HTTP %d" % status)
		return {}
	var parsed = JSON.parse_string(String(response["body"]))
	if typeof(parsed) != TYPE_DICTIONARY:
		push_error("HearthServer.redeem: response is not a JSON object")
		return {}
	return parsed


## Report one lifecycle event (`started`, `heartbeat`, `player_joined`,
## `player_left`, `result`, `ended`). Fields in `data` are merged into the body,
## so `report("ended", {"reason": "win"})` sends
## `{"type": "ended", "reason": "win"}`. Awaits the round trip and returns
## whether the room accepted it.
func report(type: String, data := {}) -> bool:
	var body := {"type": type}
	if typeof(data) == TYPE_DICTIONARY:
		for key in data:
			body[key] = data[key]
	var response: Dictionary = await _post("report", body)
	var status: int = response["status"]
	if status >= 200 and status < 300:
		# Reporting `Ended` ends the session, so the heartbeat must stop.
		if type == "ended":
			_stop_heartbeat()
		return true
	# 410 Gone: the session already ended or failed.
	if status == 410:
		_stop_heartbeat()
	push_error("HearthServer.report: HTTP %d for %s" % [status, type])
	return false


## Send `heartbeat` every `every_s` seconds until the session ends (an `ended`
## report or a `Gone` answer). The heartbeat is a Timer child of this node; call
## [method heartbeat_running] to check it.
func start_heartbeat(every_s: float = DEFAULT_HEARTBEAT_S) -> void:
	_stopped = false
	_stop_heartbeat_timer()
	_heartbeat = Timer.new()
	_heartbeat.name = "Heartbeat"
	_heartbeat.wait_time = maxf(every_s, 0.05)
	_heartbeat.autostart = false
	_heartbeat.timeout.connect(_on_heartbeat)
	add_child(_heartbeat)
	_heartbeat.start()


## Whether the heartbeat timer is running.
func heartbeat_running() -> bool:
	return _heartbeat != null and not _heartbeat.is_stopped()


func _on_heartbeat() -> void:
	if _stopped:
		_stop_heartbeat_timer()
		return
	await report("heartbeat")


func _stop_heartbeat() -> void:
	_stopped = true
	_stop_heartbeat_timer()


func _stop_heartbeat_timer() -> void:
	if _heartbeat != null:
		_heartbeat.stop()


## POST one JSON body to `<report_url>/<action>` with the session secret.
func _post(action: String, body: Dictionary) -> Dictionary:
	if not is_inside_tree():
		push_error("HearthServer: add the node to the scene tree before its HTTP methods")
		return {"status": 0, "body": ""}
	var http := HTTPRequest.new()
	http.timeout = HTTP_TIMEOUT_S
	add_child(http)
	var url := report_url.trim_suffix("/") + "/" + action
	var headers := PackedStringArray([
		"Content-Type: application/json",
		"%s: %s" % [SECRET_HEADER, secret],
	])
	var error := http.request(url, headers, HTTPClient.METHOD_POST, JSON.stringify(body))
	if error != OK:
		http.queue_free()
		push_error("HearthServer: HTTPRequest.request failed (%d) for %s" % [error, action])
		return {"status": 0, "body": ""}
	var result: Array = await http.request_completed
	http.queue_free()
	var status := int(result[1])
	var raw: PackedByteArray = result[3]
	return {"status": status, "body": raw.get_string_from_utf8()}
