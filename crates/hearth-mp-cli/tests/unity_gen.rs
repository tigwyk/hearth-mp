//! Plan v0.2b Task 5: golden and unit tests for the C# generator.
//!
//! `tictactoe_golden` runs `hearth-mp codegen --unity` on `examples/tictactoe`
//! (with its own `CARGO_TARGET_DIR`) and byte-compares the output against
//! `tests/golden/unity/`. `UPDATE_GOLDEN=1` rewrites the goldens instead.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use hearth_mp_cli::codegen::unity;
use hearth_mp_server::SchemaDoc;
use hearth_mp_sync::{Field, Shape, TypeDef, TypeRef, Variant, Vis};

const HEARTH: &str = env!("CARGO_BIN_EXE_hearth-mp");
const NS: &str = "Hearth.Generated";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/hearth-mp-cli is two levels below the repo root")
        .to_path_buf()
}

#[test]
fn tictactoe_golden() {
    let repo = repo_root();
    let out_dir = std::env::temp_dir().join(format!("hearth-unity-gen-{}", std::process::id()));
    let _ = fs::remove_dir_all(&out_dir);
    let output = Command::new(HEARTH)
        .arg("codegen")
        .arg("--unity")
        .arg(&out_dir)
        .arg("--manifest-path")
        .arg(repo.join("examples/tictactoe/Cargo.toml"))
        .env(
            "CARGO_TARGET_DIR",
            repo.join("target/hearth-unity-gen-driver"),
        )
        .output()
        .expect("the hearth binary runs");
    assert!(
        output.status.success(),
        "codegen failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let read = |dir: &Path| -> BTreeMap<String, String> {
        fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("missing {dir:?}: {e}"))
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "cs"))
            .map(|p| {
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                (name, fs::read_to_string(&p).unwrap())
            })
            .collect()
    };
    let generated = read(&out_dir);
    let _ = fs::remove_dir_all(&out_dir);
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/unity");

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        let _ = fs::remove_dir_all(&golden);
        fs::create_dir_all(&golden).unwrap();
        for (name, contents) in &generated {
            fs::write(golden.join(name), contents).unwrap();
        }
        return;
    }
    assert!(!generated.is_empty());
    assert_eq!(
        read(&golden),
        generated,
        "golden mismatch; review the diff, then run with UPDATE_GOLDEN=1"
    );
}

fn field(name: &str, ty: TypeRef) -> Field {
    Field {
        name: name.to_owned(),
        ty,
        vis: Vis::Public,
    }
}

fn doc(fields: Vec<Field>, extra: Vec<(&str, TypeDef)>) -> SchemaDoc {
    let mut types: BTreeMap<String, TypeDef> = extra
        .into_iter()
        .map(|(name, def)| (name.to_owned(), def))
        .collect();
    types.insert("State".into(), TypeDef::Struct { fields });
    types
        .entry("Intent".into())
        .or_insert(TypeDef::Enum { variants: vec![] });
    SchemaDoc {
        hearth_schema: 1,
        room: "Test".into(),
        hash: "0123456789abcdef".into(),
        state: "State".into(),
        intent: "Intent".into(),
        types,
    }
}

fn file(doc: &SchemaDoc, name: &str) -> String {
    unity::generate(std::slice::from_ref(doc), NS)
        .expect("generation succeeds")
        .into_iter()
        .find(|(file, _)| file == name)
        .unwrap_or_else(|| panic!("expected {name}"))
        .1
}

#[test]
fn base_member_suffixed() {
    let state = file(
        &doc(vec![field("load", TypeRef::U8)], vec![]),
        "TestState.cs",
    );
    assert!(
        state.contains("public byte Load_ { get; private set; }"),
        "{state}"
    );
    assert!(state.contains("ref __Load_, Load_"), "{state}");
    assert!(
        state.contains("raw, \"load\""),
        "the wire name is kept: {state}"
    );
}

#[test]
fn pascal_collision_errors() {
    let doc = doc(
        vec![field("foo_bar", TypeRef::U8), field("fooBar", TypeRef::U8)],
        vec![],
    );
    let error = unity::generate(&[doc], NS).unwrap_err();
    assert!(error.contains("both map to C# `FooBar`"), "{error}");
    assert!(
        error.contains("`foo_bar` and `fooBar` of `State`"),
        "{error}"
    );
}

#[test]
fn all_int_widths_mapped() {
    let widths = [
        ("a", TypeRef::U8, "byte", "U8"),
        ("b", TypeRef::I8, "sbyte", "I8"),
        ("c", TypeRef::U16, "ushort", "U16"),
        ("d", TypeRef::I16, "short", "I16"),
        ("e", TypeRef::U32, "uint", "U32"),
        ("f", TypeRef::I32, "int", "I32"),
        ("g", TypeRef::U64, "ulong", "U64"),
        ("h", TypeRef::I64, "long", "I64"),
        ("i", TypeRef::F32, "float", "F32"),
        ("j", TypeRef::F64, "double", "F64"),
        ("k", TypeRef::Bool, "bool", "Bool"),
        ("l", TypeRef::PlayerId, "string", "Str"),
    ];
    let fields = widths
        .iter()
        .map(|(n, t, ..)| field(n, t.clone()))
        .collect();
    let state = file(&doc(fields, vec![]), "TestState.cs");
    for (name, _, cs, conv) in widths {
        let prop = name.to_uppercase();
        assert!(
            state.contains(&format!("public {cs} {prop} {{")),
            "{cs}: {state}"
        );
        assert!(
            state.contains(&format!("Conv.{conv}, v => On{prop}Change")),
            "{conv}: {state}"
        );
    }
}

#[test]
fn option_value_vs_reference() {
    let state = file(
        &doc(
            vec![
                field("score", TypeRef::Option(Box::new(TypeRef::U32))),
                field("name", TypeRef::Option(Box::new(TypeRef::String))),
            ],
            vec![],
        ),
        "TestState.cs",
    );
    assert!(state.contains("public uint? Score {"), "{state}");
    assert!(
        state.contains("Conv.Nullable<uint>(global::Hearth.Load.Conv.U32)"),
        "{state}"
    );
    assert!(state.contains("public string Name {"), "{state}");
    assert!(
        state.contains("Conv.Option<string>(global::Hearth.Load.Conv.Str)"),
        "{state}"
    );
}

#[test]
fn intent_constructors_cover_all_shapes() {
    let intent = TypeDef::Enum {
        variants: vec![
            Variant {
                name: "Rematch".into(),
                shape: Shape::Unit,
            },
            Variant {
                name: "Add".into(),
                shape: Shape::Newtype(TypeRef::U8),
            },
            Variant {
                name: "Timer".into(),
                shape: Shape::Tuple(vec![TypeRef::String, TypeRef::U64]),
            },
            Variant {
                name: "Place".into(),
                shape: Shape::Struct(vec![
                    field("cell_index", TypeRef::U8),
                    field("class", TypeRef::Bool),
                ]),
            },
        ],
    };
    let intents = file(&doc(vec![], vec![("Intent", intent)]), "TestIntents.cs");
    assert!(
        intents.contains("public static class TestIntents"),
        "{intents}"
    );
    assert!(
        intents.contains("public static JToken Rematch() => \"Rematch\";"),
        "{intents}"
    );
    assert!(
        intents
            .contains("public static JToken Add(byte value) => new JObject { [\"Add\"] = value };"),
        "{intents}"
    );
    assert!(
        intents.contains("public static JToken Timer(string value0, ulong value1) => new JObject { [\"Timer\"] = new JArray(value0, value1) };"),
        "{intents}"
    );
    assert!(
        intents.contains("public static JToken Place(byte cellIndex, bool @class) => new JObject { [\"Place\"] = new JObject { [\"cell_index\"] = cellIndex, [\"class\"] = @class } };"),
        "{intents}"
    );
}

#[test]
fn owner_field_documented() {
    let mut secret = field("hand", TypeRef::List(Box::new(TypeRef::U8)));
    secret.vis = Vis::Owner;
    let state = file(&doc(vec![secret], vec![]), "TestState.cs");
    assert!(
        state.contains("/// <summary>Only your own entry is visible.</summary>\n        public global::Hearth.HearthList<byte> Hand"),
        "{state}"
    );
}

#[test]
fn root_carries_schema_and_header() {
    let state = file(&doc(vec![], vec![]), "TestState.cs");
    assert!(state.starts_with(unity::HEADER), "{state}");
    assert!(state.contains("#nullable disable"), "{state}");
    assert!(
        state.contains("[global::Hearth.HearthSchema(\"0123456789abcdef\")]"),
        "{state}"
    );
    assert!(
        state.contains("public const string SchemaHash = \"0123456789abcdef\";"),
        "{state}"
    );
    assert!(
        state.contains("public sealed partial class TestState"),
        "{state}"
    );
}
