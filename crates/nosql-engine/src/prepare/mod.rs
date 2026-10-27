//! Preparing statements: resolving names against a schema snapshot, typing terms and bind
//! markers, analysing restrictions and selections. The output is owned, typed plans that execute
//! without looking at the syntax tree again.

pub mod restrictions;
pub mod selection;
pub mod terms;

use std::sync::Arc;

use nosql_cql::ast::{BindMarker, Ident};
use nosql_types::{CqlType, Dialect};

use crate::error::Result;
use crate::functions::{Registry, UserFunctions};
use crate::response::ColumnSpec;
use crate::schema::{Schema, Table};

/// The spec a term is prepared for: where it goes (for bind marker metadata and messages) and
/// its type.
#[derive(Clone, Debug, PartialEq)]
pub struct Receiver {
    pub keyspace: String,
    pub table: String,
    pub name: String,
    pub ty: CqlType,
}

impl Receiver {
    pub fn new(keyspace: &str, table: &str, name: impl Into<String>, ty: CqlType) -> Self {
        Receiver {
            keyspace: keyspace.into(),
            table: table.into(),
            name: name.into(),
            ty,
        }
    }

    pub fn column(t: &Table, name: &str, ty: &CqlType) -> Self {
        Receiver::new(&t.keyspace, &t.name, name, ty.clone())
    }

    /// Same place, another name and type (`value(l)`, `key(m)`, …).
    pub fn derive(&self, name: impl Into<String>, ty: CqlType) -> Self {
        Receiver {
            keyspace: self.keyspace.clone(),
            table: self.table.clone(),
            name: name.into(),
            ty,
        }
    }

    pub fn spec(&self) -> ColumnSpec {
        ColumnSpec::new(&self.keyspace, &self.table, &self.name, self.ty.clone())
    }
}

/// State shared while preparing one statement.
pub struct PrepareCtx<'a> {
    pub dialect: Dialect,
    pub schema: &'a Arc<Schema>,
    /// The keyspace of the statement (user types and unqualified user functions resolve here).
    pub keyspace: String,
    pub functions: &'a Registry,
    pub user_functions: Option<&'a dyn UserFunctions>,
    /// The spec of each bind marker (by marker index).
    pub binds: Vec<Option<ColumnSpec>>,
    /// Bind marker names (`:name`), by marker index.
    pub names: Vec<Option<String>>,
    /// The bind variable (slot) of each marker. ScyllaDB gives repeated `:name` markers one shared
    /// variable; Cassandra gives every marker its own.
    pub slots: Vec<usize>,
    /// The receiver each bind variable was typed for, before a `:name` renamed it (Cassandra's
    /// `targetColumns`, used for the partition key indexes).
    pub targets: Vec<Option<String>>,
    /// Every marker's (bind variable, receiver): a ScyllaDB variable shared by repeated `:name`
    /// markers routes every partition key column one of them binds.
    pub marker_targets: Vec<(usize, String)>,
    /// The index planner a SELECT's restrictions consult (`None`: no index is used).
    pub indexes: Option<Arc<dyn crate::hooks::IndexPlanner>>,
    /// The column of an `ORDER BY … ANN OF` (a restriction an index serves, as Cassandra's
    /// `addOrderingRestrictions`).
    pub ann: Option<usize>,
    /// The user functions the statement calls (with an aggregate's sub-functions), for `EXECUTE`
    /// checks and prepared-statement eviction (`crate::udf`).
    pub used: Vec<Arc<dyn crate::functions::Function>>,
    /// ScyllaDB's `cql_in_bind_variable_name_uses_uppercase_operator = false`: `IN ?` markers
    /// are named `in(c)` / `not in(c)`.
    pub lowercase_in: bool,
}

impl<'a> PrepareCtx<'a> {
    pub fn new(
        dialect: Dialect,
        schema: &'a Arc<Schema>,
        keyspace: &str,
        functions: &'a Registry,
        markers: &[Option<Ident>],
    ) -> Self {
        let names: Vec<Option<String>> = markers
            .iter()
            .map(|m| m.as_ref().map(|i| i.name().to_string()))
            .collect();
        let mut slots = Vec::with_capacity(names.len());
        let mut count = 0usize;
        for (i, n) in names.iter().enumerate() {
            let shared = match (dialect, n) {
                (Dialect::Scylla, Some(n)) => names[..i]
                    .iter()
                    .position(|m| m.as_deref() == Some(n.as_str()))
                    .map(|j| slots[j]),
                _ => None,
            };
            match shared {
                Some(s) => slots.push(s),
                None => {
                    slots.push(count);
                    count += 1;
                }
            }
        }
        PrepareCtx {
            dialect,
            schema,
            keyspace: keyspace.to_string(),
            functions,
            user_functions: None,
            binds: vec![None; count],
            names,
            slots,
            targets: vec![None; count],
            marker_targets: Vec::new(),
            indexes: None,
            ann: None,
            used: Vec::new(),
            lowercase_in: false,
        }
    }

    /// Applies ScyllaDB's live preparation switches (call right after [`PrepareCtx::new`]):
    /// `cql_duplicate_bind_variable_names_refer_to_same_variable = false` gives every `:name`
    /// marker its own variable (as Cassandra does), and
    /// `cql_in_bind_variable_name_uses_uppercase_operator = false` lowercases `IN(c)`.
    pub fn apply_settings(&mut self, s: &crate::Settings) {
        if !self.dialect.is_scylla() {
            return;
        }
        self.lowercase_in = !s.scylla.in_bind_name_uppercase;
        if !s.scylla.duplicate_bind_names_same_variable {
            let n = self.names.len();
            self.slots = (0..n).collect();
            self.binds = vec![None; n];
            self.targets = vec![None; n];
        }
    }

    /// Records a resolved call to a user function (and what it runs).
    pub fn record_call(&mut self, f: &Arc<dyn crate::functions::Function>) {
        if f.name().keyspace == "system" {
            return;
        }
        self.used.push(f.clone());
        self.used.extend(f.dependencies());
    }

    /// Records the spec of a bind marker and returns its bind variable; a named marker reports
    /// its own name.
    pub fn bind(&mut self, marker: &BindMarker, receiver: &Receiver) -> usize {
        let m = marker.index as usize;
        let mut spec = receiver.spec();
        if let Some(Some(n)) = self.names.get(m) {
            spec.name = n.clone();
        } else if let Some(n) = &marker.name {
            spec.name = n.name().to_string();
        } else if self.lowercase_in {
            for (upper, lower) in [("IN(", "in("), ("NOT IN(", "not in(")] {
                if let Some(rest) = spec.name.strip_prefix(upper) {
                    spec.name = format!("{lower}{rest}");
                    break;
                }
            }
        }
        let slot = self.slots.get(m).copied().unwrap_or(m);
        if slot >= self.binds.len() {
            self.binds.resize(slot + 1, None);
        }
        if self.targets.len() < self.binds.len() {
            self.targets.resize(self.binds.len(), None);
        }
        if self.binds[slot].is_none() {
            self.binds[slot] = Some(spec);
            self.targets[slot] = Some(receiver.name.clone());
        }
        self.marker_targets.push((slot, receiver.name.clone()));
        slot
    }

    /// `VariableSpecifications.getPartitionKeyBindVariableIndexes`: the bind variable of each
    /// partition key column of `t`, or nothing unless every component is bound directly.
    pub fn pk_indexes(&self, t: &Table) -> Vec<u16> {
        let mut positions = vec![None; t.partition_key.len()];
        for (i, target) in &self.marker_targets {
            let i = *i;
            let Some(Some(spec)) = self.binds.get(i) else {
                continue;
            };
            if spec.keyspace != t.keyspace || spec.table != t.name {
                continue;
            }
            if let Some(c) = t.column(target)
                && c.kind == crate::schema::ColumnKind::PartitionKey
                && c.ty == spec.ty
            {
                positions[c.position as usize] = Some(i as u16);
            }
        }
        if positions.iter().any(Option::is_none) {
            return Vec::new();
        }
        positions.into_iter().flatten().collect()
    }

    /// Bind specs, failing if a marker was never typed (the reference's "could not infer").
    pub fn bind_specs(&self) -> Result<Vec<ColumnSpec>> {
        Ok(self
            .binds
            .iter()
            .map(|b| {
                b.clone()
                    .unwrap_or_else(|| ColumnSpec::new("", "", "", CqlType::Blob))
            })
            .collect())
    }

    pub fn udt_resolver(&self) -> impl Fn(&str) -> Option<Arc<nosql_types::UdtDef>> + '_ {
        move |name| {
            self.schema
                .keyspace(&self.keyspace)
                .and_then(|k| k.types.get(name).cloned())
        }
    }
}
