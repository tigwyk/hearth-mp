use std::collections::BTreeMap;

use hearth_protocol::{PlayerId, Value};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::SyncMap;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64-bit hash.
pub const fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    hash
}

/// Combine two hashes: [`fnv1a64`] over the 16 little-endian bytes of `a` then `b`.
pub const fn mix(a: u64, b: u64) -> u64 {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let mut bytes = [0u8; 16];
    let mut i = 0;
    while i < 8 {
        bytes[i] = a[i];
        bytes[i + 8] = b[i];
        i += 1;
    }
    fnv1a64(&bytes)
}

/// A type as it appears in the machine-readable schema.
///
/// Serialised with serde's default external tagging plus `snake_case` names:
/// `"player_id"`, `{"list": "u8"}`, `{"sync_map": ["player_id", "u8"]}`, and so
/// on. Codegen consumes this shape verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TypeRef {
    /// `bool`.
    Bool,
    /// `i8`.
    I8,
    /// `i16`.
    I16,
    /// `i32`.
    I32,
    /// `i64`.
    I64,
    /// `u8`.
    U8,
    /// `u16`.
    U16,
    /// `u32`.
    U32,
    /// `u64`.
    U64,
    /// `f32`.
    F32,
    /// `f64`.
    F64,
    /// `String`.
    String,
    /// `PlayerId` (a string on the wire).
    PlayerId,
    /// `()`: `null` on the wire.
    Unit,
    /// `Option<T>`: `null` or a value.
    Option(Box<TypeRef>),
    /// `Vec<T>` or a synced list.
    List(Box<TypeRef>),
    /// A fixed-size array: the element type and the length.
    Array(Box<TypeRef>, u64),
    /// A map; keys travel as strings.
    Map(Box<TypeRef>, Box<TypeRef>),
    /// A [`SyncMap`], diffed per key.
    SyncMap(Box<TypeRef>, Box<TypeRef>),
    /// A tuple: an array with one value per element type.
    Tuple(Vec<TypeRef>),
    /// A type registered in the [`Registry`] under this Rust type name.
    Named(String),
}

/// Who may see a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vis {
    /// Every client sees the field.
    Public,
    /// Each client sees only its own entry (`#[sync(owner)]`).
    Owner,
}

/// One field of a struct or struct variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    /// The field name on the wire.
    pub name: String,
    /// The field's type.
    pub ty: TypeRef,
    /// Who may see it.
    pub vis: Vis,
}

/// One enum variant and its payload shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variant {
    /// The variant name on the wire.
    pub name: String,
    /// Its payload.
    pub shape: Shape,
}

/// The payload shape of an enum variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Shape {
    /// No payload.
    Unit,
    /// One value.
    Newtype(TypeRef),
    /// Positional values.
    Tuple(Vec<TypeRef>),
    /// Named fields.
    Struct(Vec<Field>),
}

/// The machine-readable shape of a type: a struct or an enum.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TypeDef {
    /// A struct: one value per field.
    Struct {
        /// The fields, in declaration order.
        fields: Vec<Field>,
    },
    /// An enum, in serde's external tagging.
    Enum {
        /// The variants, in declaration order.
        variants: Vec<Variant>,
    },
}

/// Every named type reachable from one room's state and intent types.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    /// Type name to `(schema hash, definition)`.
    pub types: BTreeMap<String, (u64, TypeDef)>,
}

impl Registry {
    /// Register `name`. Registering the same name with the same hash again is a
    /// no-op; registering it with a different hash is a hard error, because
    /// generated clients need unique type names.
    pub fn add(&mut self, name: &str, hash: u64, def: impl FnOnce(&mut Registry) -> TypeDef) {
        if let Some((existing, _)) = self.types.get(name) {
            if *existing == hash {
                return;
            }
            panic!(
                "two schema types are named `{name}`; rename one (codegen needs unique type names)"
            );
        }
        let type_def = def(self);
        self.types.insert(name.to_owned(), (hash, type_def));
    }
}

/// Compile-time fingerprint of a state type's shape.
///
/// Two schema hashes match only when the synchronised shape matches; clients
/// send the hash in `HELLO` and a mismatch is a hard reject.
pub trait Schema {
    /// The type's schema hash: changes whenever its wire shape does.
    const HASH: u64;

    /// This type's shape, registering every named type it reaches in `reg`.
    fn describe(reg: &mut Registry) -> TypeRef;
}

macro_rules! schema_by_name {
    ($($ty:ty => $name:expr, $type_ref:expr),* $(,)?) => {
        $(
            impl Schema for $ty {
                const HASH: u64 = fnv1a64($name);

                fn describe(_reg: &mut Registry) -> TypeRef {
                    $type_ref
                }
            }
        )*
    };
}

schema_by_name!(
    bool => b"bool", TypeRef::Bool,
    i8 => b"i8", TypeRef::I8,
    i16 => b"i16", TypeRef::I16,
    i32 => b"i32", TypeRef::I32,
    i64 => b"i64", TypeRef::I64,
    u8 => b"u8", TypeRef::U8,
    u16 => b"u16", TypeRef::U16,
    u32 => b"u32", TypeRef::U32,
    u64 => b"u64", TypeRef::U64,
    f32 => b"f32", TypeRef::F32,
    f64 => b"f64", TypeRef::F64,
    String => b"String", TypeRef::String,
    PlayerId => b"PlayerId", TypeRef::PlayerId,
    () => b"()", TypeRef::Unit,
);

impl<T: Schema> Schema for Option<T> {
    const HASH: u64 = mix(fnv1a64(b"Option"), T::HASH);

    fn describe(reg: &mut Registry) -> TypeRef {
        TypeRef::Option(Box::new(T::describe(reg)))
    }
}

impl<T: Schema> Schema for Vec<T> {
    const HASH: u64 = mix(fnv1a64(b"Vec"), T::HASH);

    fn describe(reg: &mut Registry) -> TypeRef {
        TypeRef::List(Box::new(T::describe(reg)))
    }
}

impl<T: Schema, const N: usize> Schema for [T; N] {
    const HASH: u64 = mix(mix(fnv1a64(b"array"), T::HASH), N as u64);

    fn describe(reg: &mut Registry) -> TypeRef {
        TypeRef::Array(Box::new(T::describe(reg)), N as u64)
    }
}

impl<K: Schema, V: Schema> Schema for BTreeMap<K, V> {
    const HASH: u64 = mix(mix(fnv1a64(b"BTreeMap"), K::HASH), V::HASH);

    fn describe(reg: &mut Registry) -> TypeRef {
        TypeRef::Map(Box::new(K::describe(reg)), Box::new(V::describe(reg)))
    }
}

impl<K: Ord + Schema, V: Schema> Schema for SyncMap<K, V> {
    const HASH: u64 = mix(mix(fnv1a64(b"SyncMap"), K::HASH), V::HASH);

    fn describe(reg: &mut Registry) -> TypeRef {
        TypeRef::SyncMap(Box::new(K::describe(reg)), Box::new(V::describe(reg)))
    }
}

impl<A: Schema> Schema for (A,) {
    const HASH: u64 = mix(fnv1a64(b"tuple1"), A::HASH);

    fn describe(reg: &mut Registry) -> TypeRef {
        TypeRef::Tuple(vec![A::describe(reg)])
    }
}

impl<A: Schema, B: Schema> Schema for (A, B) {
    const HASH: u64 = mix(mix(fnv1a64(b"tuple2"), A::HASH), B::HASH);

    fn describe(reg: &mut Registry) -> TypeRef {
        TypeRef::Tuple(vec![A::describe(reg), B::describe(reg)])
    }
}

impl<A: Schema, B: Schema, C: Schema> Schema for (A, B, C) {
    const HASH: u64 = mix(mix(mix(fnv1a64(b"tuple3"), A::HASH), B::HASH), C::HASH);

    fn describe(reg: &mut Registry) -> TypeRef {
        TypeRef::Tuple(vec![A::describe(reg), B::describe(reg), C::describe(reg)])
    }
}

impl<A: Schema, B: Schema, C: Schema, D: Schema> Schema for (A, B, C, D) {
    const HASH: u64 = mix(
        mix(mix(mix(fnv1a64(b"tuple4"), A::HASH), B::HASH), C::HASH),
        D::HASH,
    );

    fn describe(reg: &mut Registry) -> TypeRef {
        TypeRef::Tuple(vec![
            A::describe(reg),
            B::describe(reg),
            C::describe(reg),
            D::describe(reg),
        ])
    }
}

/// A field whose value is filtered per viewer before it leaves the server.
pub trait OwnerScoped {
    /// The value `viewer` may see: only their own entry, or `{}`.
    fn owner_view(&self, viewer: &PlayerId) -> Value;
}

impl<V: Serialize> OwnerScoped for SyncMap<PlayerId, V> {
    fn owner_view(&self, viewer: &PlayerId) -> Value {
        let mut map = serde_json::Map::new();
        if let Some(value) = self.get(viewer) {
            map.insert(viewer.0.clone(), crate::__private::to_value(value));
        }
        Value::Object(map)
    }
}

/// Durable room state that is synchronised to clients.
pub trait HearthState: Schema + Serialize + DeserializeOwned + Default + 'static {
    /// Whether this state has any `#[sync(owner)]` field. The derive emits the
    /// exact answer; the conservative default keeps hand-written impls safe.
    const HAS_OWNER: bool = true;

    /// This state's `#[sync]` (public) fields only: the part every client sees
    /// identically, so the runtime can diff it once per handler pass.
    fn public_view(&self) -> Value;

    /// Only this state's `#[sync(owner)]` fields, already filtered to `viewer`:
    /// one key per owner field, each holding only the viewer's own entry.
    fn owner_view(&self, viewer: &PlayerId) -> Value;

    /// This state as `viewer` may see it: synced fields, with owner-scoped
    /// fields filtered to the viewer.
    fn view(&self, viewer: &PlayerId) -> Value {
        let mut map = match self.public_view() {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        if let Value::Object(owner) = self.owner_view(viewer) {
            map.extend(owner);
        }
        Value::Object(map)
    }

    /// The **state-only** schema hash as 16-char lowercase hex. The wire
    /// `HELLO.schema_hash` is the room-level hash covering state *and* intent;
    /// see `hearth_server::schema_hash_hex`.
    fn schema_hash_hex() -> String {
        format!("{:016x}", Self::HASH)
    }
}
