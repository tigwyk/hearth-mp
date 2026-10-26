extends SceneTree
## The GDScript public surface of `addons/hearth` against `api/godot-gdscript.txt`
## (v1.0a Task 4), which spec/stability.md puts under semver: for each script,
## its `class_name`, public methods with their argument names, and signals with
## theirs. Names starting with `_` are private and left out.
##
##   godot --headless --path examples/tictactoe-godot -s tests/api_snapshot.gd
##
## A difference fails until the snapshot is regenerated on purpose with
## scripts/check-stability.sh --bless (it sets HEARTH_API_BLESS=1).

const ADDON := "res://addons/hearth"
const SNAPSHOT := "../../api/godot-gdscript.txt"


func _init() -> void:
	var lines: PackedStringArray = []
	var files := DirAccess.get_files_at(ADDON)
	var scripts: Array = []
	for file in files:
		if file.ends_with(".gd"):
			scripts.append(file)
	scripts.sort()
	for file in scripts:
		var script: Script = load(ADDON.path_join(file))
		var name := String(script.get_global_name())
		lines.append("script %s%s" % [file, "" if name.is_empty() else " class_name " + name])
		var entries: PackedStringArray = []
		for method in script.get_script_method_list():
			var method_name := String(method["name"])
			if method_name.begins_with("_") or method_name.begins_with("@"):
				continue
			entries.append("  func %s(%s)" % [method_name, _args(method["args"])])
		for sig in script.get_script_signal_list():
			entries.append("  signal %s(%s)" % [sig["name"], _args(sig["args"])])
		entries.sort()
		# The method list repeats overridden names; keep each line once.
		var last := ""
		for entry in entries:
			if entry != last:
				lines.append(entry)
			last = entry
	var current := "\n".join(lines) + "\n"
	var path := ProjectSettings.globalize_path("res://").path_join(SNAPSHOT).simplify_path()
	if OS.get_environment("HEARTH_API_BLESS") == "1":
		var out := FileAccess.open(path, FileAccess.WRITE)
		out.store_string(current)
		out.close()
		print("api_snapshot: wrote " + path)
		quit(0)
		return
	var expected := FileAccess.get_file_as_string(path).replace("\r\n", "\n")
	if expected != current:
		printerr("api_snapshot FAILED: API changed: review, then run scripts/check-stability.sh --bless")
		printerr("current surface:\n" + current)
		quit(1)
		return
	print("api_snapshot: %d scripts match %s" % [scripts.size(), path])
	quit(0)


func _args(args: Array) -> String:
	var names: PackedStringArray = []
	for arg in args:
		names.append(String(arg["name"]))
	return ", ".join(names)
