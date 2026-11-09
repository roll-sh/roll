//! What a view (materialized view or index table) is made of, resolved against its base table:
//! where each key column's value comes from, which base columns are copied, the `WHERE` filter.
//! Built once per schema version ([`Specs::build`]) and shared by the write path and backfill.

use std::collections::HashMap;
use std::sync::Arc;

use nosql_cql::ast::StatementKind;
use nosql_types::Dialect;

use crate::database::Database;
use crate::error::Result;
use crate::index::{self, Computed, IndexClass, IndexDef, TargetKind};
use crate::prepare::PrepareCtx;
use crate::prepare::restrictions::{self, RowFilter, StmtKind};
use crate::prepare::terms::EvalCtx;
use crate::schema::{ColumnKind, Schema, Table};

/// Where a view key column's value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// A base column (table column index).
    Base(usize),
    /// `idx_token`: the base partition's token.
    Token,
    /// `coll_value`: the element (value, key or entry) of an indexed collection.
    Element,
    /// `keys_for_values_idx`: the element's cell path.
    Path,
    /// Alternator's `alternator_extract_from_attrs`: an attribute of the `:attrs` map
    /// ([`ViewSpec::extracted`], by index).
    Extracted(usize),
}

/// An Alternator index key attribute: the value of `:attrs[attr]` when it has the key's type,
/// without its type tag (`{"type":"alternator_extract_from_attrs","attr_name":..,
/// "desired_type":"S"|"B"|"N"}` in `system_schema.computed_columns`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extract {
    /// The base map column (`:attrs`), by table column index.
    pub column: usize,
    /// The attribute name: the map key (a `text` cell path).
    pub path: bytes::Bytes,
    /// The value's type tag: 0 (S), 1 (B) or 3 (N).
    pub tag: u8,
}

impl Extract {
    /// The computation JSON of an Alternator key attribute.
    pub fn json(attr: &str, desired_type: &str) -> String {
        format!(
            r#"{{"type":"alternator_extract_from_attrs","attr_name":{},"desired_type":{}}}"#,
            json_string(attr),
            json_string(desired_type)
        )
    }

    /// Parses an Alternator computation for `base` (`None` if it is not one).
    pub fn parse(json: &str, base: &Table) -> Option<Extract> {
        let fields = flat_json_strings(json)?;
        let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        if get("type")? != "alternator_extract_from_attrs" {
            return None;
        }
        let tag = match get("desired_type")? {
            "S" => 0,
            "B" => 1,
            "N" => 3,
            _ => return None,
        };
        Some(Extract {
            column: base.column_index(":attrs")?,
            path: bytes::Bytes::copy_from_slice(get("attr_name")?.as_bytes()),
            tag,
        })
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The members of a flat JSON object whose values are all strings.
fn flat_json_strings(json: &str) -> Option<Vec<(String, String)>> {
    let mut it = json.trim().chars().peekable();
    let skip_ws = |it: &mut std::iter::Peekable<std::str::Chars>| {
        while it.peek().is_some_and(|c| c.is_whitespace()) {
            it.next();
        }
    };
    let string = |it: &mut std::iter::Peekable<std::str::Chars>| -> Option<String> {
        if it.next()? != '"' {
            return None;
        }
        let mut s = String::new();
        loop {
            match it.next()? {
                '"' => return Some(s),
                '\\' => match it.next()? {
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    'r' => s.push('\r'),
                    'b' => s.push('\u{8}'),
                    'f' => s.push('\u{c}'),
                    'u' => {
                        let hex: String = (0..4).filter_map(|_| it.next()).collect();
                        let mut cp = u32::from_str_radix(&hex, 16).ok()?;
                        if (0xD800..0xDC00).contains(&cp) {
                            if it.next()? != '\\' || it.next()? != 'u' {
                                return None;
                            }
                            let lo: String = (0..4).filter_map(|_| it.next()).collect();
                            let lo = u32::from_str_radix(&lo, 16).ok()?;
                            cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.checked_sub(0xDC00)?);
                        }
                        s.push(char::from_u32(cp)?);
                    }
                    c => s.push(c),
                },
                c => s.push(c),
            }
        }
    };
    skip_ws(&mut it);
    if it.next()? != '{' {
        return None;
    }
    let mut out = Vec::new();
    loop {
        skip_ws(&mut it);
        if it.peek() == Some(&'}') {
            return Some(out);
        }
        let k = string(&mut it)?;
        skip_ws(&mut it);
        if it.next()? != ':' {
            return None;
        }
        skip_ws(&mut it);
        let v = string(&mut it)?;
        out.push((k, v));
        skip_ws(&mut it);
        match it.next()? {
            ',' => continue,
            '}' => return Some(out),
            _ => return None,
        }
    }
}

/// One view of a base table.
#[derive(Debug)]
pub struct ViewSpec {
    /// The view (or index) table: its storage layout.
    pub view: Arc<Table>,
    /// Per view key column, partition key then clustering (by position).
    pub key: Vec<KeySource>,
    /// Regular view columns copied from the base: `(view column id, base column index)`.
    pub copied: Vec<(u32, usize)>,
    /// Base columns present in the view, by base column index.
    pub selected: Vec<bool>,
    /// The base non-primary-key column in the view's primary key (`baseNonPKColumnsInViewPK`).
    pub regular_in_pk: Option<usize>,
    /// A collection index: the collection (base column index) and what of it is indexed.
    pub elements: Option<(usize, TargetKind)>,
    /// The indexed column is static: one entry per base partition.
    pub static_target: bool,
    /// The view's `WHERE` restrictions on base rows (beyond `IS NOT NULL`).
    pub filter: Option<RowFilter>,
    pub include_all: bool,
    /// The view's primary key has exactly the base primary key columns.
    pub same_pk: bool,
    /// The index this table belongs to (index tables only).
    pub index: Option<IndexDef>,
    /// Alternator index key attributes ([`KeySource::Extracted`]).
    pub extracted: Vec<Extract>,
    /// A hidden (Cassandra) index table, not in the schema.
    pub hidden: bool,
}

impl ViewSpec {
    /// A materialized view (or a ScyllaDB index view) defined in the schema.
    pub fn view(
        db: &Database,
        schema: &Arc<Schema>,
        base: &Table,
        view: &Arc<Table>,
    ) -> Result<ViewSpec> {
        let meta = view.view.as_ref().expect("a view");
        let index_name = view.name.strip_suffix("_index");
        let def = index_name.and_then(|n| {
            base.indexes
                .iter()
                .find(|m| m.name == n)
                .and_then(|m| index::def(base, m))
        });
        if let Some(def) = def {
            return Ok(Self::index_spec(base, view.clone(), def, false));
        }
        let computed: HashMap<&str, Option<Computed>> = meta
            .computed
            .iter()
            .map(|(n, j)| (n.as_str(), Computed::parse(j)))
            .collect();
        let mut key = Vec::new();
        let mut extracted = Vec::new();
        for &vi in view.partition_key.iter().chain(&view.clustering) {
            let c = &view.columns[vi];
            let alternator = meta
                .computed
                .iter()
                .find(|(n, _)| *n == c.name)
                .and_then(|(_, j)| Extract::parse(j, base));
            if let Some(x) = alternator {
                extracted.push(x);
                key.push(KeySource::Extracted(extracted.len() - 1));
                continue;
            }
            key.push(match computed.get(c.name.as_str()) {
                Some(Some(Computed::Token)) => KeySource::Token,
                Some(_) if c.name == "keys_for_values_idx" => KeySource::Path,
                Some(_) => KeySource::Element,
                None => KeySource::Base(base.column_index(&c.name).unwrap_or(0)),
            });
        }
        let mut selected = vec![false; base.columns.len()];
        let mut copied = Vec::new();
        for c in &view.columns {
            if let Some(bi) = base.column_index(&c.name) {
                selected[bi] = true;
                if !c.is_primary_key() {
                    copied.push((c.id, bi));
                }
            }
        }
        let regular_in_pk = key.iter().find_map(|k| match k {
            KeySource::Base(bi) if !base.columns[*bi].is_primary_key() => Some(*bi),
            _ => None,
        });
        let same_pk = regular_in_pk.is_none()
            && key.iter().all(|k| matches!(k, KeySource::Base(_)))
            && key.len() == base.partition_key.len() + base.clustering.len();
        let filter = where_filter(db, schema, base, &meta.where_clause)?;
        Ok(ViewSpec {
            view: view.clone(),
            key,
            copied,
            selected,
            regular_in_pk,
            elements: None,
            static_target: false,
            filter,
            include_all: meta.include_all_columns,
            same_pk,
            index: None,
            hidden: false,
            extracted,
        })
    }

    /// An index table (ScyllaDB's index view or Cassandra's hidden table) of `def`.
    pub fn index_spec(base: &Table, view: Arc<Table>, def: IndexDef, hidden: bool) -> ViewSpec {
        let target = &base.columns[def.column];
        let elements = (target.ty.is_multicell() && target.ty.is_collection()).then(|| {
            let kind = match (&target.ty, def.target) {
                (nosql_types::CqlType::Set { .. }, _) => TargetKind::Keys,
                (_, k) => k,
            };
            (def.column, kind)
        });
        let mut key = Vec::new();
        for &vi in view.partition_key.iter().chain(&view.clustering) {
            let c = &view.columns[vi];
            key.push(match c.name.as_str() {
                "idx_token" if def.class != IndexClass::Local => KeySource::Token,
                "coll_value" if elements.is_some() => KeySource::Element,
                "keys_for_values_idx" if elements.is_some() => KeySource::Path,
                n => KeySource::Base(base.column_index(n).unwrap_or(def.column)),
            });
        }
        let mut selected = vec![false; base.columns.len()];
        for k in &key {
            if let KeySource::Base(bi) = k {
                selected[*bi] = true;
            }
        }
        selected[def.column] = true;
        let regular_in_pk = (elements.is_none() && !target.is_primary_key()).then_some(def.column);
        let same_pk = target.is_primary_key();
        ViewSpec {
            view,
            key,
            copied: Vec::new(),
            selected,
            regular_in_pk,
            elements,
            static_target: target.kind == ColumnKind::Static,
            filter: None,
            include_all: false,
            same_pk,
            index: Some(def),
            hidden,
            extracted: Vec::new(),
        }
    }
}

/// A materialized view's `WHERE` clause as a row filter (`None` when it is only `IS NOT NULL`).
fn where_filter(
    db: &Database,
    schema: &Arc<Schema>,
    base: &Table,
    clause: &str,
) -> Result<Option<RowFilter>> {
    if clause.trim().is_empty() {
        return Ok(None);
    }
    let d = db.dialect();
    let text = format!(
        "SELECT * FROM {}.{} WHERE {clause}",
        nosql_types::quote_identifier(&base.keyspace, d),
        nosql_types::quote_identifier(&base.name, d)
    );
    let Ok(stmt) = crate::session::parse(&text, d) else {
        return Ok(None);
    };
    let StatementKind::Select(s) = &stmt.kind else {
        return Ok(None);
    };
    let mut cx = PrepareCtx::new(d, schema, &base.keyspace, &db.functions, &stmt.bind_markers);
    let r = restrictions::analyze(
        base,
        StmtKind::Select,
        &s.where_clause,
        true,
        false,
        true,
        &mut cx,
    )?;
    let values = Vec::new();
    let ecx = EvalCtx::new(&values, d);
    let f = r.row_filter(base, &ecx)?;
    Ok((!f.is_empty()).then_some(f))
}

/// Every view of each base table of one schema version.
#[derive(Default)]
pub struct Specs {
    pub version: [u8; 16],
    pub by_base: HashMap<[u8; 16], Arc<BaseSpecs>>,
}

/// The derived data of one base table.
#[derive(Default, Debug)]
pub struct BaseSpecs {
    pub views: Vec<Arc<ViewSpec>>,
    /// The CDC log of the table (ScyllaDB).
    pub cdc: Option<Arc<crate::cdc::LogSpec>>,
}

impl BaseSpecs {
    pub fn is_empty(&self) -> bool {
        self.views.is_empty() && self.cdc.is_none()
    }
}

impl Specs {
    /// Resolves every view, index table and CDC log of `schema`.
    pub fn build(db: &Database, schema: &Arc<Schema>) -> Specs {
        let d = db.dialect();
        let mut by_base: HashMap<[u8; 16], BaseSpecs> = HashMap::new();
        for ks in schema.user_keyspaces() {
            for t in ks.tables.values() {
                if let Some(v) = &t.view
                    && let Some(base) = ks.tables.values().find(|b| b.id == v.base_table_id)
                {
                    match ViewSpec::view(db, schema, base, t) {
                        Ok(spec) => by_base
                            .entry(base.id)
                            .or_default()
                            .views
                            .push(Arc::new(spec)),
                        Err(e) => tracing::warn!("view {}.{}: {}", t.keyspace, t.name, e.message),
                    }
                }
                if d == Dialect::Cassandra && !t.is_view() {
                    for def in index::defs(t) {
                        if let Some(h) = index::hidden_table(t, &def, d) {
                            let spec = ViewSpec::index_spec(t, Arc::new(h), def, true);
                            by_base.entry(t.id).or_default().views.push(Arc::new(spec));
                        }
                    }
                }
                if let Some(log) = crate::cdc::LogSpec::for_base(ks, t, &db.cfg) {
                    by_base.entry(t.id).or_default().cdc = Some(Arc::new(log));
                }
            }
        }
        Specs {
            version: schema.version,
            by_base: by_base.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
        }
    }

    pub fn get(&self, base: &[u8; 16]) -> Option<&Arc<BaseSpecs>> {
        self.by_base.get(base)
    }
}
