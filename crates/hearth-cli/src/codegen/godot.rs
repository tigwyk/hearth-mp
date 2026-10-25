//! GDScript generation for the typed state mirror.
//!
//! Plan v0.2a Task 7: one `HearthObject` subclass per Rust struct in a room's
//! schema, one `HearthEnum` subclass per Rust enum, and one `<Room>Intents`
//! helper with a typed constructor per intent variant. The generated classes
//! are thin metadata over the runtime contract in `godot/addons/hearth/`
//! (`hearth_typed.gd`, `hearth_map.gd`, `hearth_list.gd`, `hearth_enum.gd`).
//!
//! Files are named after the class in snake case. The class name is the room
//! name plus the Rust type name in PascalCase, which keeps generated names
//! unique across rooms and clear of Godot built-ins (`Timer`, `Node`, ...).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use hearth_server::SchemaDoc;
use hearth_sync::{Field, Shape, TypeDef, TypeRef, Variant, Vis};

use crate::codegen::GENERATED_HEADER;

/// Render one GDScript file per type, plus one intent helper per room.
pub fn generate(docs: &[SchemaDoc]) -> Result<Vec<(String, String)>, String> {
    let mut files = Vec::new();
    for doc in docs {
        for (type_name, definition) in &doc.types {
            let class = class_name(&doc.room, type_name);
            let file = format!("{}.gd", snake_case(&class));
            let contents = match definition {
                TypeDef::Struct { fields } => {
                    struct_class(doc, &class, type_name, fields, &doc.types)
                }
                TypeDef::Enum { variants } => enum_class(&doc.room, &class, variants, &doc.types),
            };
            files.push((file, contents));
        }
        // The room's intent type gets constructors instead of a state mirror:
        // intents are sent as JSON, never mirrored.
        if let Some(TypeDef::Enum { variants }) = doc.types.get(&doc.intent) {
            let class = format!("{}Intents", pascal_case(&doc.room));
            let file = format!("{}.gd", snake_case(&class));
            files.push((file, intents_class(&doc.room, &class, variants)));
        }
    }
    Ok(files)
}

/// One `HearthObject` subclass: typed vars, per-field signals and metadata.
fn struct_class(
    doc: &SchemaDoc,
    class: &str,
    type_name: &str,
    fields: &[Field],
    all_types: &BTreeMap<String, TypeDef>,
) -> String {
    let mut out = header(class, "HearthObject");
    if doc.state == type_name {
        writeln!(out, "const SCHEMA_HASH := \"{}\"", doc.hash).unwrap();
        out.push('\n');
    }

    let signalled: Vec<&Field> = fields
        .iter()
        .filter(|field| has_signal(&field.ty, all_types))
        .collect();
    for field in &signalled {
        writeln!(out, "signal {}_changed(value)", gd_name(&field.name)).unwrap();
    }
    if !signalled.is_empty() {
        out.push('\n');
    }

    for field in fields {
        if field.vis == Vis::Owner {
            out.push_str("## Only your own entry is visible.\n");
        }
        writeln!(
            out,
            "var {}: {}",
            gd_name(&field.name),
            gd_type(&doc.room, &field.ty, all_types)
        )
        .unwrap();
    }
    if !fields.is_empty() {
        out.push('\n');
    }
    out.push('\n');

    out.push_str("static func _hearth_meta() -> Dictionary:\n\treturn {\n");
    if fields.is_empty() {
        out.push_str("\t\t\"fields\": [],\n");
    } else {
        out.push_str("\t\t\"fields\": [\n");
        for field in fields {
            writeln!(
                out,
                "\t\t\t{{\"gd\": \"{}\", \"wire\": \"{}\", \"ty\": {}}},",
                gd_name(&field.name),
                field.name,
                gd_value(&type_ref_json(&field.ty))
            )
            .unwrap();
        }
        out.push_str("\t\t],\n");
    }
    let referenced = referenced_types(fields.iter().map(|field| &field.ty));
    write_types_dict(&mut out, &doc.room, &referenced);
    out.push_str("\t}\n");
    out
}

/// One `HearthEnum` subclass: variant constants and metadata.
fn enum_class(
    room: &str,
    class: &str,
    variants: &[Variant],
    all_types: &BTreeMap<String, TypeDef>,
) -> String {
    let mut out = header(class, "HearthEnum");
    for variant in variants {
        writeln!(
            out,
            "const {} := \"{}\"",
            snake_case(&variant.name).to_ascii_uppercase(),
            variant.name
        )
        .unwrap();
    }
    if !variants.is_empty() {
        out.push('\n');
    }
    out.push('\n');

    out.push_str("static func _hearth_meta() -> Dictionary:\n\treturn {\n");
    if variants.is_empty() {
        out.push_str("\t\t\"variants\": [],\n");
    } else {
        out.push_str("\t\t\"variants\": [\n");
        for variant in variants {
            writeln!(
                out,
                "\t\t\t{{\"name\": \"{}\", \"shape\": {}}},",
                variant.name,
                gd_value(&shape_json(&variant.shape))
            )
            .unwrap();
        }
        out.push_str("\t\t],\n");
    }
    let mut referenced = BTreeSet::new();
    for variant in variants {
        for name in shape_types(&variant.shape) {
            if all_types.contains_key(&name) {
                referenced.insert(name);
            }
        }
    }
    write_types_dict(&mut out, room, &referenced);
    out.push_str("\t}\n");
    out
}

/// `<Room>Intents`: one static constructor per intent variant, built with
/// serde's external tagging so the returned value is already wire-shaped.
fn intents_class(room: &str, class: &str, variants: &[Variant]) -> String {
    let mut out = header(class, "RefCounted");
    out.push('\n');
    for (index, variant) in variants.iter().enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        let function = snake_case(&variant.name);
        match &variant.shape {
            Shape::Unit => {
                writeln!(out, "static func {function}() -> String:").unwrap();
                writeln!(out, "\treturn \"{}\"", variant.name).unwrap();
            }
            Shape::Newtype(ty) => {
                writeln!(
                    out,
                    "static func {function}(value{}) -> Dictionary:",
                    annotation(room, ty)
                )
                .unwrap();
                writeln!(out, "\treturn {{\"{}\": value}}", variant.name).unwrap();
            }
            Shape::Tuple(types) => {
                let parameters = types
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| format!("value{index}{}", annotation(room, ty)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let values = (0..types.len())
                    .map(|index| format!("value{index}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(out, "static func {function}({parameters}) -> Dictionary:").unwrap();
                writeln!(out, "\treturn {{\"{}\": [{values}]}}", variant.name).unwrap();
            }
            Shape::Struct(fields) => {
                let parameters = fields
                    .iter()
                    .map(|field| format!("{}{}", gd_name(&field.name), annotation(room, &field.ty)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let pairs = fields
                    .iter()
                    .map(|field| format!("\"{}\": {}", field.name, gd_name(&field.name)))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(out, "static func {function}({parameters}) -> Dictionary:").unwrap();
                writeln!(out, "\treturn {{\"{}\": {{{pairs}}}}}", variant.name).unwrap();
            }
        }
    }
    out
}

/// `# header`, then `class_name` and `extends`, then one blank line.
fn header(class: &str, base: &str) -> String {
    format!("{GENERATED_HEADER}\nclass_name {class}\nextends {base}\n\n")
}

/// The `"types": {...}` entry of a class's metadata: Rust type name -> preload
/// of the generated sibling script. Relative preload paths resolve against the
/// generated script's own directory, so the output works in any project folder.
fn write_types_dict(out: &mut String, room: &str, types: &BTreeSet<String>) {
    if types.is_empty() {
        out.push_str("\t\t\"types\": {},\n");
        return;
    }
    out.push_str("\t\t\"types\": {\n");
    for name in types {
        writeln!(
            out,
            "\t\t\t\"{name}\": preload(\"{}.gd\"),",
            snake_case(&class_name(room, name))
        )
        .unwrap();
    }
    out.push_str("\t\t},\n");
}

/// `Named` types referenced anywhere inside `types`.
fn referenced_types<'a>(types: impl Iterator<Item = &'a TypeRef>) -> BTreeSet<String> {
    let mut named = BTreeSet::new();
    for ty in types {
        collect_named(ty, &mut named);
    }
    named
}

fn shape_types(shape: &Shape) -> Vec<String> {
    let mut named = BTreeSet::new();
    match shape {
        Shape::Unit => {}
        Shape::Newtype(ty) => collect_named(ty, &mut named),
        Shape::Tuple(types) => {
            for ty in types {
                collect_named(ty, &mut named);
            }
        }
        Shape::Struct(fields) => {
            for field in fields {
                collect_named(&field.ty, &mut named);
            }
        }
    }
    named.into_iter().collect()
}

fn collect_named(ty: &TypeRef, named: &mut BTreeSet<String>) {
    match ty {
        TypeRef::Option(inner) | TypeRef::List(inner) => collect_named(inner, named),
        TypeRef::Array(inner, _) => collect_named(inner, named),
        TypeRef::Map(key, value) | TypeRef::SyncMap(key, value) => {
            collect_named(key, named);
            collect_named(value, named);
        }
        TypeRef::Tuple(items) => {
            for item in items {
                collect_named(item, named);
            }
        }
        TypeRef::Named(name) => {
            named.insert(name.clone());
        }
        _ => {}
    }
}

/// Whether the runtime emits a `<field>_changed` signal for this field type.
/// Containers emit their own signals; object fields reload in place.
fn has_signal(ty: &TypeRef, all_types: &BTreeMap<String, TypeDef>) -> bool {
    match ty {
        TypeRef::List(_) | TypeRef::Array(..) | TypeRef::Map(..) | TypeRef::SyncMap(..) => false,
        TypeRef::Named(name) => matches!(all_types.get(name), Some(TypeDef::Enum { .. })),
        _ => true,
    }
}

/// The GDScript type of one field.
fn gd_type(room: &str, ty: &TypeRef, all_types: &BTreeMap<String, TypeDef>) -> String {
    match ty {
        TypeRef::Bool => "bool".to_owned(),
        TypeRef::I8
        | TypeRef::I16
        | TypeRef::I32
        | TypeRef::I64
        | TypeRef::U8
        | TypeRef::U16
        | TypeRef::U32
        | TypeRef::U64 => "int".to_owned(),
        TypeRef::F32 | TypeRef::F64 => "float".to_owned(),
        TypeRef::String | TypeRef::PlayerId => "String".to_owned(),
        TypeRef::List(_) | TypeRef::Array(..) => "HearthList".to_owned(),
        TypeRef::Map(..) | TypeRef::SyncMap(..) => "HearthMap".to_owned(),
        TypeRef::Named(name) if all_types.contains_key(name) => class_name(room, name),
        _ => "Variant".to_owned(),
    }
}

/// A constructor parameter annotation, for the types the plan names explicitly.
fn annotation(room: &str, ty: &TypeRef) -> String {
    match ty {
        TypeRef::Bool => ": bool".to_owned(),
        TypeRef::I8
        | TypeRef::I16
        | TypeRef::I32
        | TypeRef::I64
        | TypeRef::U8
        | TypeRef::U16
        | TypeRef::U32
        | TypeRef::U64 => ": int".to_owned(),
        TypeRef::F32 | TypeRef::F64 => ": float".to_owned(),
        TypeRef::String | TypeRef::PlayerId => ": String".to_owned(),
        TypeRef::Named(name) => format!(": {}", class_name(room, name)),
        _ => String::new(),
    }
}

/// GDScript keywords, plus the operators Godot's docs list as reserved, plus
/// `Object`/`RefCounted` members that must not become field names.
const RESERVED: &[&str] = &[
    // GDScript 4 keywords.
    "if",
    "elif",
    "else",
    "for",
    "while",
    "match",
    "when",
    "break",
    "continue",
    "pass",
    "return",
    "class",
    "class_name",
    "extends",
    "is",
    "in",
    "as",
    "self",
    "super",
    "signal",
    "func",
    "static",
    "const",
    "enum",
    "var",
    "breakpoint",
    "preload",
    "await",
    "yield",
    "assert",
    "void",
    "PI",
    "TAU",
    "INF",
    "NAN", // Reserved operators.
    "not",
    "and",
    "or",
    // `Object` / `RefCounted` members (plan Task 7's explicit list).
    "get",
    "set",
    "free",
    "notification",
    "connect",
    "disconnect",
    "emit_signal",
    "get_class",
    "is_class",
    "script",
];

/// A field name that is legal to declare in GDScript. The wire name is kept in
/// the metadata, so only the generated identifier changes.
fn gd_name(name: &str) -> String {
    if RESERVED.contains(&name) {
        format!("{name}_")
    } else {
        name.to_owned()
    }
}

/// `TicTacToe` + `Board` -> `TicTacToeBoard`.
fn class_name(room: &str, type_name: &str) -> String {
    format!("{}{}", pascal_case(room), pascal_case(type_name))
}

/// `TicTacToe` -> `TicTacToe` (already PascalCase); `foo_bar` -> `FooBar`.
fn pascal_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut capitalize = true;
    for character in name.chars() {
        if character == '_' || character == '-' {
            capitalize = true;
            continue;
        }
        if capitalize {
            out.extend(character.to_uppercase());
            capitalize = false;
        } else {
            out.push(character);
        }
    }
    out
}

/// `TicTacToeBoard` -> `tic_tac_toe_board`.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (index, character) in name.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index != 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

fn type_ref_json(ty: &TypeRef) -> serde_json::Value {
    serde_json::to_value(ty).expect("a TypeRef always serialises as JSON")
}

fn shape_json(shape: &Shape) -> serde_json::Value {
    match shape {
        Shape::Unit => serde_json::json!("unit"),
        Shape::Newtype(ty) => serde_json::json!({"newtype": type_ref_json(ty)}),
        Shape::Tuple(types) => serde_json::json!({
            "tuple": types.iter().map(type_ref_json).collect::<Vec<_>>(),
        }),
        Shape::Struct(fields) => serde_json::json!({
            "struct": fields
                .iter()
                .map(|field| serde_json::json!({"name": field.name, "ty": type_ref_json(&field.ty)}))
                .collect::<Vec<_>>(),
        }),
    }
}

/// Render a JSON value as a GDScript literal (`{"a": ["b", 1]}` with spaces).
fn gd_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text) => gd_string(text),
        serde_json::Value::Array(items) => {
            let items = items.iter().map(gd_value).collect::<Vec<_>>().join(", ");
            format!("[{items}]")
        }
        serde_json::Value::Object(map) => {
            let entries = map
                .iter()
                .map(|(key, value)| format!("{}: {}", gd_string(key), gd_value(value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{entries}}}")
        }
    }
}

fn gd_string(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}
