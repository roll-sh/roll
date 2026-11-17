//! JSON Patch (RFC 6902) as the plugin applies it to its configuration (`add`, `remove`,
//! `replace`, `move`, `copy`, `test`): removing what isn't there changes nothing, and replacing
//! what isn't there is `Missing field "<name>"`.

use crate::json::Value;

/// Why a patch can't apply.
#[derive(Debug, PartialEq)]
pub enum PatchError {
    /// The body isn't a list of operations.
    Body,
    /// A path names a field that isn't there.
    Missing(String),
    /// A `test` failed or a path doesn't fit the document.
    Invalid(String),
}

/// Applies a patch (the request body) to `doc`.
pub fn apply(doc: &mut Value, patch: &Value) -> Result<(), PatchError> {
    let ops = patch.as_array().ok_or(PatchError::Body)?;
    for op in ops {
        let o = op.as_object().ok_or(PatchError::Body)?;
        let path = pointer(o.get("path").and_then(Value::as_str).ok_or(PatchError::Body)?)?;
        let value = || o.get("value").cloned().ok_or(PatchError::Body);
        match o.get("op").and_then(Value::as_str).ok_or(PatchError::Body)? {
            "add" => add(doc, &path, value()?)?,
            "remove" => {
                remove(doc, &path);
            }
            "replace" => {
                if get(doc, &path).is_none() {
                    return Err(PatchError::Missing(path.last().cloned().unwrap_or_default()));
                }
                remove(doc, &path);
                add(doc, &path, value()?)?;
            }
            "move" | "copy" => {
                let from = pointer(o.get("from").and_then(Value::as_str).ok_or(PatchError::Body)?)?;
                let v = get(doc, &from).cloned().ok_or_else(|| PatchError::Missing(from.last().cloned().unwrap_or_default()))?;
                if o.get("op").and_then(Value::as_str) == Some("move") {
                    remove(doc, &from);
                }
                add(doc, &path, v)?;
            }
            "test" => {
                if get(doc, &path) != Some(&value()?) {
                    return Err(PatchError::Invalid("test operation failed".into()));
                }
            }
            _ => return Err(PatchError::Body),
        }
    }
    Ok(())
}

/// A JSON pointer's tokens (`~1` is `/`, `~0` is `~`).
fn pointer(p: &str) -> Result<Vec<String>, PatchError> {
    if p.is_empty() {
        return Ok(Vec::new());
    }
    let rest = p.strip_prefix('/').ok_or_else(|| PatchError::Invalid(format!("invalid path [{p}]")))?;
    Ok(rest.split('/').map(|t| t.replace("~1", "/").replace("~0", "~")).collect())
}

fn get<'a>(doc: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(doc, |v, t| match v {
        Value::Obj(m) => m.get(t),
        Value::Arr(a) => t.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

fn get_mut<'a>(doc: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    path.iter().try_fold(doc, |v, t| match v {
        Value::Obj(m) => m.get_mut(t),
        Value::Arr(a) => t.parse::<usize>().ok().and_then(|i| a.get_mut(i)),
        _ => None,
    })
}

fn add(doc: &mut Value, path: &[String], v: Value) -> Result<(), PatchError> {
    let Some((last, parent)) = path.split_last() else {
        *doc = v;
        return Ok(());
    };
    let missing = || PatchError::Missing(parent.last().cloned().unwrap_or_default());
    match get_mut(doc, parent).ok_or_else(missing)? {
        Value::Obj(m) => {
            m.insert(last.clone(), v);
        }
        Value::Arr(a) if last == "-" => a.push(v),
        Value::Arr(a) => match last.parse::<usize>() {
            Ok(i) if i <= a.len() => a.insert(i, v),
            _ => return Err(PatchError::Invalid(format!("index [{last}] is out of bounds"))),
        },
        _ => return Err(missing()),
    }
    Ok(())
}

fn remove(doc: &mut Value, path: &[String]) -> Option<Value> {
    let (last, parent) = path.split_last()?;
    match get_mut(doc, parent)? {
        Value::Obj(m) => m.shift_remove(last),
        Value::Arr(a) => last.parse::<usize>().ok().filter(|i| *i < a.len()).map(|i| a.remove(i)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> Value {
        crate::json::parse(s.as_bytes()).unwrap().unwrap()
    }

    #[test]
    fn applies_like_the_plugin() {
        let mut d = json(r#"{"u1":{"backend_roles":["a"]},"u2":{}}"#);
        apply(&mut d, &json(r#"[{"op":"add","path":"/u1/backend_roles/-","value":"b"},{"op":"remove","path":"/u2"},{"op":"add","path":"/u3","value":{"x":1}}]"#)).unwrap();
        assert_eq!(d.to_json(), r#"{"u1":{"backend_roles":["a","b"]},"u3":{"x":1}}"#);
        assert_eq!(apply(&mut d, &json(r#"[{"op":"remove","path":"/nope"}]"#)), Ok(()));
        assert_eq!(apply(&mut d, &json(r#"[{"op":"replace","path":"/u1/bogus","value":1}]"#)), Err(PatchError::Missing("bogus".into())));
        assert_eq!(apply(&mut d, &json(r#"{"op":"add"}"#)), Err(PatchError::Body));
        assert_eq!(apply(&mut d, &json(r#"[{"op":"move","from":"/u3","path":"/a~1b"}]"#)), Ok(()));
        assert!(get(&d, &["a/b".to_string()]).is_some());
    }
}
