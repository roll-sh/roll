//! Stored definitions follow renames, the way Postgres's trees (which name objects by oid and
//! columns by attnum) do: views, materialized views, rules, column defaults and generation
//! expressions, CHECK constraints, index expressions and predicates, partition key
//! expressions, statistics expressions, policies, trigger WHEN conditions, domain CHECKs and
//! SQL-standard function bodies keep working, and print the new names, after a relation,
//! column, function, type or schema is renamed or moved.
//!
//! The engine keeps those definitions as SQL text. A rename binds each definition that may
//! name the object (against the catalog from before it), recording where each name resolved
//! (`binder::refs`), then edits the text at the recorded positions:
//!
//! - a relation's name is replaced (qualified when it was, when it moved, or when the new
//!   name would resolve to something else); a FROM item named without an alias gets an
//!   alias the engine marks as its own (`ast::alias_name`), holding the old name, so the
//!   column references qualified by it keep resolving without being touched — the deparser
//!   shows the relation under its current name, as Postgres does;
//! - a column's references (and INSERT / UPDATE target names) are replaced, except through a
//!   FROM item whose column a JOIN USING joins on: that column keeps its old name through
//!   column aliases the engine adds (which Postgres shows the same way, `t t(a, b)`);
//! - functions, types and reg* literals naming the object are replaced.

use super::Ddl;
use crate::catalog::{Catalog, ConstraintKind, IndexKey, quote_ident};
use crate::error::Result;
use crate::sql::binder::ast;
use crate::sql::binder::refs::{RefLog, RelUse};
use crate::sql::binder::{Binder, Clause};
use crate::sql::expr::Expr;
use crate::types::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// What was renamed or moved (the names before and after come from the catalogs).
#[derive(Clone, Debug)]
pub(super) enum Change {
    /// A table, view, materialized view or sequence.
    Rel(u32),
    /// A column of a table or view: (relation, attnum).
    Column(u32, i16),
    Func(u32),
    Type(u32),
    Schema(u32),
    /// A column added to a relation (its name): the column aliases the engine keeps for the
    /// relation take it, under a name of its own (`c_1` when they hold `c` already).
    AddColumn(u32, String),
    /// A column dropped (relation, attnum): those column aliases lose it.
    DropColumn(u32, i16),
}

/// One text edit: replace `start..end` with `text` (an insertion when empty).
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Apply edits (overlapping ones after the first are dropped).
fn apply(text: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| (e.start, e.end));
    let mut out = String::with_capacity(text.len() + 16);
    let mut at = 0;
    for e in edits {
        if e.start < at || e.end > text.len() {
            continue;
        }
        out.push_str(&text[at..e.start]);
        out.push_str(&e.text);
        at = e.end;
    }
    out.push_str(&text[at..]);
    out
}

// ── a small lexer for names at a position ──

/// Skip whitespace and comments.
fn skip_ws(t: &[u8], mut i: usize) -> usize {
    loop {
        while i < t.len() && t[i].is_ascii_whitespace() {
            i += 1;
        }
        if t[i..].starts_with(b"--") {
            while i < t.len() && t[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if t[i..].starts_with(b"/*") {
            let mut depth = 0;
            while i < t.len() {
                if t[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if t[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        return i;
    }
}

/// An identifier in the text: its span and its value (folded like the scanner folds it).
#[derive(Clone, Debug)]
struct Ident {
    start: usize,
    end: usize,
    value: String,
}

fn ident_at(text: &str, i: usize) -> Option<Ident> {
    let t = text.as_bytes();
    let c = *t.get(i)?;
    if c == b'"' {
        let mut j = i + 1;
        let mut value = Vec::new();
        loop {
            let c = *t.get(j)?;
            if c == b'"' {
                if t.get(j + 1) == Some(&b'"') {
                    value.push(b'"');
                    j += 2;
                    continue;
                }
                break;
            }
            value.push(c);
            j += 1;
        }
        return Some(Ident {
            start: i,
            end: j + 1,
            value: String::from_utf8(value).ok()?,
        });
    }
    if !(c.is_ascii_alphabetic() || c == b'_' || c >= 0x80) {
        return None;
    }
    if (c == b'U' || c == b'u') && t.get(i + 1) == Some(&b'&') {
        return None;
    }
    let mut j = i;
    while j < t.len() && (t[j].is_ascii_alphanumeric() || t[j] == b'_' || t[j] == b'$' || t[j] >= 0x80)
    {
        j += 1;
    }
    Some(Ident {
        start: i,
        end: j,
        value: text.get(i..j)?.to_ascii_lowercase(),
    })
}

/// A dotted name at `i` (`a`, `s.t`, `t.*`): its identifiers, and where it ends.
fn chain_at(text: &str, i: usize) -> (Vec<Ident>, usize) {
    let t = text.as_bytes();
    let mut out: Vec<Ident> = Vec::new();
    let mut end = i;
    let mut at = i;
    while let Some(id) = ident_at(text, at) {
        end = id.end;
        out.push(id);
        let j = skip_ws(t, end);
        if t.get(j) != Some(&b'.') {
            break;
        }
        let k = skip_ws(t, j + 1);
        if t.get(k) == Some(&b'*') {
            end = k + 1;
            break;
        }
        at = k;
    }
    (out, end)
}

/// A string literal at `i` (`'…'`, `E'…'`): its span and value.
fn literal_at(text: &str, i: usize) -> Option<(usize, usize, String)> {
    let t = text.as_bytes();
    let (q, esc) = match t.get(i)? {
        b'\'' => (i, false),
        b'E' | b'e' if t.get(i + 1) == Some(&b'\'') => (i + 1, true),
        _ => return None,
    };
    let mut j = q + 1;
    let mut value = Vec::new();
    loop {
        let c = *t.get(j)?;
        if c == b'\\' && esc {
            value.push(*t.get(j + 1)?);
            j += 2;
            continue;
        }
        if c == b'\'' {
            if t.get(j + 1) == Some(&b'\'') {
                value.push(b'\'');
                j += 2;
                continue;
            }
            break;
        }
        value.push(c);
        j += 1;
    }
    Some((i, j + 1, String::from_utf8(value).ok()?))
}

fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// An identifier quoted whatever it holds (the engine's alias markers).
fn quote_always(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// The alias part of a FROM item at `chain_end`: (where it starts, the alias identifier, the
/// column aliases' span and names).
struct AliasPart {
    name: Option<Ident>,
    cols: Option<(usize, usize, Vec<String>)>,
    /// Where an alias would go (after the name and any `*`).
    after_name: usize,
}

fn alias_part(text: &str, chain_end: usize, aliased: bool) -> AliasPart {
    let t = text.as_bytes();
    let mut i = skip_ws(t, chain_end);
    let mut after_name = chain_end;
    if t.get(i) == Some(&b'*') {
        after_name = i + 1;
        i = skip_ws(t, i + 1);
    }
    if !aliased {
        return AliasPart {
            name: None,
            cols: None,
            after_name,
        };
    }
    if let Some(id) = ident_at(text, i)
        && id.value == "as"
        && !text[id.start..id.end].starts_with('"')
    {
        i = skip_ws(t, id.end);
    }
    let Some(name) = ident_at(text, i) else {
        return AliasPart {
            name: None,
            cols: None,
            after_name,
        };
    };
    let j = skip_ws(t, name.end);
    let mut cols = None;
    if t.get(j) == Some(&b'(') {
        let mut k = skip_ws(t, j + 1);
        let mut names = Vec::new();
        while let Some(id) = ident_at(text, k) {
            names.push(id.value.clone());
            k = skip_ws(t, id.end);
            if t.get(k) == Some(&b',') {
                k = skip_ws(t, k + 1);
            }
        }
        if t.get(k) == Some(&b')') {
            cols = Some((j, k + 1, names));
        }
    }
    AliasPart {
        name: Some(name),
        cols,
        after_name,
    }
}

// ── names before and after ──

/// A relation's (schema, name, row type) in a catalog.
fn rel_names(cat: &Catalog, oid: u32) -> Option<(String, String, u32)> {
    if let Some(t) = cat.tables.get(&oid) {
        return Some((cat.schema_name(t.schema).to_string(), t.name.clone(), t.rowtype));
    }
    if let Some(v) = cat.views.get(&oid) {
        return Some((cat.schema_name(v.schema).to_string(), v.name.clone(), v.rowtype));
    }
    cat.sequences
        .get(&oid)
        .map(|q| (cat.schema_name(q.schema).to_string(), q.name.clone(), 0))
}

/// A relation's live column names.
fn live_columns(cat: &Catalog, oid: u32) -> Option<Vec<String>> {
    if let Some(t) = cat.tables.get(&oid) {
        return Some(t.live_columns().map(|(_, c)| c.name.clone()).collect());
    }
    cat.views
        .get(&oid)
        .map(|v| v.columns.iter().map(|c| c.0.clone()).collect())
}

/// A column's name, and its position among the live ones.
fn column(cat: &Catalog, oid: u32, attnum: i16) -> Option<(String, usize)> {
    let i = usize::try_from(attnum).ok()?.checked_sub(1)?;
    if let Some(t) = cat.tables.get(&oid) {
        let c = t.columns.get(i).filter(|c| !c.dropped)?;
        let pos = t.columns[..i].iter().filter(|c| !c.dropped).count();
        return Some((c.name.clone(), pos));
    }
    let v = cat.views.get(&oid)?;
    Some((v.columns.get(i)?.0.clone(), i))
}

fn func_names(cat: &Catalog, oid: u32) -> Option<(String, String)> {
    if let Some(f) = cat.functions.get(&oid) {
        return Some((cat.schema_name(f.schema).to_string(), f.name.clone()));
    }
    // A range type's constructors (two of the range, three of the multirange).
    let r = cat.ranges.values().find(|r| r.fn_oids.iter().take(5).any(|o| *o == oid))?;
    let i = r.fn_oids.iter().position(|o| *o == oid)?;
    let (ctor, name, schema) = if i < 2 {
        (&r.ctors.0, &r.name, r.schema)
    } else {
        (&r.ctors.1, &r.multi_name, cat.multirange_schema(r))
    };
    let name = if ctor.is_empty() { name } else { ctor };
    Some((cat.schema_name(schema).to_string(), name.clone()))
}

fn type_names(cat: &Catalog, oid: u32) -> Option<(String, String)> {
    let (schema, name) = if let Some(d) = cat.domains.get(&oid) {
        (d.schema, d.name.clone())
    } else if let Some(e) = cat.enums.get(&oid) {
        (e.schema, e.name.clone())
    } else if let Some(c) = cat.composites.get(&oid) {
        (c.schema, c.name.clone())
    } else if let Some(r) = cat.ranges.values().find(|r| r.oid == oid) {
        (r.schema, r.name.clone())
    } else if let Some(b) = cat.base_types.get(&oid) {
        (b.ty.schema, b.ty.name.to_string())
    } else {
        let r = cat.ranges.values().find(|r| r.multi_oid == oid)?;
        (r.schema, r.multi_name.clone())
    };
    Some((cat.schema_name(schema).to_string(), name))
}

/// The words a text must contain to possibly name what changed.
fn words(before: &Catalog, c: &Change) -> Vec<String> {
    let w = match c {
        Change::Rel(oid) => rel_names(before, *oid).map(|n| n.1),
        Change::Column(rel, attnum) => column(before, *rel, *attnum).map(|n| n.0),
        Change::Func(oid) => func_names(before, *oid).map(|n| n.1),
        Change::Type(oid) => type_names(before, *oid).map(|n| n.1),
        Change::Schema(oid) => before.schemas.get(oid).map(|s| s.name.clone()),
        Change::AddColumn(rel, _) | Change::DropColumn(rel, _) => {
            rel_names(before, *rel).map(|n| n.1)
        }
    };
    w.into_iter().collect()
}

/// Whether `text` may name something in `words`: one of them as a whole word (not inside a
/// longer identifier), in any letter case. A one-letter table name (`t`) is in nearly every
/// text as a letter, and each text that may name it is bound again: with many stored objects
/// every ALTER TABLE of it rebound them all.
fn mentions(text: &str, words: &[String]) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii();
    let lower = text.to_ascii_lowercase();
    words.iter().any(|w| {
        let w = w.to_ascii_lowercase();
        if w.is_empty() {
            return false;
        }
        let mut from = 0;
        while let Some(i) = lower[from..].find(&w) {
            let start = from + i;
            let end = start + w.len();
            let before = lower[..start].chars().next_back();
            let after = lower[end..].chars().next();
            if before.is_none_or(|c| !ident(c)) && after.is_none_or(|c| !ident(c)) {
                return true;
            }
            from = start + lower[start..].chars().next().map_or(1, char::len_utf8);
        }
        false
    })
}

/// The edits renaming what changed in one recorded text.
struct Text<'x> {
    text: &'x str,
    /// Recorded locations minus this are positions in `text`.
    base: i32,
    log: &'x RefLog,
    before: &'x Catalog,
    after: &'x Catalog,
    search_path: &'x [String],
}

impl Text<'_> {
    fn pos(&self, loc: i32) -> Option<usize> {
        usize::try_from(loc - self.base)
            .ok()
            .filter(|&p| p < self.text.len())
    }

    fn chain(&self, loc: i32) -> Option<(Vec<Ident>, usize)> {
        let (ch, end) = chain_at(self.text, self.pos(loc)?);
        (!ch.is_empty()).then_some((ch, end))
    }

    fn edits(&self, changes: &[Change]) -> Vec<Edit> {
        let mut out = Vec::new();
        for c in changes {
            match c {
                Change::Rel(oid) => self.rel(*oid, &mut out),
                Change::Column(rel, attnum) => self.column(*rel, *attnum, &mut out),
                Change::Func(oid) => self.func(*oid, &mut out),
                Change::Type(oid) => self.ty(*oid, &mut out),
                Change::Schema(oid) => self.schema(*oid, &mut out),
                Change::AddColumn(rel, name) => self.engine_colnames(*rel, &mut out, |cols, live| {
                    if cols.len() == live.len() {
                        let mut n = name.clone();
                        let mut i = 0;
                        while cols.contains(&n) {
                            i += 1;
                            n = crate::catalog::suffixed_name(name, &format!("_{i}"));
                        }
                        cols.push(n);
                    }
                }),
                Change::DropColumn(rel, attnum) => {
                    if let Some((_, pos)) = column(self.before, *rel, *attnum) {
                        self.engine_colnames(*rel, &mut out, |cols, _| {
                            if pos < cols.len() {
                                cols.remove(pos);
                            }
                        })
                    }
                }
            }
        }
        out
    }

    /// Replace the dotted name at `loc` (which ends in `old`) with `schema.name` (or `name`,
    /// when it wasn't qualified and `bare` allows).
    fn rename_chain(&self, loc: i32, old: &str, schema: &str, name: &str, bare: bool) -> Option<Edit> {
        let (ch, _) = self.chain(loc)?;
        if ch.last()?.value != old || ch.len() > 3 {
            return None;
        }
        let text = if ch.len() == 1 && bare {
            quote_ident(name)
        } else {
            qualified(schema, name)
        };
        Some(Edit {
            start: ch[0].start,
            end: ch.last()?.end,
            text,
        })
    }

    /// The qualifiers of a column (or whole-row) reference qualified by a relation's name:
    /// (rel name position, schema position).
    fn qualifier_edits(
        &self,
        loc: i32,
        quals: usize,
        old: &(String, String),
        new: &(String, String),
        out: &mut Vec<Edit>,
    ) {
        let Some((ch, _)) = self.chain(loc) else {
            return;
        };
        if quals == 0 || ch.len() < quals {
            return;
        }
        let rel = &ch[quals - 1];
        if rel.value != old.1 {
            return;
        }
        out.push(Edit {
            start: rel.start,
            end: rel.end,
            text: quote_ident(&new.1),
        });
        if quals >= 2 && ch[quals - 2].value == old.0 {
            out.push(Edit {
                start: ch[quals - 2].start,
                end: ch[quals - 2].end,
                text: quote_ident(&new.0),
            });
        }
    }

    fn literal_edit(&self, loc: i32, text: impl FnOnce(&str) -> String) -> Option<Edit> {
        let (start, end, value) = literal_at(self.text, self.pos(loc)?)?;
        Some(Edit {
            start,
            end,
            text: quote_literal(&text(&value)),
        })
    }

    fn rel(&self, oid: u32, out: &mut Vec<Edit>) {
        let (Some(old), Some(new)) = (rel_names(self.before, oid), rel_names(self.after, oid))
        else {
            return;
        };
        if old.0 == new.0 && old.1 == new.1 {
            return;
        }
        let moved = old.0 != new.0;
        // Unqualified, the new name must still find the relation (and no WITH query).
        let bare = !moved
            && !self.log.ctes.contains(&new.1)
            && self
                .after
                .find(None, &new.1, self.search_path)
                .is_some_and(|r| r.oid() == oid);
        for r in self.log.rels.iter().filter(|r| r.oid == oid) {
            let Some(e) = self.rename_chain(r.loc, &old.1, &new.0, &new.1, bare) else {
                continue;
            };
            let chain_end = e.end;
            out.push(e);
            match r.how {
                // References keep the old name, through an alias the engine adds.
                RelUse::From if !r.aliased => {
                    let a = alias_part(self.text, chain_end, false);
                    out.push(Edit {
                        start: a.after_name,
                        end: a.after_name,
                        text: format!(
                            " AS {}",
                            quote_always(&format!(
                                "{}{}{}{}",
                                ast::IMPLICIT,
                                old.0,
                                ast::IMPLICIT,
                                old.1
                            ))
                        ),
                    });
                }
                _ => {}
            }
        }
        // References qualified by a statement target's (or a stored expression's table's)
        // own name.
        let unaliased_target = self
            .log
            .rels
            .iter()
            .any(|r| r.oid == oid && r.how == RelUse::Target && !r.aliased);
        let olds = (old.0.clone(), old.1.clone());
        let news = (new.0.clone(), new.1.clone());
        for c in &self.log.cols {
            if c.rel == oid
                && c.quals > 0
                && c.src.is_none_or(|s| s < 0)
                && c.refname == old.1
                && (unaliased_target || c.src == Some(-1))
            {
                self.qualifier_edits(c.loc, c.quals, &olds, &news, out);
            }
        }
        for r in &self.log.rows {
            if r.rel == oid && r.src.is_none() && r.refname == old.1 {
                self.qualifier_edits(r.loc, r.quals, &olds, &news, out);
            }
        }
        for &(loc, class, o) in &self.log.literals {
            if class == crate::catalog::depend::REL && o == oid {
                out.extend(self.literal_edit(loc, |_| qualified(&new.0, &new.1)));
            }
        }
        // The relation's row type, named as a type.
        if new.2 != 0 {
            for &(loc, t) in &self.log.types {
                if t == new.2 {
                    out.extend(self.rename_chain(loc, &old.1, &new.0, &new.1, bare));
                }
            }
        }
    }

    fn column(&self, rel: u32, attnum: i16, out: &mut Vec<Edit>) {
        let (Some((old, pos)), Some((new, _))) = (
            column(self.before, rel, attnum),
            column(self.after, rel, attnum),
        ) else {
            return;
        };
        if old == new {
            return;
        }
        // FROM items whose column a USING joins on: it keeps its name there.
        let forced: HashSet<i32> = (self.log.usings.iter())
            .filter(|u| u.1 == rel && u.2 == attnum)
            .map(|u| u.0)
            .collect();
        for c in &self.log.cols {
            if c.rel != rel
                || c.attnum != attnum
                || c.name != old
                || c.src.is_none_or(|s| forced.contains(&s))
            {
                continue;
            }
            let Some((ch, _)) = self.chain(c.loc) else {
                continue;
            };
            if let Some(id) = ch.iter().skip(c.quals).find(|i| i.value == old) {
                out.push(Edit {
                    start: id.start,
                    end: id.end,
                    text: quote_ident(&new),
                });
            }
        }
        for &(loc, r, a) in &self.log.targets {
            if r == rel
                && a == attnum
                && let Some(p) = self.pos(loc)
                && let Some(id) = ident_at(self.text, p)
                && id.value == old
            {
                out.push(Edit {
                    start: id.start,
                    end: id.end,
                    text: quote_ident(&new),
                });
            }
        }
        // Column aliases: the engine's follow the column, except where a USING keeps it.
        let Some(live) = live_columns(self.before, rel) else {
            return;
        };
        let Some(names) = rel_names(self.before, rel) else {
            return;
        };
        for r in self.log.rels.iter().filter(|r| r.oid == rel && r.how == RelUse::From) {
            let Some((_, chain_end)) = self.chain(r.loc) else {
                continue;
            };
            let a = alias_part(self.text, chain_end, r.aliased);
            let implicit = a
                .name
                .as_ref()
                .is_some_and(|n| n.value.starts_with(ast::IMPLICIT));
            let list = |keep: Option<&[String]>| -> String {
                let all: Vec<String> = (live.iter().enumerate())
                    .map(|(i, n)| {
                        quote_ident(keep.and_then(|k| k.get(i)).map_or(n.as_str(), |k| k.as_str()))
                    })
                    .collect();
                format!("({})", all.join(", "))
            };
            let is_forced = forced.contains(&r.loc);
            match (&a.name, &a.cols) {
                (Some(_), Some((start, end, cols))) if implicit => {
                    if !is_forced && cols.get(pos).is_some_and(|c| *c == old) {
                        let mut cols = cols.clone();
                        cols[pos] = new.clone();
                        out.push(Edit {
                            start: *start,
                            end: *end,
                            text: list(Some(&cols)),
                        });
                    }
                }
                (Some(_), Some((start, end, cols))) if is_forced && cols.len() <= pos => {
                    // Written column aliases that don't reach the column: the engine's
                    // complete them.
                    let name = a.name.as_ref().unwrap();
                    let full = (cols.iter().cloned())
                        .chain(live.iter().skip(cols.len()).cloned())
                        .collect::<Vec<_>>();
                    out.push(Edit {
                        start: name.start.min(*start),
                        end: *end,
                        text: format!(
                            "{}{}",
                            quote_always(&format!("{}{}{}", ast::IMPLICIT, ast::IMPLICIT, name.value)),
                            list(Some(&full))
                        ),
                    });
                }
                (Some(name), None) if is_forced => {
                    let alias = if implicit {
                        format!("\"{}\"", name.value.replace('"', "\"\""))
                    } else {
                        quote_always(&format!("{}{}{}", ast::IMPLICIT, ast::IMPLICIT, name.value))
                    };
                    out.push(Edit {
                        start: name.start,
                        end: name.end,
                        text: format!("{alias}{}", list(None)),
                    });
                }
                (None, _) if is_forced => {
                    out.push(Edit {
                        start: a.after_name,
                        end: a.after_name,
                        text: format!(
                            " AS {}{}",
                            quote_always(&format!(
                                "{}{}{}{}",
                                ast::IMPLICIT,
                                names.0,
                                ast::IMPLICIT,
                                names.1
                            )),
                            list(None)
                        ),
                    });
                }
                _ => {}
            }
        }
    }

    /// Change the column aliases the engine keeps for `rel`'s FROM items (`f` gets them and
    /// the relation's live columns before the change).
    fn engine_colnames(
        &self,
        rel: u32,
        out: &mut Vec<Edit>,
        f: impl Fn(&mut Vec<String>, &[String]),
    ) {
        let Some(live) = live_columns(self.before, rel) else {
            return;
        };
        for r in self.log.rels.iter().filter(|r| r.oid == rel && r.how == RelUse::From) {
            let Some((_, chain_end)) = self.chain(r.loc) else {
                continue;
            };
            let a = alias_part(self.text, chain_end, r.aliased);
            if !a.name.as_ref().is_some_and(|n| n.value.starts_with(ast::IMPLICIT)) {
                continue;
            }
            let Some((start, end, cols)) = a.cols else {
                continue;
            };
            let mut cols = cols;
            f(&mut cols, &live);
            let all: Vec<String> = cols.iter().map(|c| quote_ident(c)).collect();
            out.push(Edit {
                start,
                end,
                text: format!("({})", all.join(", ")),
            });
        }
    }

    fn func(&self, oid: u32, out: &mut Vec<Edit>) {
        let (Some(old), Some(new)) = (func_names(self.before, oid), func_names(self.after, oid))
        else {
            return;
        };
        if old == new {
            return;
        }
        let bare = old.0 == new.0;
        for &(loc, f) in &self.log.funcs {
            if f == oid {
                out.extend(self.rename_chain(loc, &old.1, &new.0, &new.1, bare));
            }
        }
        for &(loc, class, o) in &self.log.literals {
            if class == crate::catalog::depend::PROC && o == oid {
                out.extend(self.literal_edit(loc, |v| {
                    let args = v.find('(').map_or("", |i| &v[i..]);
                    format!("{}{args}", qualified(&new.0, &new.1))
                }));
            }
        }
    }

    fn ty(&self, oid: u32, out: &mut Vec<Edit>) {
        let (Some(old), Some(new)) = (type_names(self.before, oid), type_names(self.after, oid))
        else {
            return;
        };
        if old == new {
            return;
        }
        let bare = old.0 == new.0;
        for &(loc, t) in &self.log.types {
            if t == oid {
                out.extend(self.rename_chain(loc, &old.1, &new.0, &new.1, bare));
            }
        }
        for &(loc, class, o) in &self.log.literals {
            if class == crate::catalog::depend::TYPE && o == oid {
                out.extend(self.literal_edit(loc, |_| qualified(&new.0, &new.1)));
            }
        }
    }

    fn schema(&self, oid: u32, out: &mut Vec<Edit>) {
        let (Some(old), Some(new)) = (self.before.schemas.get(&oid), self.after.schemas.get(&oid))
        else {
            return;
        };
        if old.name == new.name {
            return;
        }
        let (old, new) = (old.name.clone(), new.name.clone());
        let requalify = |loc: i32, out: &mut Vec<Edit>| {
            if let Some((ch, _)) = self.chain(loc)
                && ch.len() >= 2
                && ch[ch.len() - 2].value == old
            {
                let s = &ch[ch.len() - 2];
                out.push(Edit {
                    start: s.start,
                    end: s.end,
                    text: quote_ident(&new),
                });
            }
        };
        for r in &self.log.rels {
            requalify(r.loc, out);
        }
        for &(loc, _) in self.log.funcs.iter().chain(&self.log.types) {
            requalify(loc, out);
        }
        // `schema.table.column`: through a FROM item, an alias keeps the old schema; else the
        // schema is renamed.
        let mut keep: HashSet<i32> = HashSet::new();
        for (loc, src, quals) in (self.log.cols.iter().map(|c| (c.loc, c.src, c.quals)))
            .chain(self.log.rows.iter().map(|r| (r.loc, r.src, r.quals)))
        {
            if quals < 2 {
                continue;
            }
            let Some((ch, _)) = self.chain(loc) else {
                continue;
            };
            if ch.len() < quals || ch[quals - 2].value != old {
                continue;
            }
            match src {
                Some(s) if s >= 0 => {
                    keep.insert(s);
                }
                _ => out.push(Edit {
                    start: ch[quals - 2].start,
                    end: ch[quals - 2].end,
                    text: quote_ident(&new),
                }),
            }
        }
        for r in self.log.rels.iter().filter(|r| keep.contains(&r.loc) && !r.aliased) {
            let Some((ch, end)) = self.chain(r.loc) else {
                continue;
            };
            let a = alias_part(self.text, end, false);
            out.push(Edit {
                start: a.after_name,
                end: a.after_name,
                text: format!(
                    " AS {}",
                    quote_always(&format!(
                        "{}{}{}{}",
                        ast::IMPLICIT,
                        old,
                        ast::IMPLICIT,
                        ch.last().map_or("", |i| i.value.as_str())
                    ))
                ),
            });
        }
        for &(loc, class, o) in &self.log.literals {
            if class == crate::catalog::depend::REL
                && let Some((schema, name, _)) = rel_names(self.after, o)
                && schema == new
            {
                out.extend(self.literal_edit(loc, |v| {
                    if v.contains('.') {
                        qualified(&schema, &name)
                    } else {
                        v.to_string()
                    }
                }));
            }
        }
    }
}

/// After names changed in a statement, its output columns keep theirs (Postgres keeps a
/// target's name in the tree): an unnamed select-list item whose name (FigureColname)
/// changed is given the one it had.
fn keep_names(old: &str, new: String) -> String {
    let (Ok(a), Ok(mut b)) = (ast::parse_stmt(old), ast::parse_stmt(&new)) else {
        return new;
    };
    let mut changed = false;
    pair_stmt(&a, &mut b, &mut changed);
    if !changed {
        return new;
    }
    ast::deparse_stmt(&b).unwrap_or(new)
}

fn pair_stmt(a: &pg_query::protobuf::Node, b: &mut pg_query::protobuf::Node, changed: &mut bool) {
    use pg_query::NodeEnum as N;
    match (a.node.as_ref(), b.node.as_mut()) {
        (Some(N::SelectStmt(x)), Some(N::SelectStmt(y))) => pair_select(x, y, changed),
        (Some(N::InsertStmt(x)), Some(N::InsertStmt(y))) => {
            if let (Some(p), Some(q)) = (x.select_stmt.as_deref(), y.select_stmt.as_deref_mut()) {
                pair_stmt(p, q, changed);
            }
            if let (Some(p), Some(q)) = (&x.returning_clause, &mut y.returning_clause) {
                pair_targets(&p.exprs, &mut q.exprs, changed);
            }
        }
        (Some(N::UpdateStmt(x)), Some(N::UpdateStmt(y))) => {
            if let (Some(p), Some(q)) = (&x.returning_clause, &mut y.returning_clause) {
                pair_targets(&p.exprs, &mut q.exprs, changed);
            }
        }
        (Some(N::DeleteStmt(x)), Some(N::DeleteStmt(y))) => {
            if let (Some(p), Some(q)) = (&x.returning_clause, &mut y.returning_clause) {
                pair_targets(&p.exprs, &mut q.exprs, changed);
            }
        }
        _ => {}
    }
}

fn pair_select(
    x: &pg_query::protobuf::SelectStmt,
    y: &mut pg_query::protobuf::SelectStmt,
    changed: &mut bool,
) {
    use pg_query::NodeEnum as N;
    if let (Some(p), Some(q)) = (&x.with_clause, &mut y.with_clause) {
        for (c, d) in p.ctes.iter().zip(q.ctes.iter_mut()) {
            if let (Some(N::CommonTableExpr(c)), Some(N::CommonTableExpr(d))) =
                (c.node.as_ref(), d.node.as_mut())
                && let (Some(c), Some(d)) = (c.ctequery.as_deref(), d.ctequery.as_deref_mut())
            {
                pair_stmt(c, d, changed);
            }
        }
    }
    if let (Some(p), Some(q)) = (x.larg.as_deref(), y.larg.as_deref_mut()) {
        pair_select(p, q, changed);
    }
    pair_targets(&x.target_list, &mut y.target_list, changed);
    fn from(a: &pg_query::protobuf::Node, b: &mut pg_query::protobuf::Node, changed: &mut bool) {
        use pg_query::NodeEnum as N;
        match (a.node.as_ref(), b.node.as_mut()) {
            (Some(N::RangeSubselect(p)), Some(N::RangeSubselect(q))) => {
                if let (Some(p), Some(q)) = (p.subquery.as_deref(), q.subquery.as_deref_mut()) {
                    pair_stmt(p, q, changed);
                }
            }
            (Some(N::JoinExpr(p)), Some(N::JoinExpr(q))) => {
                for (p, q) in [(&p.larg, &mut q.larg), (&p.rarg, &mut q.rarg)] {
                    if let (Some(p), Some(q)) = (p.as_deref(), q.as_deref_mut()) {
                        from(p, q, changed);
                    }
                }
            }
            _ => {}
        }
    }
    for (a, b) in x.from_clause.iter().zip(y.from_clause.iter_mut()) {
        from(a, b, changed);
    }
}

fn pair_targets(
    a: &[pg_query::protobuf::Node],
    b: &mut [pg_query::protobuf::Node],
    changed: &mut bool,
) {
    use pg_query::NodeEnum as N;
    if a.len() != b.len() {
        return;
    }
    for (p, q) in a.iter().zip(b.iter_mut()) {
        if let (Some(N::ResTarget(p)), Some(N::ResTarget(q))) = (p.node.as_ref(), q.node.as_mut())
            && p.name.is_empty()
            && q.name.is_empty()
            && let (Some(pv), Some(qv)) = (p.val.as_deref(), q.val.as_deref())
            && !matches!(pv.node.as_ref(), Some(N::ColumnRef(c)) if matches!(c.fields.last().and_then(|f| f.node.as_ref()), Some(N::AStar(_))))
        {
            let (old, new) = (
                crate::sql::binder::figure_colname(pv),
                crate::sql::binder::figure_colname(qv),
            );
            if old != new {
                q.name = old;
                *changed = true;
            }
        }
    }
}

/// `text` with the names `log` recorded that have no schema qualified (relations,
/// functions, types, reg* literals). None: nothing to qualify.
fn qualify_text(text: &str, log: &RefLog, cat: &Catalog, search_path: &[String]) -> Option<String> {
    let t = Text {
        text,
        base: 0,
        log,
        before: cat,
        after: cat,
        search_path,
    };
    let mut edits = Vec::new();
    let mut bare = |loc: i32, schema: &str, name: &str| {
        if let Some((ch, _)) = t.chain(loc)
            && ch.len() == 1
            && ch[0].value == name
        {
            edits.push(Edit {
                start: ch[0].start,
                end: ch[0].end,
                text: qualified(schema, name),
            });
        }
    };
    for r in &log.rels {
        if let Some((schema, name, _)) = rel_names(cat, r.oid) {
            bare(r.loc, &schema, &name);
        }
    }
    for &(loc, oid) in &log.funcs {
        if let Some((schema, name)) = func_names(cat, oid) {
            bare(loc, &schema, &name);
        }
    }
    for &(loc, oid) in &log.types {
        if let Some((schema, name)) = type_names(cat, oid) {
            bare(loc, &schema, &name);
        }
    }
    // A user operator: `OPERATOR(schema.op)`.
    for &(loc, oid) in &log.opers {
        if let Some(o) = cat.operators.get(&oid)
            && let Some(p) = t.pos(loc)
            && text[p..].starts_with(o.name.as_str())
        {
            edits.push(Edit {
                start: p,
                end: p + o.name.len(),
                text: format!("OPERATOR({}.{})", quote_ident(cat.schema_name(o.schema)), o.name),
            });
        }
    }
    for &(loc, class, oid) in &log.literals {
        if class == crate::catalog::depend::REL
            && let Some((schema, name, _)) = rel_names(cat, oid)
            && let Some(p) = t.pos(loc)
            && let Some((start, end, value)) = literal_at(text, p)
            && !value.contains('.')
        {
            edits.push(Edit {
                start,
                end,
                text: quote_literal(&qualified(&schema, &name)),
            });
        }
    }
    if edits.is_empty() {
        return None;
    }
    Some(apply(text, edits))
}

/// Each join in a query (walking its levels' FROM items, children first): `f` gets it and
/// the locations of the relation names (RangeVars) under it.
fn each_join(
    n: &mut pg_query::protobuf::Node,
    f: &mut dyn FnMut(&mut pg_query::protobuf::JoinExpr, &[i32]),
) {
    use pg_query::NodeEnum as N;
    type F<'f> = &'f mut dyn FnMut(&mut pg_query::protobuf::JoinExpr, &[i32]);
    fn select(s: &mut pg_query::protobuf::SelectStmt, f: F) {
        if let Some(w) = s.with_clause.as_mut() {
            for c in &mut w.ctes {
                if let Some(N::CommonTableExpr(cte)) = c.node.as_mut()
                    && let Some(q) = cte.ctequery.as_deref_mut()
                {
                    each_join(q, f);
                }
            }
        }
        for x in [s.larg.as_deref_mut(), s.rarg.as_deref_mut()].into_iter().flatten() {
            select(x, f);
        }
        for item in &mut s.from_clause {
            from(item, f);
        }
    }
    /// The relation names under a FROM item.
    fn from(n: &mut pg_query::protobuf::Node, f: F) -> Vec<i32> {
        match n.node.as_mut() {
            Some(N::RangeVar(rv)) => vec![rv.location],
            Some(N::RangeSubselect(rs)) => {
                if let Some(q) = rs.subquery.as_deref_mut() {
                    each_join(q, f);
                }
                Vec::new()
            }
            Some(N::JoinExpr(j)) => {
                let mut locs = Vec::new();
                for x in [j.larg.as_deref_mut(), j.rarg.as_deref_mut()].into_iter().flatten() {
                    locs.extend(from(x, f));
                }
                f(j, &locs);
                locs
            }
            _ => Vec::new(),
        }
    }
    if let Some(N::SelectStmt(s)) = n.node.as_mut() {
        select(s, f);
    }
}

/// The recorded join an AST join is (and its index): the one whose relations are the
/// recorded ones under it.
fn join_rec<'l>(
    log: &'l RefLog,
    locs: &[i32],
) -> Option<(usize, &'l crate::sql::binder::refs::JoinRec)> {
    let mut rels: Vec<i32> = (locs.iter().copied())
        .filter(|l| log.rels.iter().any(|r| r.loc == *l && r.how == RelUse::From))
        .collect();
    rels.sort_unstable();
    if rels.is_empty() {
        return None;
    }
    log.joins
        .iter()
        .enumerate()
        .find(|(_, j)| j.members() == rels)
}

/// A binder recording what the names of one text resolve to, over `cat`.
fn recorder<'b, 'a>(ctx: &'b crate::exec::ExecCtx<'a>, cat: &Arc<Catalog>) -> Binder<'b, 'a> {
    let mut b = Binder::new(ctx, Vec::new());
    b.cat = cat.clone();
    b.deparse = true;
    b.unplanned = true;
    b.refs = Some(Box::default());
    b
}

/// The prefix `ast::parse_expr` puts before an expression.
const EXPR_BASE: i32 = "SELECT ".len() as i32;

/// A log recorded binding an expression parsed by `ast::parse_expr`, its positions in the
/// expression's own text.
fn expr_log(mut log: RefLog) -> RefLog {
    for r in &mut log.rels {
        r.loc -= EXPR_BASE;
    }
    for x in log.funcs.iter_mut().chain(log.types.iter_mut()).chain(log.opers.iter_mut()) {
        x.0 -= EXPR_BASE;
    }
    for x in &mut log.literals {
        x.0 -= EXPR_BASE;
    }
    log
}

impl Ddl<'_, '_> {
    /// Bind with `f`, recording, then rename what changed in `text`. None: unchanged (or not
    /// bindable).
    fn rewrite_text(
        &self,
        before: &Arc<Catalog>,
        changes: &[Change],
        text: &str,
        base: i32,
        f: impl FnOnce(&mut Binder) -> Result<()>,
    ) -> Option<String> {
        let mut b = recorder(self.ctx, before);
        f(&mut b).ok()?;
        let log = b.refs.take()?;
        let t = Text {
            text,
            base,
            log: &log,
            before,
            after: &self.cat,
            search_path: self.search_path(),
        };
        let edits = t.edits(changes);
        if edits.is_empty() {
            return None;
        }
        Some(apply(text, edits))
    }

    /// A view's joins with column aliases after a column was added to (or dropped from) a
    /// relation under them: the aliases keep standing for the same columns, the new column
    /// taking its own name (made unique among them), as Postgres shows them. None: no change.
    fn join_aliases(
        &self,
        before: &Arc<Catalog>,
        changes: &[Change],
        v: &crate::catalog::ViewDef,
        text: &str,
    ) -> Option<String> {
        if !changes
            .iter()
            .any(|c| matches!(c, Change::AddColumn(..) | Change::DropColumn(..)))
        {
            return None;
        }
        let after = Arc::new(self.cat.clone());
        let mut node = ast::parse_stmt(text).ok()?;
        // (Bound as it was: a column added may make it ambiguous until named apart.)
        let tmp = crate::catalog::ViewDef {
            query: text.to_string(),
            ..v.clone()
        };
        let mut b = recorder(self.ctx, before);
        b.bind_view_query(&tmp).ok()?;
        let log = b.refs.take()?;
        let mut changed = false;
        for c in changes {
            // (relation, the column's attnum, whether it was added under that name)
            let (rel, attnum, name) = match c {
                Change::AddColumn(rel, name) => (*rel, 0, Some(name.clone())),
                Change::DropColumn(rel, attnum) => (*rel, *attnum, None),
                _ => continue,
            };
            each_join(&mut node, &mut |j, locs| {
                let Some((_, rec)) = join_rec(&log, locs) else {
                    return;
                };
                if rec.aliases == 0 {
                    return;
                }
                // A new column comes after its relation's others.
                let pos = match name {
                    Some(_) => rec.cols.iter().rposition(|x| x.0 == rel).map(|p| p + 1),
                    None => rec.cols.iter().position(|x| *x == (rel, attnum)),
                };
                let Some(pos) = pos else {
                    return;
                };
                let Some(alias) = j.alias.as_mut() else {
                    return;
                };
                let mut names = ast::names(&alias.colnames);
                if pos >= names.len() && name.is_some() || pos > names.len() {
                    return;
                }
                match &name {
                    Some(n) => {
                        let mut u = n.clone();
                        let mut i = 0;
                        while names.contains(&u) {
                            i += 1;
                            u = crate::catalog::suffixed_name(n, &format!("_{i}"));
                        }
                        names.insert(pos, u);
                    }
                    None if pos < names.len() => {
                        names.remove(pos);
                    }
                    None => return,
                }
                alias.colnames = names.iter().map(|n| string_node(n)).collect();
                changed = true;
            });
        }
        // A column added takes a name of its own where it would clash with a USING name.
        if changes.iter().any(|c| matches!(c, Change::AddColumn(..)))
            && using_names(&mut node, &log, &after)
        {
            changed = true;
        }
        if !changed {
            return None;
        }
        ast::deparse_stmt(&node).ok()
    }

    /// An expression stored over `table` (a default, a constraint, an index key …).
    fn rewrite_table_expr(
        &self,
        before: &Arc<Catalog>,
        changes: &[Change],
        table: &Arc<crate::catalog::Table>,
        sql: &str,
        clause: Clause,
    ) -> Option<String> {
        let node = ast::parse_expr(sql).ok()?;
        self.rewrite_text(before, changes, sql, EXPR_BASE, |b| {
            b.bind_over_table(table, &node, clause).map(|_| ())
        })
    }

    /// Make every stored definition follow `changes`, already made in the catalog; `before`:
    /// the catalog from before them.
    pub(super) fn follow_renames(&mut self, before: &Arc<Catalog>, changes: &[Change]) -> Result<()> {
        let words: Vec<String> = changes.iter().flat_map(|c| words(before, c)).collect();
        if words.is_empty() {
            return Ok(());
        }
        let hit = |s: &str| mentions(s, &words);
        // Views and materialized views.
        for v in before.views.values() {
            if !hit(&v.query) {
                continue;
            }
            // (A column added only renames what clashes with it: `join_aliases`.)
            let added = changes.iter().any(|c| matches!(c, Change::AddColumn(..)));
            let mut q = if added {
                None
            } else {
                self.rewrite_text(before, changes, &v.query, 0, |b| {
                    b.bind_view_query(v).map(|_| ())
                })
            };
            // Joins with column aliases over a relation whose columns came or went.
            let text = q.clone().unwrap_or_else(|| v.query.clone());
            if let Some(j) = self.join_aliases(before, changes, v, &text) {
                q = Some(j);
            }
            let Some(q) = q else {
                continue;
            };
            if let Some(cur) = self.cat.views.get(&v.oid).cloned()
                && cur.query == v.query
            {
                let mut nv = (*cur).clone();
                nv.query = keep_names(&v.query, q);
                self.cat.put_view(nv);
            }
        }
        // Tables: defaults, generation expressions, CHECK constraints, partition keys.
        for t in before.tables.values() {
            let Some(cur) = self.cat.tables.get(&t.oid).cloned() else {
                continue;
            };
            let mut nt = (*cur).clone();
            let mut changed = false;
            for (i, c) in t.columns.iter().enumerate() {
                if let Some(d) = c.default.as_deref().filter(|d| hit(d))
                    && let Some(new) = self.rewrite_table_expr(before, changes, t, d, Clause::Default)
                    && nt.columns.get(i).is_some_and(|x| x.default.as_deref() == Some(d))
                {
                    nt.columns[i].default = Some(new);
                    changed = true;
                }
                if let Some(g) = c.generated.as_deref().filter(|g| hit(g))
                    && let Some(new) =
                        self.rewrite_table_expr(before, changes, t, g, Clause::Generated)
                    && nt.columns.get(i).is_some_and(|x| x.generated.as_deref() == Some(g))
                {
                    nt.columns[i].generated = Some(new);
                    changed = true;
                }
            }
            for (i, con) in t.constraints.iter().enumerate() {
                if let ConstraintKind::Check { expr, .. } = &con.kind
                    && hit(expr)
                    && let Some(new) = self.rewrite_table_expr(before, changes, t, expr, Clause::Check)
                    && let Some(ConstraintKind::Check { expr: cur, .. }) =
                        nt.constraints.get_mut(i).map(|c| &mut c.kind)
                    && cur == expr
                {
                    *cur = new;
                    changed = true;
                }
            }
            if let Some(pk) = &t.partkey {
                for (i, k) in pk.cols.iter().enumerate() {
                    if let Some(e) = k.expr.as_deref().filter(|e| hit(e))
                        && let Some(new) =
                            self.rewrite_table_expr(before, changes, t, e, Clause::IndexExpr)
                        && let Some(cur) = nt.partkey.as_mut().and_then(|p| p.cols.get_mut(i))
                        && cur.expr.as_deref() == Some(e)
                    {
                        cur.expr = Some(new);
                        changed = true;
                    }
                }
            }
            if changed {
                self.cat.put_storage(nt);
            }
        }
        // Index expressions and predicates.
        for ix in before.indexes.values() {
            let Some(t) = before.index_table(ix.table) else {
                continue;
            };
            let Some(cur) = self.cat.indexes.get(&ix.oid).cloned() else {
                continue;
            };
            let mut nix = (*cur).clone();
            let mut changed = false;
            for (i, c) in ix.columns.iter().enumerate() {
                if let IndexKey::Expr(e) = &c.key
                    && hit(e)
                    && let Some(new) = self.rewrite_table_expr(before, changes, &t, e, Clause::IndexExpr)
                    && let Some(IndexKey::Expr(cur)) = nix.columns.get_mut(i).map(|c| &mut c.key)
                    && cur == e
                {
                    *cur = new;
                    changed = true;
                }
            }
            if let Some(p) = ix.predicate.as_deref().filter(|p| hit(p))
                && let Some(new) = self.rewrite_table_expr(before, changes, &t, p, Clause::Check)
                && nix.predicate.as_deref() == Some(p)
            {
                nix.predicate = Some(new);
                changed = true;
            }
            if changed {
                self.cat.put_index(nix);
            }
        }
        // Statistics objects' expressions.
        for st in before.statistics.values() {
            let Some(t) = before.tables.get(&st.table).cloned() else {
                continue;
            };
            let mut exprs = st.exprs.clone();
            let mut changed = false;
            for e in &mut exprs {
                if hit(e)
                    && let Some(new) = self.rewrite_table_expr(before, changes, &t, e, Clause::IndexExpr)
                {
                    *e = new;
                    changed = true;
                }
            }
            if changed
                && let Some(cur) = self.cat.statistics.get(&st.oid).cloned()
                && cur.exprs == st.exprs
            {
                let mut ns = (*cur).clone();
                ns.exprs = exprs;
                self.cat.statistics.insert(ns.oid, Arc::new(ns));
            }
        }
        // Policies.
        for p in before.policies.values() {
            let Some(t) = before.tables.get(&p.table).cloned() else {
                continue;
            };
            let rw = |s: &Option<String>| {
                s.as_deref()
                    .filter(|s| hit(s))
                    .and_then(|s| self.rewrite_table_expr(before, changes, &t, s, Clause::Where))
            };
            let (qual, check) = (rw(&p.qual), rw(&p.with_check));
            if (qual.is_some() || check.is_some())
                && let Some(cur) = self.cat.policies.get(&p.oid).cloned()
                && cur.qual == p.qual
                && cur.with_check == p.with_check
            {
                let mut np = (*cur).clone();
                if qual.is_some() {
                    np.qual = qual;
                }
                if check.is_some() {
                    np.with_check = check;
                }
                self.cat.policies.insert(np.oid, Arc::new(np));
            }
        }
        // Trigger WHEN conditions.
        for tg in before.triggers.values() {
            let Some(w) = tg.when.as_deref().filter(|w| hit(w)) else {
                continue;
            };
            let table = before
                .tables
                .get(&tg.table)
                .cloned()
                .or_else(|| before.views.get(&tg.table).map(|v| v.as_table()));
            let Some(table) = table else {
                continue;
            };
            let Some(new) = self.rewrite_text(before, changes, w, EXPR_BASE, |b| {
                b.bind_trigger_when(&table, w).map(|_| ())
            }) else {
                continue;
            };
            if let Some(cur) = self.cat.triggers.get(&tg.oid).cloned()
                && cur.when.as_deref() == Some(w)
            {
                let mut nt = (*cur).clone();
                nt.when = Some(new);
                self.cat.triggers.insert(nt.oid, Arc::new(nt));
            }
        }
        // Domain CHECKs.
        for d in before.domains.values() {
            let mut checks = d.checks.clone();
            let mut changed = false;
            for (_, sql) in &mut checks {
                if !hit(sql) {
                    continue;
                }
                let Ok(node) = ast::parse_expr(sql) else {
                    continue;
                };
                let base = d.base.clone();
                if let Some(new) = self.rewrite_text(before, changes, sql, EXPR_BASE, |b| {
                    b.bind_domain_check(&node, &base).map(|_| ())
                }) {
                    *sql = new;
                    changed = true;
                }
            }
            if changed
                && let Some(cur) = self.cat.domains.get(&d.oid).cloned()
                && cur.checks == d.checks
            {
                let mut nd = (*cur).clone();
                nd.checks = checks;
                self.cat.domains.insert(nd.oid, Arc::new(nd));
            }
        }
        // Rules: the condition and each action.
        for r in before.rules.values() {
            if !r.qual.as_deref().is_some_and(hit) && !r.actions.iter().any(|a| hit(a)) {
                continue;
            }
            let table = before
                .tables
                .get(&r.table)
                .cloned()
                .or_else(|| before.views.get(&r.table).map(|v| v.as_table()));
            let Some(table) = table else {
                continue;
            };
            let mut nr = (**r).clone();
            let mut changed = false;
            if let Some(q) = r.qual.as_deref().filter(|q| hit(q))
                && let Ok(node) = ast::parse_expr(q)
                && let Some(new) = self.rewrite_text(before, changes, q, EXPR_BASE, |b| {
                    b.check_rule(&table, r.event, Some(&node), &[], r.instead)
                        .map(|_| ())
                })
            {
                nr.qual = Some(new);
                changed = true;
            }
            for (i, a) in r.actions.iter().enumerate() {
                if !hit(a) {
                    continue;
                }
                let Ok(node) = ast::parse_stmt(a) else {
                    continue;
                };
                // (Bound without the condition, whose positions are in its own text.)
                if let Some(new) = self.rewrite_text(before, changes, a, 0, |b| {
                    b.check_rule(&table, r.event, None, std::slice::from_ref(&node), r.instead)
                        .map(|_| ())
                }) {
                    nr.actions[i] = keep_names(a, new);
                    changed = true;
                }
            }
            if changed
                && let Some(cur) = self.cat.rules.get(&r.oid)
                && cur.qual == r.qual
                && cur.actions == r.actions
            {
                self.cat.rules.insert(nr.oid, Arc::new(nr));
            }
        }
        // SQL-standard function bodies, statement by statement.
        for f in before.functions.values() {
            if f.sql_body.is_none() || !hit(&f.body) {
                continue;
            }
            let Ok(stmts) = crate::sql::parse::parse(&f.body) else {
                continue;
            };
            let args: Vec<Expr> = f
                .args
                .iter()
                .map(|(_, t)| Expr::Const(Value::Null, t.clone()))
                .collect();
            let mut edits = Vec::new();
            for raw in &stmts {
                let start = (raw.stmt_location.max(0) as usize).min(f.body.len());
                let text = crate::sql::parse::stmt_text(&f.body, raw);
                let lead = f.body[start..].len() - f.body[start..].trim_start().len();
                let text = text.trim_end_matches(';');
                if !hit(text) {
                    continue;
                }
                let one = crate::catalog::Function {
                    body: text.to_string(),
                    config: Vec::new(),
                    ..(**f).clone()
                };
                let mut b = recorder(self.ctx, before);
                if b.bind_user_body(&one, &args).is_err() {
                    continue;
                }
                let Some(log) = b.refs.take() else {
                    continue;
                };
                let t = Text {
                    text,
                    base: 0,
                    log: &log,
                    before,
                    after: &self.cat,
                    search_path: self.search_path(),
                };
                let off = start + lead;
                let mine = t.edits(changes);
                if !mine.is_empty() {
                    // The statement as a whole (its output names kept).
                    edits.push(Edit {
                        start: off,
                        end: off + text.len(),
                        text: keep_names(text, apply(text, mine)),
                    });
                }
            }
            if edits.is_empty() {
                continue;
            }
            let body = apply(&f.body, edits);
            if let Some(cur) = self.cat.functions.get(&f.oid).cloned()
                && cur.body == f.body
            {
                let mut nf = (*cur).clone();
                nf.body = body;
                self.cat.functions.insert(nf.oid, Arc::new(nf));
            }
        }
        Ok(())
    }
}

// ── a new view's query, as stored ──

/// Fixes in a view's query what Postgres's stored tree fixes when the view is made: the
/// columns a star stands for, the columns a NATURAL join joins on, the relations named (by
/// schema), and the output names of FROM subqueries and WITH queries.
struct Normalize<'x> {
    log: &'x RefLog,
    natural: &'x std::collections::HashMap<usize, Vec<String>>,
    cat: &'x Catalog,
    changed: bool,
}

fn string_node(s: &str) -> pg_query::protobuf::Node {
    ast::mk(pg_query::NodeEnum::String(pg_query::protobuf::String {
        sval: s.to_string(),
    }))
}

impl Normalize<'_> {
    fn stmt(&mut self, n: &mut pg_query::protobuf::Node, named: bool) {
        if let Some(pg_query::NodeEnum::SelectStmt(s)) = n.node.as_mut() {
            self.select(s, named);
        }
    }

    fn select(&mut self, s: &mut pg_query::protobuf::SelectStmt, named: bool) {
        use pg_query::NodeEnum as N;
        use pg_query::protobuf as pb;
        if let Some(w) = s.with_clause.as_mut() {
            for c in &mut w.ctes {
                if let Some(N::CommonTableExpr(cte)) = c.node.as_mut()
                    && let Some(q) = cte.ctequery.as_deref_mut()
                {
                    self.stmt(q, true);
                }
            }
        }
        if let Some(l) = s.larg.as_deref_mut() {
            self.select(l, named);
        }
        if let Some(r) = s.rarg.as_deref_mut() {
            self.select(r, false);
        }
        let mut targets = Vec::with_capacity(s.target_list.len());
        for mut t in std::mem::take(&mut s.target_list) {
            if let Some(N::ResTarget(rt)) = t.node.as_mut() {
                // A star: the columns it stands for, under their names.
                if let Some(N::ColumnRef(c)) = rt.val.as_deref().and_then(|v| v.node.as_ref())
                    && matches!(c.fields.last().and_then(|f| f.node.as_ref()), Some(N::AStar(_)))
                {
                    if let Some((_, Some(items))) =
                        self.log.stars.iter().find(|(loc, _)| *loc == c.location)
                    {
                        for (quals, name) in items {
                            let fields = (quals.iter().chain(std::iter::once(name)))
                                .map(|q| string_node(q))
                                .collect();
                            targets.push(ast::mk(N::ResTarget(Box::new(pb::ResTarget {
                                name: name.clone(),
                                indirection: Vec::new(),
                                val: Some(Box::new(ast::mk(N::ColumnRef(pb::ColumnRef {
                                    fields,
                                    location: -1,
                                })))),
                                location: -1,
                            }))));
                        }
                        self.changed = true;
                        continue;
                    }
                    targets.push(t);
                    continue;
                }
                if let Some(v) = rt.val.as_deref_mut() {
                    self.expr(v);
                    if named && rt.name.is_empty() {
                        rt.name = crate::sql::binder::figure_colname(v);
                        self.changed = true;
                    }
                }
            }
            targets.push(t);
        }
        s.target_list = targets;
        for f in &mut s.from_clause {
            self.walk_from_item(f);
        }
        for e in [s.where_clause.as_deref_mut(), s.having_clause.as_deref_mut()]
            .into_iter()
            .flatten()
        {
            self.expr(e);
        }
    }

    fn walk_from_item(&mut self, n: &mut pg_query::protobuf::Node) {
        use pg_query::NodeEnum as N;
        match n.node.as_mut() {
            Some(N::RangeVar(rv)) => {
                if rv.schemaname.is_empty()
                    && let Some(r) = (self.log.rels.iter())
                        .find(|r| r.loc == rv.location && r.how == RelUse::From)
                    && let Some((schema, name, _)) = rel_names(self.cat, r.oid)
                    && name == rv.relname
                {
                    rv.schemaname = schema;
                    self.changed = true;
                }
            }
            Some(N::RangeSubselect(rs)) => {
                if let Some(q) = rs.subquery.as_deref_mut() {
                    self.stmt(q, true);
                }
            }
            Some(N::JoinExpr(j)) => {
                let key = &**j as *const pg_query::protobuf::JoinExpr as usize;
                if let Some(l) = j.larg.as_deref_mut() {
                    self.walk_from_item(l);
                }
                if let Some(r) = j.rarg.as_deref_mut() {
                    self.walk_from_item(r);
                }
                if j.is_natural
                    && let Some(cols) = self.natural.get(&key)
                {
                    j.is_natural = false;
                    j.using_clause = cols.iter().map(|c| string_node(c)).collect();
                    // No common columns: a cross join (`ON TRUE` for an outer one).
                    if cols.is_empty() && j.jointype != pg_query::protobuf::JoinType::JoinInner as i32 {
                        j.quals = Some(Box::new(ast::mk(pg_query::NodeEnum::AConst(
                            pg_query::protobuf::AConst {
                                isnull: false,
                                location: -1,
                                val: Some(pg_query::protobuf::a_const::Val::Boolval(
                                    pg_query::protobuf::Boolean { boolval: true },
                                )),
                            },
                        ))));
                    }
                    self.changed = true;
                }
                if let Some(q) = j.quals.as_deref_mut() {
                    self.expr(q);
                }
            }
            _ => {}
        }
    }

    /// Subqueries inside an expression.
    fn expr(&mut self, n: &mut pg_query::protobuf::Node) {
        use pg_query::NodeEnum as N;
        let Some(e) = n.node.as_mut() else {
            return;
        };
        let each = |v: &mut Vec<pg_query::protobuf::Node>, me: &mut Self| {
            for x in v {
                me.expr(x);
            }
        };
        match e {
            N::SubLink(l) => {
                if let Some(t) = l.testexpr.as_deref_mut() {
                    self.expr(t);
                }
                if let Some(q) = l.subselect.as_deref_mut() {
                    self.stmt(q, false);
                }
            }
            N::AExpr(a) => {
                for x in [a.lexpr.as_deref_mut(), a.rexpr.as_deref_mut()]
                    .into_iter()
                    .flatten()
                {
                    self.expr(x);
                }
            }
            N::BoolExpr(b) => each(&mut b.args, self),
            N::FuncCall(f) => each(&mut f.args, self),
            N::TypeCast(t) => {
                if let Some(a) = t.arg.as_deref_mut() {
                    self.expr(a);
                }
            }
            N::NullTest(t) => {
                if let Some(a) = t.arg.as_deref_mut() {
                    self.expr(a);
                }
            }
            N::BooleanTest(t) => {
                if let Some(a) = t.arg.as_deref_mut() {
                    self.expr(a);
                }
            }
            N::CaseExpr(c) => {
                if let Some(a) = c.arg.as_deref_mut() {
                    self.expr(a);
                }
                each(&mut c.args, self);
                if let Some(d) = c.defresult.as_deref_mut() {
                    self.expr(d);
                }
            }
            N::CaseWhen(w) => {
                for x in [w.expr.as_deref_mut(), w.result.as_deref_mut()]
                    .into_iter()
                    .flatten()
                {
                    self.expr(x);
                }
            }
            N::CoalesceExpr(c) => each(&mut c.args, self),
            N::MinMaxExpr(m) => each(&mut m.args, self),
            N::RowExpr(r) => each(&mut r.args, self),
            N::AArrayExpr(a) => each(&mut a.elements, self),
            N::List(l) => each(&mut l.items, self),
            N::CollateClause(c) => {
                if let Some(a) = c.arg.as_deref_mut() {
                    self.expr(a);
                }
            }
            N::AIndirection(i) => {
                if let Some(a) = i.arg.as_deref_mut() {
                    self.expr(a);
                }
            }
            N::NamedArgExpr(a) => {
                if let Some(x) = a.arg.as_deref_mut() {
                    self.expr(x);
                }
            }
            _ => {}
        }
    }
}

impl Ddl<'_, '_> {
    /// A new view's (or materialized view's) query as stored (`Normalize`); None to keep it as
    /// written. `cols`: its output columns, as bound. The result must bind to the same columns
    /// and print the same.
    pub(super) fn normalized_view_query(
        &self,
        q: &pg_query::protobuf::Node,
        cols: &[crate::sql::plan::OutCol],
        names: &[String],
    ) -> Option<String> {
        let original = ast::deparse_stmt(q).ok()?;
        let cat = Arc::new(self.cat.clone());
        // Relations, NATURAL joins, stars, output names.
        let (mut text, mut changed) = self.normalize_pass(&cat, &original)?;
        // The names Postgres gives USING columns and the columns around them; then the
        // stars they let be written out.
        let mut renamed = false;
        if let Ok(mut node) = ast::parse_stmt(&text) {
            let mut b = recorder(self.ctx, &cat);
            if b.bind_statement(&node).is_ok()
                && let Some(log) = b.refs.take()
                && using_names(&mut node, &log, &cat)
                && let Ok(t) = ast::deparse_stmt(&node)
            {
                text = t;
                changed = true;
                renamed = true;
                if let Some((t, _)) = self.normalize_pass(&cat, &text) {
                    text = t;
                }
            }
        }
        if let Some(q) = self.qualify_names(&cat, &text) {
            text = q;
            changed = true;
        }
        if !changed {
            return None;
        }
        // The same columns …
        let mut b = Binder::new(self.ctx, Vec::new());
        b.cat = cat.clone();
        b.unplanned = true;
        let bound = b.bind_statement(&ast::parse_stmt(&text).ok()?).ok()?;
        if bound.cols.len() != cols.len()
            || bound
                .cols
                .iter()
                .zip(cols)
                .any(|(a, b)| a.name != b.name || a.ty != b.ty)
        {
            return None;
        }
        // … printed the same (with names resolving as the deparser's do), unless names were
        // given (which show).
        if !renamed {
            let ctx = self.ctx.child(self.ctx.params);
            for pretty in [false, true] {
                let def = |query: &str| {
                    crate::sql::ruleutils::query_def(&ctx, query, names.to_vec(), pretty)
                };
                if def(&original) != def(&text) {
                    return None;
                }
            }
        }
        Some(text)
    }

    /// One `Normalize` pass over a view query's text: the text, and whether it changed.
    fn normalize_pass(&self, cat: &Arc<Catalog>, text: &str) -> Option<(String, bool)> {
        let node = ast::parse_stmt(text).ok()?;
        let mut b = recorder(self.ctx, cat);
        b.bind_statement(&node).ok()?;
        let log = b.refs.take()?;
        let natural = std::mem::take(&mut b.natural_using);
        drop(b);
        let mut node = node;
        let mut n = Normalize {
            log: &log,
            natural: &natural,
            cat,
            changed: false,
        };
        n.stmt(&mut node, false);
        if !n.changed {
            return Some((text.to_string(), false));
        }
        Some((ast::deparse_stmt(&node).ok()?, true))
    }
}

impl Ddl<'_, '_> {
    /// A statement's text with the relations, functions, types and reg* literals it names
    /// without a schema qualified (they keep naming the same objects, whatever the search
    /// path, as Postgres's stored oids do). None: nothing to qualify.
    fn qualify_names(&self, cat: &Arc<Catalog>, text: &str) -> Option<String> {
        let node = ast::parse_stmt(text).ok()?;
        let mut b = recorder(self.ctx, cat);
        b.bind_statement(&node).ok()?;
        let log = b.refs.take()?;
        qualify_text(text, &log, cat, self.search_path())
    }

    /// An expression stored over `table`, its names qualified (`qualify_names`); a column's
    /// default or generation expression (`col`: its type and name) as coerced to the
    /// column's type, which reg* literals are read as. None: nothing to qualify (or not
    /// bindable).
    fn qualified_table_expr(
        &self,
        cat: &Arc<Catalog>,
        table: &Arc<crate::catalog::Table>,
        sql: &str,
        clause: Clause,
        col: Option<(&crate::types::DataType, &str)>,
    ) -> Option<String> {
        let node = ast::parse_expr(sql).ok()?;
        // Over the catalog as the statement has it: names (regclass input) of a relation
        // being created resolve.
        let nctx = self.ctx.child_over(cat.clone());
        let mut b = recorder(&nctx, cat);
        let e = b.bind_over_table(table, &node, clause).ok()?;
        if let Some((ty, name)) = col {
            b.coerce_assign(e, ty, name).ok()?;
        }
        let log = b.refs.take()?;
        qualify_text(sql, &expr_log(*log), cat, self.search_path())
    }

    /// Whether two CHECK expressions over `table` are the same (as Postgres compares their
    /// trees): equal texts, or equal once their names are qualified (one stored, so
    /// qualified, the other not yet).
    pub(super) fn same_check(&self, table: &Arc<crate::catalog::Table>, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        let cat = Arc::new(self.cat.clone());
        let q = |s: &str| {
            self.qualified_table_expr(&cat, table, s, Clause::Check, None)
                .unwrap_or_else(|| s.to_string())
        };
        q(a) == q(b)
    }

    /// The expressions this statement stored (those `before` doesn't have) with the
    /// relations, functions, types and reg* literals they name without a schema qualified,
    /// so they keep naming the same objects under any search path, as Postgres's stored
    /// trees (which name objects by oid) do: column defaults and generation expressions,
    /// CHECK constraints, partition keys, index expressions and predicates, statistics
    /// expressions, policies, trigger WHEN conditions and domain CHECKs. (Views, rules and
    /// SQL-standard function bodies are qualified when made.)
    pub(super) fn qualify_new_exprs(&mut self, before: &Catalog) {
        let cat = Arc::new(self.cat.clone());
        let ctx = self.ctx;
        let nctx = ctx.child_over(cat.clone());
        let mut tables = Vec::new();
        for (oid, t) in &cat.tables {
            let old = before.tables.get(oid);
            if old.is_some_and(|o| Arc::ptr_eq(o, t)) {
                continue;
            }
            let mut nt = (**t).clone();
            let mut changed = false;
            for (i, c) in t.columns.iter().enumerate() {
                let prev = old.and_then(|o| o.columns.get(i));
                if let Some(d) = c.default.as_deref()
                    && prev.is_none_or(|p| p.default.as_deref() != Some(d))
                    && let Some(q) = self.qualified_table_expr(&cat, t, d, Clause::Default, Some((&c.ty, &c.name)))
                {
                    nt.columns[i].default = Some(q);
                    changed = true;
                }
                if let Some(g) = c.generated.as_deref()
                    && prev.is_none_or(|p| p.generated.as_deref() != Some(g))
                    && let Some(q) = self.qualified_table_expr(&cat, t, g, Clause::Generated, Some((&c.ty, &c.name)))
                {
                    nt.columns[i].generated = Some(q);
                    changed = true;
                }
            }
            for (i, con) in t.constraints.iter().enumerate() {
                let ConstraintKind::Check { expr, .. } = &con.kind else {
                    continue;
                };
                let known = old.is_some_and(|o| {
                    o.constraints.iter().any(|x| {
                        matches!(&x.kind, ConstraintKind::Check { expr: e, .. } if e == expr)
                    })
                });
                if !known
                    && let Some(q) = self.qualified_table_expr(&cat, t, expr, Clause::Check, None)
                    && let Some(ConstraintKind::Check { expr: cur, .. }) =
                        nt.constraints.get_mut(i).map(|c| &mut c.kind)
                {
                    *cur = q;
                    changed = true;
                }
            }
            if let Some(pk) = &t.partkey {
                let old_pk = old.and_then(|o| o.partkey.as_ref());
                for (i, k) in pk.cols.iter().enumerate() {
                    if let Some(e) = k.expr.as_deref()
                        && old_pk
                            .and_then(|p| p.cols.get(i))
                            .is_none_or(|x| x.expr.as_deref() != Some(e))
                        && let Some(q) = self.qualified_table_expr(&cat, t, e, Clause::IndexExpr, None)
                        && let Some(cur) = nt.partkey.as_mut().and_then(|p| p.cols.get_mut(i))
                    {
                        cur.expr = Some(q);
                        changed = true;
                    }
                }
            }
            if changed {
                tables.push(nt);
            }
        }
        for nt in tables {
            self.cat.put_storage(nt);
        }
        // Index expressions and predicates.
        let mut indexes = Vec::new();
        for (oid, ix) in &cat.indexes {
            let old = before.indexes.get(oid);
            if old.is_some_and(|o| Arc::ptr_eq(o, ix)) {
                continue;
            }
            let Some(t) = cat.index_table(ix.table) else {
                continue;
            };
            let mut nix = (**ix).clone();
            let mut changed = false;
            for (i, c) in ix.columns.iter().enumerate() {
                if let IndexKey::Expr(e) = &c.key
                    && old
                        .and_then(|o| o.columns.get(i))
                        .is_none_or(|x| !matches!(&x.key, IndexKey::Expr(p) if p == e))
                    && let Some(q) = self.qualified_table_expr(&cat, &t, e, Clause::IndexExpr, None)
                    && let Some(IndexKey::Expr(cur)) = nix.columns.get_mut(i).map(|c| &mut c.key)
                {
                    *cur = q;
                    changed = true;
                }
            }
            if let Some(p) = ix.predicate.as_deref()
                && old.is_none_or(|o| o.predicate.as_deref() != Some(p))
                && let Some(q) = self.qualified_table_expr(&cat, &t, p, Clause::Check, None)
            {
                nix.predicate = Some(q);
                changed = true;
            }
            if changed {
                indexes.push(nix);
            }
        }
        for nix in indexes {
            self.cat.put_index(nix);
        }
        // Statistics objects' expressions.
        for (oid, st) in &cat.statistics {
            if before.statistics.get(oid).is_some_and(|o| o.exprs == st.exprs) {
                continue;
            }
            let Some(t) = cat.tables.get(&st.table) else {
                continue;
            };
            let mut exprs = st.exprs.clone();
            let mut changed = false;
            for e in &mut exprs {
                if let Some(q) = self.qualified_table_expr(&cat, t, e, Clause::IndexExpr, None) {
                    *e = q;
                    changed = true;
                }
            }
            if changed {
                let mut ns = (**st).clone();
                ns.exprs = exprs;
                self.cat.statistics.insert(ns.oid, Arc::new(ns));
            }
        }
        // Policies.
        for (oid, p) in &cat.policies {
            let old = before.policies.get(oid);
            if old.is_some_and(|o| o.qual == p.qual && o.with_check == p.with_check) {
                continue;
            }
            let Some(t) = cat.tables.get(&p.table) else {
                continue;
            };
            let q = |s: &Option<String>, prev: Option<&Option<String>>| {
                s.as_deref()
                    .filter(|_| prev != Some(s))
                    .and_then(|s| self.qualified_table_expr(&cat, t, s, Clause::Where, None))
            };
            let (qual, check) = (
                q(&p.qual, old.map(|o| &o.qual)),
                q(&p.with_check, old.map(|o| &o.with_check)),
            );
            if qual.is_some() || check.is_some() {
                let mut np = (**p).clone();
                if qual.is_some() {
                    np.qual = qual;
                }
                if check.is_some() {
                    np.with_check = check;
                }
                self.cat.policies.insert(np.oid, Arc::new(np));
            }
        }
        // Trigger WHEN conditions.
        for (oid, tg) in &cat.triggers {
            let Some(w) = tg.when.as_deref() else {
                continue;
            };
            if before.triggers.get(oid).is_some_and(|o| o.when.as_deref() == Some(w)) {
                continue;
            }
            let table = (cat.tables.get(&tg.table).cloned())
                .or_else(|| cat.views.get(&tg.table).map(|v| v.as_table()));
            let Some(table) = table else {
                continue;
            };
            let mut b = recorder(&nctx, &cat);
            if b.bind_trigger_when(&table, w).is_err() {
                continue;
            }
            let Some(log) = b.refs.take() else {
                continue;
            };
            if let Some(q) = qualify_text(w, &expr_log(*log), &cat, self.search_path()) {
                let mut nt = (**tg).clone();
                nt.when = Some(q);
                self.cat.triggers.insert(nt.oid, Arc::new(nt));
            }
        }
        // Domain CHECKs.
        for (oid, d) in &cat.domains {
            let old = before.domains.get(oid);
            if old.is_some_and(|o| o.checks == d.checks) {
                continue;
            }
            let mut checks = d.checks.clone();
            let mut changed = false;
            for (name, sql) in &mut checks {
                if old.is_some_and(|o| o.checks.iter().any(|(n, s)| n == name && s == sql)) {
                    continue;
                }
                let Ok(node) = ast::parse_expr(sql) else {
                    continue;
                };
                let mut b = recorder(&nctx, &cat);
                if b.bind_domain_check(&node, &d.base).is_err() {
                    continue;
                }
                let Some(log) = b.refs.take() else {
                    continue;
                };
                if let Some(q) = qualify_text(sql, &expr_log(*log), &cat, self.search_path()) {
                    *sql = q;
                    changed = true;
                }
            }
            if changed {
                let mut nd = (**d).clone();
                nd.checks = checks;
                self.cat.domains.insert(nd.oid, Arc::new(nd));
            }
        }
    }

    /// A new SQL-standard function's body, its statements' names qualified
    /// (`qualify_names`); as it was when that can't be done.
    pub(super) fn qualified_body(&self, f: crate::catalog::Function) -> crate::catalog::Function {
        if f.sql_body.is_none() {
            return f;
        }
        let Ok(stmts) = crate::sql::parse::parse(&f.body) else {
            return f;
        };
        let cat = Arc::new(self.cat.clone());
        let args: Vec<Expr> = (f.args.iter())
            .map(|(_, t)| Expr::Const(Value::Null, t.clone()))
            .collect();
        let mut edits = Vec::new();
        for raw in &stmts {
            let start = (raw.stmt_location.max(0) as usize).min(f.body.len());
            let lead = f.body[start..].len() - f.body[start..].trim_start().len();
            let text = crate::sql::parse::stmt_text(&f.body, raw).trim_end_matches(';');
            let one = crate::catalog::Function {
                body: text.to_string(),
                config: Vec::new(),
                ..f.clone()
            };
            let mut b = recorder(self.ctx, &cat);
            if b.bind_user_body(&one, &args).is_err() {
                return f;
            }
            let Some(log) = b.refs.take() else {
                return f;
            };
            if let Some(q) = qualify_text(text, &log, &cat, self.search_path()) {
                edits.push(Edit {
                    start: start + lead,
                    end: start + lead + text.len(),
                    text: q,
                });
            }
        }
        if edits.is_empty() {
            return f;
        }
        let body = apply(&f.body, edits);
        crate::catalog::Function { body, ..f }
    }

    /// A new rule's condition and actions, their names qualified (`qualify_names`).
    pub(super) fn qualified_rule(
        &self,
        table: &Arc<crate::catalog::Table>,
        event: char,
        instead: bool,
        qual: Option<String>,
        actions: Vec<String>,
    ) -> (Option<String>, Vec<String>) {
        let cat = Arc::new(self.cat.clone());
        let qual = qual.map(|q| {
            let Ok(node) = ast::parse_expr(&q) else {
                return q;
            };
            let mut b = recorder(self.ctx, &cat);
            if b.check_rule(table, event, Some(&node), &[], instead).is_err() {
                return q;
            }
            let Some(mut log) = b.refs.take() else {
                return q;
            };
            // (Positions in `SELECT <expr>`.)
            for r in &mut log.rels {
                r.loc -= EXPR_BASE;
            }
            for x in log.funcs.iter_mut().chain(log.types.iter_mut()) {
                x.0 -= EXPR_BASE;
            }
            for x in &mut log.literals {
                x.0 -= EXPR_BASE;
            }
            qualify_text(&q, &log, &cat, self.search_path()).unwrap_or(q)
        });
        let actions = actions
            .into_iter()
            .map(|a| {
                let Ok(node) = ast::parse_stmt(&a) else {
                    return a;
                };
                let mut b = recorder(self.ctx, &cat);
                if b
                    .check_rule(table, event, None, std::slice::from_ref(&node), instead)
                    .is_err()
                {
                    return a;
                }
                let Some(log) = b.refs.take() else {
                    return a;
                };
                qualify_text(&a, &log, &cat, self.search_path()).unwrap_or(a)
            })
            .collect();
        (qual, actions)
    }

    /// ALTER COLUMN TYPE: what depends on the column and can't follow its new type
    /// (RememberAllDependentForRebuilding) is an error, the first found in pg_depend's order.
    pub(super) fn column_type_dependents(&self, table: u32, idx: usize, colname: &str) -> Result<()> {
        use crate::catalog::depend as d;
        let cat = Arc::new(self.cat.clone());
        let column = (d::REL, table, idx as i32 + 1);
        let mut objs: Vec<d::Addr> = d::depends(self.ctx, &cat)
            .into_iter()
            .filter(|x| x.refobj == column)
            .map(|x| x.obj)
            .collect();
        objs.sort_by_key(|o| o.1);
        objs.dedup();
        for o in objs {
            let msg = match o.0 {
                d::REWRITE => "cannot alter type of a column used by a view or rule",
                d::TRIGGER => "cannot alter type of a column used in a trigger definition",
                d::POLICY => "cannot alter type of a column used in a policy definition",
                d::PROC => "cannot alter type of a column used by a function or procedure",
                _ => continue,
            };
            let what = d::describe(&cat, self.search_path(), o).unwrap_or_default();
            return Err(crate::error::Error::new(crate::error::code::FEATURE_NOT_SUPPORTED, msg)
                .detail(format!("{what} depends on column \"{colname}\"")));
        }
        Ok(())
    }
}

// ── USING names (set_using_names, set_relation_column_names) ──

/// `make_colname_unique`: `name`, else `name_1`, `name_2`, … not taken.
fn unique_name(name: &str, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    (1..)
        .map(|i| crate::catalog::suffixed_name(name, &format!("_{i}")))
        .find(|n| !taken(n))
        .unwrap_or_default()
}

/// The names Postgres gives a query level's USING columns and its relations' columns when
/// it prints them, made part of the query: a USING column's name goes down to the columns
/// it merges, and (when a FULL JOIN's or a coerced USING column can't be read as an input's
/// column, `has_dangerous_join_using`) USING names are unique across the level and no
/// other column takes one — `t t(x_1, z)`, `USING (x_1)`. Relations' columns get the names
/// through the column aliases the engine adds. `cat`: where the relations' columns are
/// read. Whether anything changed.
fn using_names(node: &mut pg_query::protobuf::Node, log: &RefLog, cat: &Catalog) -> bool {
    use pg_query::NodeEnum as N;
    use std::collections::HashMap;
    let Some(N::SelectStmt(s)) = node.node.as_mut() else {
        return false;
    };
    let mut changed = false;
    // Other levels.
    if let Some(w) = s.with_clause.as_mut() {
        for c in &mut w.ctes {
            if let Some(N::CommonTableExpr(cte)) = c.node.as_mut()
                && let Some(q) = cte.ctequery.as_deref_mut()
            {
                changed |= using_names(q, log, cat);
            }
        }
    }
    for x in [s.larg.as_deref_mut(), s.rarg.as_deref_mut()].into_iter().flatten() {
        let mut n = ast::mk(N::SelectStmt(Box::new(std::mem::take(x))));
        changed |= using_names(&mut n, log, cat);
        if let Some(N::SelectStmt(b)) = n.node {
            *x = *b;
        }
    }
    fn subqueries(n: &mut pg_query::protobuf::Node, log: &RefLog, cat: &Catalog) -> bool {
        match n.node.as_mut() {
            Some(N::RangeSubselect(rs)) => rs
                .subquery
                .as_deref_mut()
                .is_some_and(|q| using_names(q, log, cat)),
            Some(N::JoinExpr(j)) => {
                let mut c = false;
                for x in [j.larg.as_deref_mut(), j.rarg.as_deref_mut()].into_iter().flatten() {
                    c |= subqueries(x, log, cat);
                }
                c
            }
            _ => false,
        }
    }
    for f in &mut s.from_clause {
        changed |= subqueries(f, log, cat);
    }

    /// The relation names under a FROM item.
    fn locs(n: &pg_query::protobuf::Node) -> Vec<i32> {
        match n.node.as_ref() {
            Some(N::RangeVar(rv)) => vec![rv.location],
            Some(N::JoinExpr(j)) => [j.larg.as_deref(), j.rarg.as_deref()]
                .into_iter()
                .flatten()
                .flat_map(locs)
                .collect(),
            _ => Vec::new(),
        }
    }
    // This level: is a USING column dangerous anywhere in it?
    fn dangerous(n: &pg_query::protobuf::Node, log: &RefLog) -> bool {
        let Some(N::JoinExpr(j)) = n.node.as_ref() else {
            return false;
        };
        join_rec(log, &locs(n)).is_some_and(|(_, r)| r.dangerous && !r.usings.is_empty())
            || [j.larg.as_deref(), j.rarg.as_deref()]
                .into_iter()
                .flatten()
                .any(|x| dangerous(x, log))
    }
    let unique_using = s.from_clause.iter().any(|f| dangerous(f, log));
    type Side = (Option<i32>, i16);
    struct St {
        using_names: Vec<String>,
        pushed: HashMap<Side, String>,
        parent_using: HashMap<i32, Vec<String>>,
        unique_using: bool,
        changed: bool,
    }
    fn visit(n: &mut pg_query::protobuf::Node, log: &RefLog, parent: &[String], st: &mut St) {
        let here = locs(n);
        match n.node.as_mut() {
            Some(N::RangeVar(rv)) => {
                st.parent_using.insert(rv.location, parent.to_vec());
            }
            Some(N::JoinExpr(j)) => {
                let mut parent = parent.to_vec();
                if let Some((index, rec)) = join_rec(log, &here).filter(|(_, r)| !r.usings.is_empty()) {
                    let written: Vec<String> =
                        j.alias.as_ref().map(|a| ast::names(&a.colnames)).unwrap_or_default();
                    let mut names: Vec<String> = Vec::new();
                    for (i, (name, l, r)) in rec.usings.iter().enumerate() {
                        let mine = (Some(crate::sql::binder::refs::merged_src(index, i)), 0);
                        let colname = match st.pushed.get(&mine) {
                            Some(p) => p.clone(),
                            None => {
                                let base = written.get(i).unwrap_or(name);
                                let n = unique_name(base, &|c| {
                                    names.iter().any(|x| x == c)
                                        || st.unique_using && st.using_names.iter().any(|x| x == c)
                                        || parent.iter().any(|x| x == c)
                                });
                                if st.unique_using {
                                    st.using_names.push(n.clone());
                                }
                                n
                            }
                        };
                        // A merged column below this one is keyed by its join and index.
                        for side in [l, r] {
                            let k = match side.0 {
                                Some(s) if s < -1 => (Some(s), 0),
                                _ => *side,
                            };
                            st.pushed.insert(k, colname.clone());
                        }
                        names.push(colname);
                    }
                    let had = ast::names(&j.using_clause);
                    if names != had && had.len() == names.len() {
                        j.using_clause = names.iter().map(|n| string_node(n)).collect();
                        st.changed = true;
                    }
                    parent.extend(names);
                }
                for x in [j.larg.as_deref_mut(), j.rarg.as_deref_mut()].into_iter().flatten() {
                    visit(x, log, &parent, st);
                }
            }
            _ => {}
        }
    }
    let mut st = St {
        using_names: Vec::new(),
        pushed: HashMap::new(),
        parent_using: HashMap::new(),
        unique_using,
        changed: false,
    };
    for f in &mut s.from_clause {
        visit(f, log, &[], &mut st);
    }
    // Each relation's columns.
    fn rels(n: &mut pg_query::protobuf::Node, out: &mut Vec<*mut pg_query::protobuf::RangeVar>) {
        match n.node.as_mut() {
            Some(N::RangeVar(rv)) => out.push(rv as *mut _),
            Some(N::JoinExpr(j)) => {
                for x in [j.larg.as_deref_mut(), j.rarg.as_deref_mut()].into_iter().flatten() {
                    rels(x, out);
                }
            }
            _ => {}
        }
    }
    let mut all = Vec::new();
    for f in &mut s.from_clause {
        rels(f, &mut all);
    }
    for p in all {
        // SAFETY: the pointers are to distinct RangeVars of `s`, not otherwise borrowed now.
        let rv = unsafe { &mut *p };
        let Some(r) = log.rels.iter().find(|r| r.loc == rv.location && r.how == RelUse::From) else {
            continue;
        };
        let Some(real) = live_columns(cat, r.oid) else {
            continue;
        };
        let (written, implicit_cols) = match rv.alias.as_ref() {
            Some(a) => (ast::names(&a.colnames), ast::alias_name(a).implicit_cols),
            None => (Vec::new(), false),
        };
        let attnums: Vec<i16> = match cat.tables.get(&r.oid) {
            Some(t) => t.live_columns().map(|(i, _)| i as i16 + 1).collect(),
            None => (1..=real.len() as i16).collect(),
        };
        let parent = st.parent_using.get(&rv.location).cloned().unwrap_or_default();
        // Names given first (pushed down from USING, or kept from before), then the rest.
        let mut names: Vec<Option<String>> = attnums
            .iter()
            .enumerate()
            .map(|(i, a)| {
                st.pushed
                    .get(&(Some(rv.location), *a))
                    .cloned()
                    .or_else(|| implicit_cols.then(|| written.get(i).cloned()).flatten())
            })
            .collect();
        for i in 0..names.len() {
            if names[i].is_some() {
                continue;
            }
            let base = if implicit_cols {
                real[i].clone()
            } else {
                written.get(i).cloned().unwrap_or_else(|| real[i].clone())
            };
            let n = unique_name(&base, &|c| {
                names.iter().flatten().any(|x| x == c)
                    || st.unique_using && st.using_names.iter().any(|x| x == c)
                    || parent.iter().any(|x| x == c)
            });
            names[i] = Some(n);
        }
        let names: Vec<String> = names.into_iter().flatten().collect();
        let shown: Vec<String> = if implicit_cols {
            written.clone()
        } else {
            (0..real.len())
                .map(|i| written.get(i).cloned().unwrap_or_else(|| real[i].clone()))
                .collect()
        };
        if names == shown || names.len() != real.len() {
            continue;
        }
        let aliasname = match rv.alias.as_ref().map(ast::alias_name) {
            Some(a) if !a.written => rv.alias.as_ref().map(|x| x.aliasname.clone()),
            Some(a) => Some(format!("{}{}{}", ast::IMPLICIT, ast::IMPLICIT, a.name)),
            None => rel_names(cat, r.oid).map(|(schema, name, _)| {
                format!("{}{}{}{}", ast::IMPLICIT, schema, ast::IMPLICIT, name)
            }),
        };
        let Some(aliasname) = aliasname else {
            continue;
        };
        rv.alias = Some(pg_query::protobuf::Alias {
            aliasname,
            colnames: names.iter().map(|n| string_node(n)).collect(),
        });
        st.changed = true;
    }
    changed || st.changed
}
