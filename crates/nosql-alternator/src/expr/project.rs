//! ProjectionExpression / AttributesToGet: selecting document paths of an item.

use std::collections::BTreeMap;

use super::resolve::{RPath, Step};
use crate::json::Json;
use crate::value::Item;

/// A selection being built: a whole value, or some members of a map, or some elements of a
/// list (by source index).
enum Sel {
    Whole(Json),
    Map(Vec<(String, Sel)>),
    List(BTreeMap<u32, Sel>),
}

impl Sel {
    fn into_json(self) -> Json {
        match self {
            Sel::Whole(v) => v,
            Sel::Map(m) => Json::object().with(
                "M",
                Json::Object(m.into_iter().map(|(k, s)| (k, s.into_json())).collect()),
            ),
            Sel::List(l) => Json::object().with(
                "L",
                Json::Array(l.into_values().map(Sel::into_json).collect()),
            ),
        }
    }
}

/// Whether `steps` exist below `v`.
fn exists(v: &Json, steps: &[Step]) -> bool {
    let mut v = v;
    for s in steps {
        match super::eval::step(v, s) {
            Some(x) => v = x,
            None => return false,
        }
    }
    true
}

/// A fresh selection of `steps` below `v` (which must exist).
fn fresh(v: &Json, steps: &[Step]) -> Sel {
    let Some((first, rest)) = steps.split_first() else {
        return Sel::Whole(v.clone());
    };
    let Some(child) = super::eval::step(v, first) else {
        return Sel::Whole(v.clone());
    };
    match first {
        Step::Field(f) => Sel::Map(vec![(f.clone(), fresh(child, rest))]),
        Step::Index(i) => Sel::List(BTreeMap::from([(*i, fresh(child, rest))])),
    }
}

/// Adds the selection of `steps` below `v` (which must exist) to `sel`.
fn add(sel: &mut Sel, v: &Json, steps: &[Step]) {
    let Some((first, rest)) = steps.split_first() else {
        *sel = Sel::Whole(v.clone());
        return;
    };
    let Some(child) = super::eval::step(v, first) else {
        return;
    };
    match (sel, first) {
        (Sel::Map(members), Step::Field(f)) => match members.iter_mut().find(|(k, _)| k == f) {
            Some((_, s)) => add(s, child, rest),
            None => members.push((f.clone(), fresh(child, rest))),
        },
        (Sel::List(elems), Step::Index(i)) => match elems.get_mut(i) {
            Some(s) => add(s, child, rest),
            None => {
                elems.insert(*i, fresh(child, rest));
            }
        },
        // A whole value already covers the path.
        _ => {}
    }
}

/// The parts of `item` the paths select. A list keeps the selected elements in index order
/// (compacted); a map keeps the selected members. Paths that don't exist select nothing.
pub fn project(item: &Item, paths: &[RPath]) -> Item {
    let mut sels: Vec<(String, Sel)> = Vec::new();
    for p in paths {
        let Some(v) = item.get(&p.root) else { continue };
        if !exists(v, &p.steps) {
            continue;
        }
        match sels.iter_mut().find(|(k, _)| *k == p.root) {
            Some((_, s)) => add(s, v, &p.steps),
            None => sels.push((p.root.clone(), fresh(v, &p.steps))),
        }
    }
    sels.into_iter().map(|(k, s)| (k, s.into_json())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn item(s: &str) -> Item {
        parse(s.as_bytes())
            .unwrap()
            .as_object()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    fn p(root: &str, steps: Vec<Step>) -> RPath {
        RPath {
            root: root.into(),
            steps,
        }
    }

    #[test]
    fn nested_selection() {
        let it = item(
            r#"{"a":{"M":{"b":{"L":[{"N":"0"},{"N":"1"},{"M":{"c":{"S":"x"},"d":{"S":"y"}}}]},"e":{"S":"z"}}},"f":{"S":"q"}}"#,
        );
        let out = project(
            &it,
            &[
                p(
                    "a",
                    vec![
                        Step::Field("b".into()),
                        Step::Index(2),
                        Step::Field("d".into()),
                    ],
                ),
                p("a", vec![Step::Field("b".into()), Step::Index(0)]),
                p("f", vec![]),
                p("missing", vec![]),
                p("a", vec![Step::Field("zz".into())]),
                p("f", vec![Step::Index(0)]),
            ],
        );
        let j = Json::Object(out.into_iter().collect()).to_json_string();
        assert_eq!(
            j,
            r#"{"a":{"M":{"b":{"L":[{"N":"0"},{"M":{"d":{"S":"y"}}}]}}},"f":{"S":"q"}}"#
        );
    }
}
