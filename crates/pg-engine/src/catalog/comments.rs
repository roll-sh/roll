//! COMMENT ON (round 19, agent 76): pg_description (and the database's copy of
//! pg_shdescription) as a record per object (catalog format 2), so a DDL commit writes the
//! comments of the objects it changed, not every comment in the catalog header.
//!
//! An object's comments are by sub-id, each with when it was set (`tick`, from
//! `Catalog::comment_tick`): pg_description's heap order, in which a comment set again is a
//! new row at the end, is the order of the ticks.

use super::Catalog;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// One object's comments: (sub-id, tick, text), by sub-id. (An empty text is kept: it hides
/// a built-in object's description.)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjComments(pub Vec<(i32, u64, String)>);

/// The comments of a catalog, by object.
pub type Comments = super::pmap::PMap<Arc<ObjComments>>;

/// `comments` from a list of (object, sub-id, text) in pg_description's order (the catalog
/// header's before format 2), ticks counting from 1.
pub fn from_list(list: Vec<(u32, i32, String)>) -> Comments {
    let mut m: std::collections::BTreeMap<u32, ObjComments> = std::collections::BTreeMap::new();
    for (i, (o, sub, text)) in list.into_iter().enumerate() {
        let e = m.entry(o).or_default();
        e.0.retain(|c| c.0 != sub);
        e.0.push((sub, i as u64 + 1, text));
        e.0.sort_by_key(|c| c.0);
    }
    m.into_iter().map(|(o, c)| (o, Arc::new(c))).collect()
}

/// (Stored as a map of objects; a list of (object, sub-id, text) — the header's form before
/// format 2 — is read too.)
pub mod serde_any {
    use super::*;
    use serde::Deserializer;
    use serde::de::{MapAccess, SeqAccess, Visitor};

    struct Any;

    impl<'de> Visitor<'de> for Any {
        type Value = Comments;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("comments, by object or as a list")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Comments, A::Error> {
            let mut list = Vec::new();
            while let Some(x) = s.next_element::<(u32, i32, String)>()? {
                list.push(x);
            }
            Ok(from_list(list))
        }

        fn visit_map<A: MapAccess<'de>>(self, m: A) -> Result<Comments, A::Error> {
            Comments::deserialize(serde::de::value::MapAccessDeserializer::new(m))
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Comments, D::Error> {
        d.deserialize_any(Any)
    }
}

impl Catalog {
    /// The comment on `oid`'s sub-object `sub` (0: the object), empty ones included.
    pub fn comment(&self, oid: u32, sub: i32) -> Option<&str> {
        let c = self.comments.get(&oid)?;
        c.0.iter().find(|c| c.0 == sub).map(|c| c.2.as_str())
    }

    /// `oid`'s comments: (sub-id, tick, text) by sub-id.
    pub fn comments_of(&self, oid: u32) -> &[(i32, u64, String)] {
        self.comments.get(&oid).map_or(&[], |c| &c.0)
    }

    /// Every comment, in pg_description's order: (object, sub-id, text).
    pub fn comments_in_order(&self) -> Vec<(u32, i32, &str)> {
        let mut all: Vec<(u64, u32, i32, &str)> = (self.comments.iter())
            .flat_map(|(o, c)| c.0.iter().map(move |(s, t, x)| (*t, *o, *s, x.as_str())))
            .collect();
        all.sort_unstable_by_key(|c| (c.0, c.1, c.2));
        all.into_iter().map(|(_, o, s, x)| (o, s, x)).collect()
    }

    /// Set the comment on `oid`'s sub-object `sub` (a new row at pg_description's end).
    pub fn set_comment(&mut self, oid: u32, sub: i32, text: String) {
        self.comment_tick += 1;
        let tick = self.comment_tick;
        let mut c = self.comments.get(&oid).map(|c| (**c).clone()).unwrap_or_default();
        c.0.retain(|c| c.0 != sub);
        c.0.push((sub, tick, text));
        c.0.sort_by_key(|c| c.0);
        self.comments.insert(oid, Arc::new(c));
    }

    /// Remove the comment on `oid`'s sub-object `sub`.
    pub fn drop_comment(&mut self, oid: u32, sub: i32) {
        let Some(c) = self.comments.get(&oid) else { return };
        if !c.0.iter().any(|c| c.0 == sub) {
            return;
        }
        let mut c = (**c).clone();
        c.0.retain(|c| c.0 != sub);
        if c.0.is_empty() {
            self.comments.remove(&oid);
        } else {
            self.comments.insert(oid, Arc::new(c));
        }
    }

    /// Remove `oid`'s comments (its own and its sub-objects').
    pub fn drop_comments(&mut self, oid: u32) {
        self.comments.remove(&oid);
    }

    /// Remove the comments of `oid`'s sub-objects `drop` says go.
    pub fn drop_comments_where(&mut self, oid: u32, drop: impl Fn(i32) -> bool) {
        let Some(c) = self.comments.get(&oid) else { return };
        if !c.0.iter().any(|c| drop(c.0)) {
            return;
        }
        let mut c = (**c).clone();
        c.0.retain(|c| !drop(c.0));
        if c.0.is_empty() {
            self.comments.remove(&oid);
        } else {
            self.comments.insert(oid, Arc::new(c));
        }
    }

    /// `old`'s comments now `new`'s, where they were in pg_description's order.
    pub fn move_comments(&mut self, old: u32, new: u32) {
        let Some(c) = self.comments.remove(&old) else { return };
        let c = match self.comments.get(&new) {
            None => c,
            Some(n) => {
                let mut m = (**n).clone();
                for e in c.0.iter() {
                    m.0.retain(|x| x.0 != e.0);
                    m.0.push(e.clone());
                }
                m.0.sort_by_key(|c| c.0);
                Arc::new(m)
            }
        };
        self.comments.insert(new, c);
    }

    /// Whether any comment is there.
    pub fn has_comments(&self) -> bool {
        !self.comments.is_empty()
    }
}
