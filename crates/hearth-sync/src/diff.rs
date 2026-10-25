use hearth_protocol::{PatchOp, Value};

/// Compute the patch ops that turn `old` into `new`.
///
/// Two objects diff as removed `Del`s first, then added `MapInsert`s, then
/// shared keys recurse (all in `serde_json::Map` order). Two arrays emit one
/// `ListSplice` covering their changed middle. Everything else is a `Set`.
pub fn diff(old: &Value, new: &Value) -> Vec<PatchOp> {
    // shortcut: whole-view diffing is O(clients × state) per handler pass.
    // Upgrade path: in-place change tracking in SyncMap/SyncVec (v0.4) so only
    // dirty paths are diffed per client.
    let mut ops = Vec::new();
    diff_at(&mut ops, &mut Vec::new(), old, new);
    ops
}

fn diff_at(ops: &mut Vec<PatchOp>, path: &mut Vec<Value>, old: &Value, new: &Value) {
    if old == new {
        return;
    }
    match (old, new) {
        (Value::Object(old_map), Value::Object(new_map)) => {
            for key in old_map.keys() {
                if !new_map.contains_key(key) {
                    path.push(Value::String(key.clone()));
                    ops.push(PatchOp::Del { path: path.clone() });
                    path.pop();
                }
            }
            for (key, value) in new_map {
                if !old_map.contains_key(key) {
                    ops.push(PatchOp::MapInsert {
                        path: path.clone(),
                        key: key.clone(),
                        value: value.clone(),
                    });
                }
            }
            for (key, new_value) in new_map {
                if let Some(old_value) = old_map.get(key) {
                    path.push(Value::String(key.clone()));
                    diff_at(ops, path, old_value, new_value);
                    path.pop();
                }
            }
        }
        (Value::Array(old_items), Value::Array(new_items)) => {
            let prefix = old_items
                .iter()
                .zip(new_items)
                .take_while(|(old, new)| old == new)
                .count();
            let suffix = old_items[prefix..]
                .iter()
                .rev()
                .zip(new_items[prefix..].iter().rev())
                .take_while(|(old, new)| old == new)
                .count();
            let old_middle = &old_items[prefix..old_items.len() - suffix];
            let new_middle = &new_items[prefix..new_items.len() - suffix];
            match (u32::try_from(prefix), u32::try_from(old_middle.len())) {
                (Ok(index), Ok(delete)) => ops.push(PatchOp::ListSplice {
                    path: path.clone(),
                    index,
                    delete,
                    insert: new_middle.to_vec(),
                }),
                // Unreachable for JSON trees that fit the protocol's u32 indices.
                _ => ops.push(PatchOp::Set {
                    path: path.clone(),
                    value: new.clone(),
                }),
            }
        }
        _ => ops.push(PatchOp::Set {
            path: path.clone(),
            value: new.clone(),
        }),
    }
}
