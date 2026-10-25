//! Plan v0.2a Task 7 Step 1: golden tests for the GDScript generator.
//!
//! `tictactoe_golden` exports the real schema for `examples/tictactoe` with the
//! built `hearth` binary (the same nested-cargo path the driver test uses, with
//! its own `CARGO_TARGET_DIR` so the two tests never share nested-cargo state)
//! and byte-compares the generated files against `tests/golden/godot/`.
//! `UPDATE_GOLDEN=1` rewrites the goldens instead.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use hearth_cli::codegen::{godot, load_docs};
use hearth_server::SchemaDoc;
use hearth_sync::{Field, Shape, TypeDef, TypeRef, Variant, Vis};

const HEARTH: &str = env!("CARGO_BIN_EXE_hearth");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/hearth-cli is two levels below the repo root")
        .to_path_buf()
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/godot")
}

fn unique_temp(label: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "hearth-godot-gen-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).expect("a temp dir is creatable");
    path
}

/// Export the real schema docs for the tictactoe sample through the CLI driver.
fn tictactoe_docs() -> Vec<SchemaDoc> {
    let repo = repo_root();
    let target_dir = repo.join("target").join("hearth-godot-gen-driver");
    let out_dir = unique_temp("out");
    let manifest = repo.join("examples").join("tictactoe").join("Cargo.toml");
    let output = Command::new(HEARTH)
        .arg("codegen")
        .arg("--godot")
        .arg(&out_dir)
        .arg("--manifest-path")
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .expect("the hearth binary runs");
    let _ = fs::remove_dir_all(&out_dir);
    assert!(
        output.status.success(),
        "codegen failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let docs = load_docs(&target_dir.join("hearth-schema")).expect("schema docs load");
    assert!(!docs.is_empty(), "the sample exported at least one room");
    docs
}

#[test]
fn tictactoe_golden() {
    let docs = tictactoe_docs();
    let files = godot::generate(&docs).expect("generation succeeds");
    let generated: BTreeMap<String, String> = files.into_iter().collect();
    let golden = golden_dir();

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(&golden).expect("the golden directory is creatable");
        for entry in fs::read_dir(&golden).expect("the golden directory is readable") {
            let path = entry.expect("a directory entry").path();
            if path.extension().is_some_and(|extension| extension == "gd") {
                fs::remove_file(&path).expect("a stale golden is removable");
            }
        }
        for (name, contents) in &generated {
            fs::write(golden.join(name), contents).expect("a golden is writable");
        }
        eprintln!("updated {} golden files in {:?}", generated.len(), golden);
        return;
    }

    assert!(!generated.is_empty(), "generation produced no files");
    let mut golden_names: Vec<String> = fs::read_dir(&golden)
        .unwrap_or_else(|error| panic!("missing golden dir {:?}: {error}", golden))
        .map(|entry| entry.expect("a directory entry").file_name())
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name.ends_with(".gd"))
        .collect();
    golden_names.sort();
    let generated_names: Vec<String> = generated.keys().cloned().collect();
    assert_eq!(
        golden_names, generated_names,
        "the golden file set changed; review the diff, then run with UPDATE_GOLDEN=1"
    );
    for (name, contents) in &generated {
        let expected = fs::read_to_string(golden.join(name))
            .unwrap_or_else(|error| panic!("missing golden {name}: {error}"));
        assert_eq!(
            &expected, contents,
            "golden mismatch for {name}; review the diff, then run with UPDATE_GOLDEN=1"
        );
    }
}

// ---- unit tests over hand-built schema docs -------------------------------

fn field(name: &str, ty: TypeRef, vis: Vis) -> Field {
    Field {
        name: name.to_owned(),
        ty,
        vis,
    }
}

fn doc(room: &str, state: &str, intent: &str, types: Vec<(&str, TypeDef)>) -> SchemaDoc {
    SchemaDoc {
        hearth_schema: 1,
        room: room.to_owned(),
        hash: "0123456789abcdef".to_owned(),
        state: state.to_owned(),
        intent: intent.to_owned(),
        types: types
            .into_iter()
            .map(|(name, def)| (name.to_owned(), def))
            .collect(),
    }
}

fn empty_enum() -> TypeDef {
    TypeDef::Enum { variants: Vec::new() }
}

fn generated_file(docs: &[SchemaDoc], name: &str) -> String {
    godot::generate(docs)
        .expect("generation succeeds")
        .into_iter()
        .find(|(file, _)| file == name)
        .unwrap_or_else(|| panic!("expected a generated {name}"))
        .1
}

#[test]
fn keyword_field_escaped() {
    let state = TypeDef::Struct {
        fields: vec![field("class", TypeRef::U8, Vis::Public)],
    };
    let docs = vec![doc(
        "TestRoom",
        "TestState",
        "TestIntent",
        vec![("TestState", state), ("TestIntent", empty_enum())],
    )];
    let contents = generated_file(&docs, "test_room_test_state.gd");
    assert!(
        contents.contains("var class_: int"),
        "keyword field is escaped in GDScript: {contents}"
    );
    assert!(
        contents.contains("signal class__changed(value)"),
        "the signal uses the escaped name: {contents}"
    );
    assert!(
        contents.contains("\"gd\": \"class_\", \"wire\": \"class\""),
        "meta keeps the wire name: {contents}"
    );
    assert!(
        !contents.contains("var class: int"),
        "the unescaped keyword never appears: {contents}"
    );
}

#[test]
fn member_field_escaped() {
    let state = TypeDef::Struct {
        fields: vec![field("connect", TypeRef::U8, Vis::Public)],
    };
    let docs = vec![doc(
        "TestRoom",
        "TestState",
        "TestIntent",
        vec![("TestState", state), ("TestIntent", empty_enum())],
    )];
    let contents = generated_file(&docs, "test_room_test_state.gd");
    assert!(
        contents.contains("var connect_: int"),
        "base-class members are escaped: {contents}"
    );
    assert!(
        contents.contains("\"gd\": \"connect_\", \"wire\": \"connect\""),
        "meta keeps the wire name: {contents}"
    );
}

#[test]
fn intent_constructors_cover_all_shapes() {
    let intent = TypeDef::Enum {
        variants: vec![
            Variant {
                name: "Rematch".to_owned(),
                shape: Shape::Unit,
            },
            Variant {
                name: "Add".to_owned(),
                shape: Shape::Newtype(TypeRef::U8),
            },
            Variant {
                name: "Timer".to_owned(),
                shape: Shape::Tuple(vec![TypeRef::String, TypeRef::U64]),
            },
            Variant {
                name: "Place".to_owned(),
                shape: Shape::Struct(vec![field("cell", TypeRef::U8, Vis::Public)]),
            },
        ],
    };
    let docs = vec![doc(
        "TestRoom",
        "TestState",
        "TestIntent",
        vec![
            ("TestState", TypeDef::Struct { fields: Vec::new() }),
            ("TestIntent", intent),
        ],
    )];
    let contents = generated_file(&docs, "test_room_intents.gd");

    assert!(
        contents.contains("class_name TestRoomIntents\nextends RefCounted"),
        "intent helper class: {contents}"
    );
    assert!(
        contents.contains("static func rematch() -> String:\n\treturn \"Rematch\""),
        "unit variant returns the bare name: {contents}"
    );
    assert!(
        contents.contains("static func add(value: int) -> Dictionary:\n\treturn {\"Add\": value}"),
        "newtype variant wraps its value: {contents}"
    );
    assert!(
        contents
            .contains("static func timer(value0: String, value1: int) -> Dictionary:\n\treturn {\"Timer\": [value0, value1]}"),
        "tuple variant takes positional args: {contents}"
    );
    assert!(
        contents
            .contains("static func place(cell: int) -> Dictionary:\n\treturn {\"Place\": {\"cell\": cell}}"),
        "struct variant takes named args: {contents}"
    );
}

#[test]
fn owner_field_documented() {
    let state = TypeDef::Struct {
        fields: vec![field(
            "hand",
            TypeRef::SyncMap(
                Box::new(TypeRef::PlayerId),
                Box::new(TypeRef::String),
            ),
            Vis::Owner,
        )],
    };
    let docs = vec![doc(
        "TestRoom",
        "TestState",
        "TestIntent",
        vec![("TestState", state), ("TestIntent", empty_enum())],
    )];
    let contents = generated_file(&docs, "test_room_test_state.gd");
    assert!(
        contents.contains("## Only your own entry is visible.\nvar hand: HearthMap"),
        "owner-only fields carry the doc comment: {contents}"
    );
}
