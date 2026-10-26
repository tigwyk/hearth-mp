extends SceneTree
## Conformance scenario runner (v1.0a Task 3) for the Godot SDK.
##
## Runs every `conformance/scenarios/*.json` against the reference server
## through the `Hearth` client's public API (`login_anonymous`, `join`,
## `send_intent`, `leave`). Start the server first:
##
##   cargo run -p hearth-conformance-server            # ws on 127.0.0.1:8790
##   $env:HEARTH_CONFORMANCE_URL="http://127.0.0.1:8790"
##   $env:HEARTH_FORCE_WEB="1"   # the pure-GDScript client; unset for the GDExtension
##   godot --headless --path examples/tictactoe-godot -s tests/scenario_runner.gd
##
## Without HEARTH_CONFORMANCE_URL the run is skipped (exit 0), unless
## HEARTH_REQUIRE_CONFORMANCE=1, which makes it a failure.
##
## Observed frames, compared with each `expect` pattern exactly as the Rust
## runner does (crates/hearth-conformance-server/tests/scenarios.rs): every given
## field equal, "*" matches anything, "$absent" means the key must be missing,
## arrays match element-wise, numbers compare as numbers.

const HearthClient = preload("res://addons/hearth/hearth_client.gd")
const SCENARIOS := "../../conformance/scenarios"
const EXPECT_TIMEOUT_MS := 5000

var _failed := false


func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	var url := OS.get_environment("HEARTH_CONFORMANCE_URL")
	if url.is_empty():
		if OS.get_environment("HEARTH_REQUIRE_CONFORMANCE") == "1":
			_fail("HEARTH_CONFORMANCE_URL is not set and HEARTH_REQUIRE_CONFORMANCE=1")
		else:
			print("scenario_runner: skipped (set HEARTH_CONFORMANCE_URL)")
			quit(0)
		return
	var dir_path := ProjectSettings.globalize_path("res://").path_join(SCENARIOS).simplify_path()
	var files := DirAccess.get_files_at(dir_path)
	var names: Array = []
	for file in files:
		if file.ends_with(".json"):
			names.append(file)
	names.sort()
	if names.is_empty():
		_fail("no scenarios in " + dir_path)
		return
	var schema := await _schema(url)
	if schema.is_empty():
		_fail("GET %s/schema/conf failed" % url)
		return
	var backend := "web (GDScript)" if OS.get_environment("HEARTH_FORCE_WEB") == "1" else "native (GDExtension)"
	for file in names:
		var scenario = JSON.parse_string(FileAccess.get_file_as_string(dir_path.path_join(file)))
		if typeof(scenario) != TYPE_DICTIONARY:
			_fail("%s is not a JSON object" % file)
			return
		await _run_scenario(url, schema, scenario)
		if _failed:
			return
		print("ok - %s" % file)
	print("scenario_runner: %d scenarios passed (%s)" % [names.size(), backend])
	quit(0)


func _run_scenario(url: String, schema: String, scenario: Dictionary) -> void:
	var room := "%s-%d" % [scenario["name"], Time.get_ticks_usec()]
	var clients := {}
	for name in scenario["clients"]:
		var client = HearthClient.new()
		client.server_url = url
		client.schema_hash = schema
		client.device_id_override = String(name)
		root.add_child(client)
		var record := {"client": client, "observed": [], "cursor": 0}
		_listen(client, record)
		clients[name] = record
	await process_frame
	for name in clients:
		await clients[name]["client"].login_anonymous()

	for step in scenario["steps"]:
		if step.has("wait_ms"):
			await create_timer(float(step["wait_ms"]) / 1000.0).timeout
			continue
		var record: Dictionary = clients[step["client"]]
		var client = record["client"]
		if step.has("expect"):
			if not await _expect(record, step["expect"]):
				_fail("%s: client %s never observed %s\nobserved after cursor: %s" % [
					scenario["name"], step["client"], JSON.stringify(step["expect"]),
					JSON.stringify(record["observed"].slice(record["cursor"])),
				])
				return
			continue
		var send = step["send"]
		if typeof(send) == TYPE_STRING:
			match send:
				"connect":
					client.join("conf", room)
				"drop":
					# The SDK reconnects by itself.
					client._debug_drop()
		else:
			match String(send.get("op", "")):
				"HELLO":
					client.schema_hash = String(send["schema_hash"])
					client.join("conf", room, send.get("opts", {}))
				"INTENT":
					for message in send["intents"]:
						client.send_intent(message["intent"])
				"LEAVE":
					client.leave()

	for name in clients:
		clients[name]["client"].leave()
		clients[name]["client"].queue_free()
	await process_frame


func _listen(client: Node, record: Dictionary) -> void:
	var observed: Array = record["observed"]
	client.joined.connect(func(player_id: String, seat: int) -> void:
		observed.append({"op": "JOIN_OK", "player_id": player_id, "seat": seat}))
	client.state_reset.connect(func() -> void:
		observed.append({"op": "FULL_STATE", "state": client.raw_state.duplicate(true)}))
	client.patched.connect(func(ops: Array) -> void:
		observed.append({"op": "PATCH", "ops": ops, "state": client.raw_state.duplicate(true)}))
	client.acked.connect(func(_state_seq: int, intent_seqs: Array) -> void:
		observed.append({"op": "ACK", "intent_seqs": intent_seqs}))
	client.event.connect(func(name: String, data: Variant) -> void:
		observed.append({"op": "EVENT", "name": name, "data": data}))
	client.error.connect(func(code: String, message: String, ref_seq: int) -> void:
		observed.append({"op": "ERROR", "code": code, "message": message, "ref_seq": null if ref_seq < 0 else ref_seq}))
	client.disconnected.connect(func(code: int, _reason: String) -> void:
		observed.append({"op": "CLOSE", "code": code}))


func _expect(record: Dictionary, pattern: Variant) -> bool:
	var deadline := Time.get_ticks_msec() + EXPECT_TIMEOUT_MS
	while true:
		var observed: Array = record["observed"]
		for i in range(record["cursor"], observed.size()):
			if _matches(pattern, observed[i]):
				record["cursor"] = i + 1
				return true
		if Time.get_ticks_msec() >= deadline:
			return false
		await process_frame
	return false


func _matches(pattern: Variant, value: Variant) -> bool:
	if typeof(pattern) == TYPE_STRING and pattern == "*":
		return true
	if typeof(pattern) == TYPE_DICTIONARY:
		if typeof(value) != TYPE_DICTIONARY:
			return false
		for key in pattern:
			var expected = pattern[key]
			if typeof(expected) == TYPE_STRING and expected == "$absent":
				if value.has(key):
					return false
				continue
			if not value.has(key) or not _matches(expected, value[key]):
				return false
		return true
	if typeof(pattern) == TYPE_ARRAY:
		if typeof(value) != TYPE_ARRAY or pattern.size() != value.size():
			return false
		for i in pattern.size():
			if not _matches(pattern[i], value[i]):
				return false
		return true
	var numbers := [TYPE_INT, TYPE_FLOAT]
	if typeof(pattern) in numbers and typeof(value) in numbers:
		return float(pattern) == float(value)
	if pattern == null:
		return value == null
	return typeof(pattern) == typeof(value) and pattern == value


func _schema(url: String) -> String:
	var http := HTTPRequest.new()
	root.add_child(http)
	if http.request(url.rstrip("/") + "/schema/conf") != OK:
		http.queue_free()
		return ""
	var response: Array = await http.request_completed
	http.queue_free()
	if int(response[1]) != 200:
		return ""
	var body = JSON.parse_string(String(response[3].get_string_from_utf8()))
	return "" if typeof(body) != TYPE_DICTIONARY else String(body.get("schema_hash", ""))


func _fail(reason: String) -> void:
	if _failed:
		return
	_failed = true
	printerr("scenario_runner FAILED: " + reason)
	quit(1)
