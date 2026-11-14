//! Parent/child relations of an index's `join` field (Elasticsearch's `Joiner`).
//!
//! An index has at most one `join` field, at the mapping root. Its `relations` map each parent
//! type to one or more child types; a type can be both (a grandparent chain). Documents index:
//!
//! - the relation name in the join field itself (terms + doc values: the type filters);
//! - a child: its parent's id in `<join>#<parent type>` (sorted doc values + terms);
//! - a parent: its own id in `<join>#<own type>`.
//!
//! So both sides of a relation carry the same join value in the same `#` field, which is what
//! the global-ordinals join of `has_child` / `has_parent` and the inner hits match on.

use crate::json::Value;
use crate::mapping::{FieldMapper, Mapper, Mapping};

/// One `join` field's relations, in mapping order.
#[derive(Clone, Debug, PartialEq)]
pub struct Joiner {
    /// The join field's name (root level).
    pub field: String,
    /// Parent type → its child types.
    relations: Vec<(String, Vec<String>)>,
}

impl Joiner {
    /// The relations of a `join` field mapper.
    pub fn from_mapper(f: &FieldMapper) -> Joiner {
        let mut relations = Vec::new();
        if let Some(Value::Obj(rels)) = f.raw("relations") {
            for (parent, children) in rels {
                let children = match children {
                    Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
                    v => vec![v.to_es_string()],
                };
                relations.push((parent.clone(), children));
            }
        }
        Joiner { field: f.name.clone(), relations }
    }

    /// The join field of `mapping` (`Joiner.getJoiner`), if any.
    pub fn of(mapping: &Mapping) -> Option<Joiner> {
        mapping.root.children.values().find_map(|m| match m {
            Mapper::Field(f) if f.ty == "join" => Some(Joiner::from_mapper(f)),
            _ => None,
        })
    }

    /// The parent type of `child`.
    pub fn parent_of(&self, child: &str) -> Option<&str> {
        self.relations.iter().find(|(_, c)| c.iter().any(|x| x == child)).map(|(p, _)| p.as_str())
    }

    pub fn is_child(&self, t: &str) -> bool {
        self.parent_of(t).is_some()
    }

    pub fn is_parent(&self, t: &str) -> bool {
        self.relations.iter().any(|(p, _)| p == t)
    }

    pub fn known(&self, t: &str) -> bool {
        self.is_child(t) || self.is_parent(t)
    }

    /// The child types of `parent`.
    pub fn children(&self, parent: &str) -> &[String] {
        self.relations.iter().find(|(p, _)| p == parent).map(|(_, c)| c.as_slice()).unwrap_or(&[])
    }

    /// The field holding a `child`'s parent id (`<join>#<parent>`).
    pub fn parent_join_field(&self, child: &str) -> Option<String> {
        self.parent_of(child).map(|p| format!("{}#{p}", self.field))
    }

    /// The field holding a `parent`'s own id (`<join>#<parent>`).
    pub fn child_join_field(&self, parent: &str) -> String {
        format!("{}#{parent}", self.field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::MappingCtx;

    fn joiner() -> Joiner {
        let v = crate::json::parse(br#"{"properties":{"t":{"type":"text"},"j":{"type":"join","relations":{"q":["a","c"],"a":"v"}}}}"#).unwrap().unwrap();
        let m = Mapping::parse(v.as_object().unwrap(), &MappingCtx::default()).unwrap();
        Joiner::of(&m).unwrap()
    }

    #[test]
    fn relations() {
        let j = joiner();
        assert_eq!(j.field, "j");
        assert_eq!(j.parent_of("a"), Some("q"));
        assert_eq!(j.parent_of("v"), Some("a"));
        assert_eq!(j.parent_of("q"), None);
        assert!(j.is_parent("a") && j.is_child("a"));
        assert!(j.known("c") && !j.known("x"));
        assert_eq!(j.children("q"), ["a".to_string(), "c".to_string()]);
        assert_eq!(j.parent_join_field("v").as_deref(), Some("j#a"));
        assert_eq!(j.child_join_field("q"), "j#q");
    }
}
