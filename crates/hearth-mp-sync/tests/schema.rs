use std::collections::BTreeMap;

use hearth_mp_protocol::PlayerId;
use hearth_mp_sync::{fnv1a64, OwnerScoped, Schema, SyncMap};
use serde_json::json;

fn pid(id: &str) -> PlayerId {
    PlayerId(id.to_owned())
}

#[test]
fn primitive_hashes_are_stable() {
    assert_eq!(<u8 as Schema>::HASH, fnv1a64(b"u8"));
    assert_eq!(<String as Schema>::HASH, fnv1a64(b"String"));
    assert_eq!(<PlayerId as Schema>::HASH, fnv1a64(b"PlayerId"));
}

#[test]
fn generic_hashes_depend_on_arguments() {
    assert_ne!(<Vec<u8> as Schema>::HASH, <Vec<u16> as Schema>::HASH);
    assert_ne!(<Option<String> as Schema>::HASH, <String as Schema>::HASH);
    assert_ne!(<(u8, u8) as Schema>::HASH, <(u8, u16) as Schema>::HASH);
    assert_ne!(<[u8; 2] as Schema>::HASH, <[u8; 3] as Schema>::HASH);
}

#[test]
fn syncmap_hash_differs_from_btreemap() {
    assert_ne!(
        <SyncMap<PlayerId, u8> as Schema>::HASH,
        <BTreeMap<PlayerId, u8> as Schema>::HASH
    );
}

#[test]
fn owner_scoped_view_only_contains_viewer() {
    let mut map: SyncMap<PlayerId, String> = SyncMap::default();
    map.insert(pid("a"), "secret-a".into());
    map.insert(pid("b"), "secret-b".into());

    assert_eq!(map.owner_view(&pid("a")), json!({"a": "secret-a"}));
    assert_eq!(map.owner_view(&pid("b")), json!({"b": "secret-b"}));
    assert_eq!(map.owner_view(&pid("c")), json!({}));
}
