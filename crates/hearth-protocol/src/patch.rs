use serde_json::Value;

use crate::frame::PatchOp;

/// Error returned by [`apply`]. The bad path is the operation's full path.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    #[error("bad patch path: {0:?}")]
    BadPath(Vec<Value>),
}

/// Apply one patch op in place. See `docs/protocol.md` and the conformance
/// fixture `tests/fixtures/patch_cases.json`.
pub fn apply(state: &mut Value, op: &PatchOp) -> Result<(), PatchError> {
    match op {
        PatchOp::Set { path, value } => match path.split_last() {
            None => {
                *state = value.clone();
                Ok(())
            }
            Some((last, parent_path)) => {
                let parent = navigate(state, parent_path, path)?;
                set_child(parent, last, value.clone(), path)
            }
        },
        PatchOp::Del { path } => {
            let Some((last, parent_path)) = path.split_last() else {
                return Err(PatchError::BadPath(path.clone()));
            };
            let parent = navigate(state, parent_path, path)?;
            match (parent, last) {
                (Value::Object(map), Value::String(key)) => map
                    .remove(key)
                    .map(|_| ())
                    .ok_or_else(|| PatchError::BadPath(path.clone())),
                (Value::Array(items), Value::Number(_)) => {
                    let index = list_index(last, path)?;
                    if index < items.len() {
                        items.remove(index);
                        Ok(())
                    } else {
                        Err(PatchError::BadPath(path.clone()))
                    }
                }
                _ => Err(PatchError::BadPath(path.clone())),
            }
        }
        PatchOp::MapInsert { path, key, value } => {
            let target = navigate(state, path, path)?;
            match target {
                Value::Object(map) => {
                    map.insert(key.clone(), value.clone());
                    Ok(())
                }
                _ => Err(PatchError::BadPath(path.clone())),
            }
        }
        PatchOp::ListSplice {
            path,
            index,
            delete,
            insert,
        } => {
            let target = navigate(state, path, path)?;
            match target {
                Value::Array(items) => {
                    let start =
                        usize::try_from(*index).map_err(|_| PatchError::BadPath(path.clone()))?;
                    let delete =
                        usize::try_from(*delete).map_err(|_| PatchError::BadPath(path.clone()))?;
                    if start > items.len() || start + delete > items.len() {
                        return Err(PatchError::BadPath(path.clone()));
                    }
                    items.splice(start..start + delete, insert.iter().cloned());
                    Ok(())
                }
                _ => Err(PatchError::BadPath(path.clone())),
            }
        }
    }
}

/// Walk `path` from `root`, returning the value at its end.
fn navigate<'a>(
    root: &'a mut Value,
    path: &[Value],
    full: &[Value],
) -> Result<&'a mut Value, PatchError> {
    let mut cursor = root;
    for segment in path {
        cursor = match (cursor, segment) {
            (Value::Object(map), Value::String(key)) => map
                .get_mut(key)
                .ok_or_else(|| PatchError::BadPath(full.to_vec()))?,
            (Value::Array(items), Value::Number(_)) => {
                let index = list_index(segment, full)?;
                items
                    .get_mut(index)
                    .ok_or_else(|| PatchError::BadPath(full.to_vec()))?
            }
            _ => return Err(PatchError::BadPath(full.to_vec())),
        };
    }
    Ok(cursor)
}

/// Set the last path segment. Object keys are created when absent; list
/// indices must already exist.
fn set_child(
    parent: &mut Value,
    segment: &Value,
    value: Value,
    full: &[Value],
) -> Result<(), PatchError> {
    match (parent, segment) {
        (Value::Object(map), Value::String(key)) => {
            map.insert(key.clone(), value);
            Ok(())
        }
        (Value::Array(items), Value::Number(_)) => {
            let index = list_index(segment, full)?;
            let slot = items
                .get_mut(index)
                .ok_or_else(|| PatchError::BadPath(full.to_vec()))?;
            *slot = value;
            Ok(())
        }
        _ => Err(PatchError::BadPath(full.to_vec())),
    }
}

fn list_index(segment: &Value, full: &[Value]) -> Result<usize, PatchError> {
    segment
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| PatchError::BadPath(full.to_vec()))
}
