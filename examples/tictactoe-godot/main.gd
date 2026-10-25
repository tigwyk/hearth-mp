extends Control
## Minimal Hearth tic-tac-toe client for the `examples/tictactoe` sample room.
##
## Flow: anonymous login -> create (or join) a room -> click cells. The board is
## ordinary synced state, so the server's patch keeps every client in step.
##
## Run `scripts/sync-addon.ps1` first so `res://addons/hearth` exists.

const ROOM_TYPE := "tictactoe"
const SCHEMA_PATH := "/schema/tictactoe"

var _buttons: Array[Button] = []
var _status_label: Label
var _room_edit: LineEdit
var _busy := false


func _ready() -> void:
	_build_ui()
	Hearth.joined.connect(_on_joined)
	Hearth.state_reset.connect(_render)
	Hearth.patched.connect(_on_patched)
	Hearth.error.connect(_on_error)
	Hearth.disconnected.connect(_on_disconnected)
	if Hearth.schema_hash.is_empty():
		Hearth.schema_hash = await _fetch_schema()
	_render()


func _build_ui() -> void:
	var layout := VBoxContainer.new()
	layout.set_anchors_preset(Control.PRESET_FULL_RECT)
	add_child(layout)

	var title := Label.new()
	title.text = "Hearth tic-tac-toe"
	layout.add_child(title)

	_status_label = Label.new()
	layout.add_child(_status_label)

	var row := HBoxContainer.new()
	layout.add_child(row)
	_room_edit = LineEdit.new()
	_room_edit.placeholder_text = "room name"
	_room_edit.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	row.add_child(_room_edit)
	var create_button := Button.new()
	create_button.text = "Create"
	create_button.pressed.connect(_on_create_pressed)
	row.add_child(create_button)
	var join_button := Button.new()
	join_button.text = "Join"
	join_button.pressed.connect(_on_join_pressed)
	row.add_child(join_button)

	var grid := GridContainer.new()
	grid.columns = 3
	layout.add_child(grid)
	for cell in range(9):
		var button := Button.new()
		button.custom_minimum_size = Vector2(64, 64)
		button.pressed.connect(_on_cell_pressed.bind(cell))
		grid.add_child(button)
		_buttons.append(button)


func _on_create_pressed() -> void:
	if _busy:
		return
	_busy = true
	await Hearth.login_anonymous()
	if not Hearth.player_id.is_empty():
		var room := await Hearth.create_room(ROOM_TYPE)
		if not room.is_empty():
			_room_edit.text = room
			Hearth.join(ROOM_TYPE, room)
	_busy = false


func _on_join_pressed() -> void:
	if _busy:
		return
	var room := _room_edit.text.strip_edges()
	if room.is_empty():
		return
	_busy = true
	await Hearth.login_anonymous()
	if not Hearth.player_id.is_empty():
		Hearth.join(ROOM_TYPE, room)
	_busy = false


func _on_cell_pressed(cell: int) -> void:
	if Hearth.send_intent({"Place": {"cell": cell}}) == 0:
		_status_label.text = "intent could not be converted"


func _on_joined(joined_player_id: String, joined_seat: int) -> void:
	_status_label.text = "joined as %s (seat %d)" % [joined_player_id, joined_seat]


func _on_patched(_ops: Array) -> void:
	_render()


func _on_error(code: String, message: String, ref_seq: int) -> void:
	_status_label.text = "%s: %s (intent %d)" % [code, message, ref_seq]


func _on_disconnected(code: int, reason: String) -> void:
	_status_label.text = "disconnected %d: %s" % [code, reason]


func _render() -> void:
	var cells: Array = Hearth.state.get("cells", [])
	var status = Hearth.state.get("status", "Waiting")
	var my_turn: bool = status == "Playing" and String(Hearth.state.get("turn", "")) == Hearth.player_id
	for cell in _buttons.size():
		var mark := int(cells[cell]) if cell < cells.size() else 0
		_buttons[cell].text = ["", "X", "O"][clampi(mark, 0, 2)]
		_buttons[cell].disabled = not my_turn or mark != 0
	_status_label.text = _status_text()


func _status_text() -> String:
	var status = Hearth.state.get("status", "Waiting")
	match typeof(status):
		TYPE_DICTIONARY:
			if status.has("Won"):
				var winner := String(status["Won"])
				return "you won!" if winner == Hearth.player_id else "won by " + winner
			if status.has("Draw"):
				return "draw"
			return "unknown status"
		TYPE_STRING:
			if status == "Waiting":
				return "waiting for a second player"
			if status == "Playing":
				var your_turn := String(Hearth.state.get("turn", "")) == Hearth.player_id
				return "your turn" if your_turn else "opponent's turn"
	return "waiting"


func _fetch_schema() -> String:
	var request := HTTPRequest.new()
	add_child(request)
	var url := Hearth.server_url.trim_suffix("/") + SCHEMA_PATH
	if request.request(url) != OK:
		request.queue_free()
		return ""
	var completed: Array = await request.request_completed
	request.queue_free()
	var parsed = JSON.parse_string((completed[3] as PackedByteArray).get_string_from_utf8())
	if typeof(parsed) == TYPE_DICTIONARY:
		return String(parsed.get("schema_hash", ""))
	return ""
