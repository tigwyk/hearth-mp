extends Control
## Minimal Hearth tic-tac-toe client for the `examples/tictactoe` sample room.
##
## Flow: anonymous login -> create (or join) a room -> click cells. The board is
## ordinary synced state, so the server's patch keeps every client in step.
##
## The typed classes under `res://hearth/` are generated from the server crate
## (see the README):
##
##   hearth codegen --godot examples/tictactoe-godot/hearth --manifest-path examples/tictactoe/Cargo.toml
##
## Run `scripts/sync-addon.ps1` first so `res://addons/hearth` exists.

const ROOM_TYPE := "tictactoe"

var _buttons: Array[Button] = []
var _status_label: Label
var _room_edit: LineEdit
var _busy := false
var _signals_bound := false


func _ready() -> void:
	# Sets the schema hash from TicTacToeBoard.SCHEMA_HASH and creates the typed
	# state mirror; the v0.1 code fetched the hash from GET /schema/tictactoe.
	Hearth.use_schema(TicTacToeBoard)
	_build_ui()
	Hearth.joined.connect(_on_joined)
	Hearth.state_reset.connect(_on_state_reset)
	Hearth.error.connect(_on_error)
	Hearth.disconnected.connect(_on_disconnected)
	_bind_state_signals()
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


## Bind the typed state signals once the mirror exists. `use_schema` creates the
## root, but its HearthMap/HearthList fields are created on the first reload, so
## this also runs from the first `state_reset`.
func _bind_state_signals() -> void:
	if _signals_bound:
		return
	var state = Hearth.state
	if state == null or state is Dictionary or state.cells == null:
		return
	state.cells.changed.connect(_render_cells)
	state.status_changed.connect(_on_state_field_changed)
	state.turn_changed.connect(_on_state_field_changed)
	_signals_bound = true


## Typed field signals carry the new value; the label only needs a re-render.
func _on_state_field_changed(_value) -> void:
	_render_status()


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
	if Hearth.send_intent(TicTacToeIntents.place(cell)) == 0:
		_status_label.text = "intent could not be converted"


func _on_joined(joined_player_id: String, joined_seat: int) -> void:
	_status_label.text = "joined as %s (seat %d)" % [joined_player_id, joined_seat]


func _on_state_reset() -> void:
	_bind_state_signals()
	_render()


func _on_error(code: String, message: String, ref_seq: int) -> void:
	_status_label.text = "%s: %s (intent %d)" % [code, message, ref_seq]


func _on_disconnected(code: int, reason: String) -> void:
	_status_label.text = "disconnected %d: %s" % [code, reason]


func _render() -> void:
	_render_cells()
	_render_status()


func _render_cells() -> void:
	var state = Hearth.state
	if state == null or state is Dictionary or state.cells == null:
		return
	var cells: HearthList = state.cells
	var my_turn := _is_my_turn()
	for cell in _buttons.size():
		var mark := int(cells.at(cell)) if cell < cells.size() else 0
		_buttons[cell].text = ["", "X", "O"][clampi(mark, 0, 2)]
		_buttons[cell].disabled = not my_turn or mark != 0


func _render_status() -> void:
	var status: TicTacToeStatus = _status()
	var text := "waiting"
	if status != null:
		if status.variant == TicTacToeStatus.WAITING:
			text = "waiting for a second player"
		elif status.variant == TicTacToeStatus.PLAYING:
			text = "your turn" if _is_my_turn() else "opponent's turn"
		elif status.variant == TicTacToeStatus.WON:
			if String(status.value) == Hearth.player_id:
				text = "you won!"
			else:
				text = "won by %s" % String(status.value)
		elif status.variant == TicTacToeStatus.DRAW:
			text = "draw"
	_status_label.text = text


func _status() -> TicTacToeStatus:
	var state = Hearth.state
	if state == null or state is Dictionary:
		return null
	return state.status


func _is_my_turn() -> bool:
	var status: TicTacToeStatus = _status()
	return (
		status != null
		and status.variant == TicTacToeStatus.PLAYING
		and String(Hearth.state.turn) == Hearth.player_id
	)
