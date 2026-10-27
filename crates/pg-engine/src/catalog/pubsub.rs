//! Logical replication's per-database catalogs: publications (pg_publication), the tables
//! they name (pg_publication_rel, with row filters and column lists) and the schemas they
//! name (pg_publication_namespace). Subscriptions are cluster-wide (`shared`).

use super::{Catalog, Table};
use serde::{Deserialize, Serialize};

/// A table a publication names: a pg_publication_rel row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PubRel {
    pub oid: u32,
    pub rel: u32,
    /// The row filter (prqual), as SQL over the table's columns.
    pub qual: Option<String>,
    /// The column list (prattrs), as attribute numbers in the order written.
    pub cols: Option<Vec<i16>>,
}

/// A schema a publication names (FOR TABLES IN SCHEMA): a pg_publication_namespace row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PubSchema {
    pub oid: u32,
    pub schema: u32,
}

/// CREATE PUBLICATION.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Publication {
    pub oid: u32,
    pub name: String,
    pub owner: u32,
    pub all_tables: bool,
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
    pub truncate: bool,
    /// publish_via_partition_root
    pub via_root: bool,
    /// publish_generated_columns: 'n' none, 's' stored.
    pub gencols: char,
    pub rels: Vec<PubRel>,
    pub schemas: Vec<PubSchema>,
}

/// is_publishable_class: a permanent user table (not a TOAST or system one).
pub fn publishable(t: &Table) -> bool {
    !t.temporary && !t.unlogged && t.foreign.is_none() && t.oid >= super::FIRST_USER_OID
}

impl Catalog {
    /// The publications that publish table `t` (GetRelationPublications plus the schemas'
    /// and FOR ALL TABLES ones), with the ancestor through which each does (`t` itself when
    /// named directly): for a partition, a publication of one of its ancestors publishes
    /// it too.
    pub fn publications_of(&self, t: &Table) -> Vec<(std::sync::Arc<Publication>, u32)> {
        if !publishable(t) {
            return Vec::new();
        }
        let mut chain = vec![t.oid];
        let mut cur = t;
        while cur.partbound.is_some() {
            let Some(p) = cur.inherits.first().and_then(|p| self.tables.get(p)) else {
                break;
            };
            chain.push(p.oid);
            cur = p;
        }
        let mut out = Vec::new();
        for p in self.publications.values() {
            let via = if p.all_tables {
                Some(*chain.last().unwrap())
            } else {
                chain.iter().rev().copied().find(|r| {
                    p.rels.iter().any(|x| x.rel == *r)
                        || self
                            .tables
                            .get(r)
                            .is_some_and(|rt| p.schemas.iter().any(|s| s.schema == rt.schema))
                })
            };
            if let Some(v) = via {
                out.push((p.clone(), v));
            }
        }
        out
    }
}

/// The statement kinds CheckCmdReplicaIdentity restricts.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RiCmd {
    Update,
    Delete,
}

/// The names of the columns a row filter (stored SQL) mentions.
fn filter_columns(qual: &str) -> Vec<String> {
    let Ok(n) = crate::sql::binder::ast::parse_expr(qual) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(ne) = n.node.as_ref() {
        for (x, ..) in ne.nodes() {
            if let pg_query::NodeRef::ColumnRef(c) = x
                && let Some(name) = crate::sql::binder::ast::names(&c.fields).last()
                && !out.contains(name)
            {
                out.push(name.clone());
            }
        }
    }
    out
}

impl Catalog {
    /// RelationGetIndexAttrBitmap(INDEX_ATTR_BITMAP_IDENTITY_KEY): the replica identity
    /// index's columns (positions), None without one.
    pub(crate) fn identity_key(&self, t: &Table) -> Option<Vec<usize>> {
        match t.replident {
            // rd_pkindex: an immediate primary key only.
            None => t.pkey.clone().filter(|_| {
                !t.constraints.iter().any(|c| {
                    matches!(c.kind, super::ConstraintKind::PrimaryKey { .. }) && c.deferrable
                })
            }),
            Some('i') => t
                .indexes
                .iter()
                .chain(&t.pkey_index)
                .filter_map(|i| self.indexes.get(i))
                .find(|ix| ix.replident)
                .map(|ix| {
                    ix.columns
                        .iter()
                        .filter_map(|c| match c.key {
                            super::IndexKey::Column(i) => Some(i),
                            _ => None,
                        })
                        .collect()
                }),
            _ => None,
        }
    }

    /// CheckCmdReplicaIdentity: whether an UPDATE or DELETE of `t` may run, given the
    /// publications that publish it (RelationBuildPublicationDesc).
    pub fn check_replica_identity(&self, t: &Table, cmd: RiCmd) -> crate::error::Result<()> {
        use crate::error::{Error, code};
        if t.partkey.is_some() || !publishable(t) || self.publications.is_empty() {
            return Ok(());
        }
        let pubs = self.publications_of(t);
        if pubs.is_empty() {
            return Ok(());
        }
        let full = t.replident == Some('f');
        let key = self.identity_key(t).unwrap_or_default();
        let (mut upd, mut del) = (false, false);
        let (mut rf_upd, mut rf_del) = (true, true);
        let (mut cols_upd, mut cols_del) = (true, true);
        let (mut gen_upd, mut gen_del) = (true, true);
        let is_partition = t.partbound.is_some();
        for (p, via) in &pubs {
            upd |= p.update;
            del |= p.delete;
            if !(p.update || p.delete) {
                continue;
            }
            // The relation whose row filter and column list apply: with
            // publish_via_partition_root, the topmost ancestor published.
            let as_rel = if p.via_root && is_partition { *via } else { t.oid };
            let entry = (!p.all_tables)
                .then(|| p.rels.iter().find(|r| r.rel == as_rel))
                .flatten();
            let parent = self.tables.get(&as_rel);
            // The column of the published relation for one of `t`'s.
            let in_parent = |i: usize| -> Option<i16> {
                let name = &t.columns.get(i)?.name;
                let pt = parent?;
                pt.column_index(name).map(|j| j as i16 + 1)
            };
            // pub_rf_contains_invalid_column.
            if !p.all_tables
                && !full
                && let Some(q) = entry.and_then(|r| r.qual.as_deref())
            {
                let bad = filter_columns(q).iter().any(|c| {
                    t.column_index(c).is_none_or(|i| !key.contains(&i))
                });
                if bad {
                    if p.update {
                        rf_upd = false;
                    }
                    if p.delete {
                        rf_del = false;
                    }
                }
            }
            // pub_contains_invalid_column.
            let columns = entry.and_then(|r| r.cols.clone());
            let (mut bad_cols, mut bad_gen) = (false, false);
            if full {
                bad_cols = columns.is_some();
                let stored = t
                    .live_columns()
                    .any(|(_, c)| c.generated.is_some() && !c.virtual_gen);
                let virt = t.live_columns().any(|(_, c)| c.virtual_gen);
                if (p.gencols != 's' && stored) || virt {
                    bad_gen = true;
                }
            } else {
                for &i in &key {
                    let c = &t.columns[i];
                    match &columns {
                        None => {
                            if c.virtual_gen || (c.generated.is_some() && p.gencols != 's') {
                                bad_gen = true;
                                break;
                            }
                        }
                        Some(list) => {
                            let a = if p.via_root { in_parent(i) } else { Some(i as i16 + 1) };
                            bad_cols |= a.is_none_or(|a| !list.contains(&a));
                        }
                    }
                }
            }
            if bad_cols || bad_gen {
                if p.update {
                    cols_upd = !bad_cols;
                    gen_upd = !bad_gen;
                }
                if p.delete {
                    cols_del = !bad_cols;
                    gen_del = !bad_gen;
                }
            }
        }
        let (verb, rf, cols, gens) = match cmd {
            RiCmd::Update => ("update", rf_upd, cols_upd, gen_upd),
            RiCmd::Delete => ("delete from", rf_del, cols_del, gen_del),
        };
        let invalid = |detail: &str| {
            Err(Error::new(
                code::INVALID_COLUMN_REFERENCE,
                format!("cannot {verb} table \"{}\"", t.name),
            )
            .detail(detail.to_string()))
        };
        if !rf {
            return invalid("Column used in the publication WHERE expression is not part of the replica identity.");
        }
        if !cols {
            return invalid("Column list used by the publication does not cover the replica identity.");
        }
        if !gens {
            return invalid("Replica identity must not contain unpublished generated columns.");
        }
        if self.identity_key(t).is_some() || full {
            return Ok(());
        }
        match cmd {
            RiCmd::Update if upd => Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "cannot update table \"{}\" because it does not have a replica identity and publishes updates",
                    t.name
                ),
            )
            .hint("To enable updating the table, set REPLICA IDENTITY using ALTER TABLE.")),
            RiCmd::Delete if del => Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "cannot delete from table \"{}\" because it does not have a replica identity and publishes deletes",
                    t.name
                ),
            )
            .hint("To enable deleting from the table, set REPLICA IDENTITY using ALTER TABLE.")),
            _ => Ok(()),
        }
    }
}
