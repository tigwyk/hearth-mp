use std::collections::BTreeMap;

use hearth_protocol::{PlayerId, Value};
use serde::de::DeserializeOwned;
use serde::Serialize;

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

/// Compile-time fingerprint of a state type's shape.
///
/// Two schema hashes match only when the synchronised shape matches; clients
/// send the hash in `HELLO` and a mismatch is a hard reject.
pub trait Schema {
    const HASH: u64;
}

macro_rules! schema_by_name {
    ($($ty:ty => $name:expr),* $(,)?) => {
        $(
            impl Schema for $ty {
                const HASH: u64 = fnv1a64($name);
            }
        )*
    };
}

schema_by_name!(
    bool => b"bool",
    i8 => b"i8",
    i16 => b"i16",
    i32 => b"i32",
    i64 => b"i64",
    u8 => b"u8",
    u16 => b"u16",
    u32 => b"u32",
    u64 => b"u64",
    f32 => b"f32",
    f64 => b"f64",
    String => b"String",
    PlayerId => b"PlayerId",
    () => b"()",
);

impl<T: Schema> Schema for Option<T> {
    const HASH: u64 = mix(fnv1a64(b"Option"), T::HASH);
}

impl<T: Schema> Schema for Vec<T> {
    const HASH: u64 = mix(fnv1a64(b"Vec"), T::HASH);
}

impl<T: Schema, const N: usize> Schema for [T; N] {
    const HASH: u64 = mix(mix(fnv1a64(b"array"), T::HASH), N as u64);
}

impl<K: Schema, V: Schema> Schema for BTreeMap<K, V> {
    const HASH: u64 = mix(mix(fnv1a64(b"BTreeMap"), K::HASH), V::HASH);
}

impl<K: Ord + Schema, V: Schema> Schema for SyncMap<K, V> {
    const HASH: u64 = mix(mix(fnv1a64(b"SyncMap"), K::HASH), V::HASH);
}

impl<A: Schema> Schema for (A,) {
    const HASH: u64 = mix(fnv1a64(b"tuple1"), A::HASH);
}

impl<A: Schema, B: Schema> Schema for (A, B) {
    const HASH: u64 = mix(mix(fnv1a64(b"tuple2"), A::HASH), B::HASH);
}

impl<A: Schema, B: Schema, C: Schema> Schema for (A, B, C) {
    const HASH: u64 = mix(mix(mix(fnv1a64(b"tuple3"), A::HASH), B::HASH), C::HASH);
}

impl<A: Schema, B: Schema, C: Schema, D: Schema> Schema for (A, B, C, D) {
    const HASH: u64 = mix(
        mix(mix(mix(fnv1a64(b"tuple4"), A::HASH), B::HASH), C::HASH),
        D::HASH,
    );
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
    /// This state as `viewer` may see it: synced fields, with owner-scoped
    /// fields filtered to the viewer.
    fn view(&self, viewer: &PlayerId) -> Value;

    /// The schema hash as the 16-char lowercase hex exchanged in `HELLO`.
    fn schema_hash_hex() -> String {
        format!("{:016x}", Self::HASH)
    }
}
