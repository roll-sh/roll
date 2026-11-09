//! DESCRIBE.
//!
//! Cassandra's cqlsh (6.2) sends DESCRIBE to the server and lays out the rows
//! (`do_describe`, `describe_list`, `describe_element`, `describe_cluster` in Apache Cassandra's
//! `cqlshmain.py`, Apache-2.0). The cqlsh in the ScyllaDB image does the same when it runs a
//! single `-e` statement; otherwise it describes from the driver's schema metadata, as cqlsh
//! did before Cassandra 4.0: keyspaces in `system_schema` scan order, `<empty>` for keyspaces
//! without objects, client-side error messages, and CREATE statements generated from the schema
//! tables ([`super::export`]).

use bytes::Bytes;
use nosql_proto::{Consistency, QueryParams, ResultBody, Rows};

use super::format::Flavor;
use super::lex::{self, Token};
use super::render::columnize;
use super::session::{Failure, cell};
use super::shell::{P, Shell};

/// A parsed DESCRIBE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    Functions,
    Function(Option<String>, String),
    Aggregates,
    Aggregate(Option<String>, String),
    Keyspaces,
    Keyspace(Option<String>),
    Table(Option<String>, String),
    Index(Option<String>, String),
    View(Option<String>, String),
    Tables,
    Schema { full: bool },
    Cluster,
    Types,
    Type(Option<String>, String),
    Object(Option<String>, String),
}

fn qual(p: &mut P) -> Option<(Option<String>, String)> {
    p.qualified().map(|(k, n)| (k.map(lex::unprotect), lex::unprotect(n)))
}

/// `ONLY? KEYSPACE`.
fn keyspace_word(p: &mut P) -> bool {
    let save = p.i;
    p.word("only");
    if p.word("keyspace") {
        return true;
    }
    p.i = save;
    false
}

/// Parses the words after DESCRIBE; `None` when cqlsh would say "Improper … command".
pub fn parse(src: &str, toks: &[Token]) -> Option<What> {
    let mut p = P::new(src, toks);
    p.i = 1;
    let what = if p.word("functions") {
        What::Functions
    } else if p.word("function") {
        let (k, n) = qual(&mut p)?;
        What::Function(k, n)
    } else if p.word("aggregates") {
        What::Aggregates
    } else if p.word("aggregate") {
        let (k, n) = qual(&mut p)?;
        What::Aggregate(k, n)
    } else if p.word("keyspaces") {
        What::Keyspaces
    } else if keyspace_word(&mut p) {
        What::Keyspace(p.name().map(lex::unprotect))
    } else if p.word("columnfamily") || p.word("table") {
        let (k, n) = qual(&mut p)?;
        What::Table(k, n)
    } else if p.word("index") {
        let (k, n) = qual(&mut p)?;
        What::Index(k, n)
    } else if p.word("materialized") {
        if !p.word("view") {
            return None;
        }
        let (k, n) = qual(&mut p)?;
        What::View(k, n)
    } else if p.word("columnfamilies") || p.word("tables") {
        What::Tables
    } else if p.word("full") {
        if !p.word("schema") {
            return None;
        }
        What::Schema { full: true }
    } else if p.word("schema") {
        What::Schema { full: false }
    } else if p.word("cluster") {
        What::Cluster
    } else if p.word("types") {
        What::Types
    } else if p.word("type") {
        let (k, n) = qual(&mut p)?;
        What::Type(k, n)
    } else {
        let (k, n) = qual(&mut p)?;
        What::Object(k, n)
    };
    if p.word("with") && !p.word("internals") {
        return None;
    }
    p.done().then_some(what)
}

pub async fn do_describe(shell: &mut Shell, src: &str, toks: &[Token], cmdword: &str) {
    let Some(what) = parse(src, toks) else {
        return shell.printerr(&format!("Improper {cmdword} command."));
    };
    if shell.flavor == Flavor::Scylla && shell.client_describe {
        return client_side(shell, what, src, toks).await;
    }
    let stmt = lex::extract(src, toks).to_string();
    let rows = match fetch(shell, &stmt).await {
        Ok(r) => r,
        Err(f) => return shell.printerr(&f.describe_line()),
    };
    let (rows, warnings) = rows;
    let Some(rows) = rows else { return };
    let second = toks.get(1).map(|t| t.text(src).to_lowercase()).unwrap_or_default();
    match second.as_str() {
        "columnfamilies" | "tables" | "types" | "functions" | "aggregates" => describe_list(shell, &rows),
        "keyspaces" => {
            let names: Vec<String> = all(&rows, "name");
            shell.print("");
            shell.print_raw(&columnize(&names, 80));
            shell.print("");
        }
        "cluster" => describe_cluster(shell, &rows),
        _ => describe_element(shell, &rows),
    }
    if !warnings.is_empty() {
        shell.print_warnings(&warnings);
    }
}

/// Every page of a DESCRIBE (LOCAL_ONE, cqlsh's page size).
async fn fetch(shell: &Shell, stmt: &str) -> Result<(Option<Rows>, Vec<String>), Failure> {
    let mut params = QueryParams::new(Consistency::LocalOne);
    if shell.paging {
        params.page_size = Some(shell.page_size);
    }
    let mut acc: Option<Rows> = None;
    let mut warnings = vec![];
    loop {
        let r = shell.conn.query(stmt, params.clone(), false).await?;
        warnings = r.warnings;
        let ResultBody::Rows(rows) = r.body else { return Ok((acc, warnings)) };
        let next = rows.metadata.paging_state.clone();
        acc = Some(match acc {
            None => rows,
            Some(mut a) => {
                let mut c = a.content.to_vec();
                c.extend_from_slice(&rows.content);
                a.content = Bytes::from(c);
                a.row_count += rows.row_count;
                a
            }
        });
        match next {
            Some(s) => params.paging_state = Some(s),
            None => return Ok((acc, warnings)),
        }
    }
}

fn str_cell(rows: &Rows, row: &[Option<Bytes>], name: &str) -> String {
    cell(rows, row, name).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_else(|| "None".into())
}

fn all(rows: &Rows, name: &str) -> Vec<String> {
    rows.to_vec().iter().map(|r| str_cell(rows, r, name)).collect()
}

fn print_names(shell: &mut Shell, keyspace: &str, names: &[String]) {
    shell.print("");
    if shell.keyspace.is_none() {
        shell.print(&format!("Keyspace {keyspace}"));
        shell.print(&format!("---------{}", "-".repeat(keyspace.chars().count())));
    }
    shell.print_raw(&columnize(names, 80));
}

/// `describe_list`.
fn describe_list(shell: &mut Shell, rows: &Rows) {
    let mut ks: Option<String> = None;
    let mut names = vec![];
    for r in rows.to_vec() {
        let k = str_cell(rows, &r, "keyspace_name");
        if ks.as_deref() != Some(&k) {
            if let Some(prev) = &ks {
                print_names(shell, prev, &names);
            }
            ks = Some(k);
            names.clear();
        }
        names.push(str_cell(rows, &r, "name"));
    }
    if let Some(prev) = ks {
        print_names(shell, &prev, &names);
        shell.print("");
    }
}

/// `describe_element`: each CREATE statement between blank lines.
fn describe_element(shell: &mut Shell, rows: &Rows) {
    for r in rows.to_vec() {
        shell.print("");
        shell.flush();
        let stmt = str_cell(rows, &r, "create_statement");
        shell.write_query(&stmt);
        shell.print("");
    }
}

fn map_of_lists(b: &[u8]) -> Vec<(String, Vec<String>)> {
    let Ok(entries) = nosql_types::map_entries(b) else { return vec![] };
    entries
        .iter()
        .map(|(k, v)| {
            let hosts = nosql_types::collection_elements(v)
                .unwrap_or_default()
                .iter()
                .map(|h| h.map(|h| String::from_utf8_lossy(h).into_owned()).unwrap_or_default())
                .collect();
            (String::from_utf8_lossy(k).into_owned(), hosts)
        })
        .collect()
}

/// `describe_cluster`.
fn describe_cluster(shell: &mut Shell, rows: &Rows) {
    for r in rows.to_vec() {
        shell.print(&format!("\nCluster: {}", str_cell(rows, &r, "cluster")));
        shell.print(&format!("Partitioner: {}", str_cell(rows, &r, "partitioner")));
        shell.print(&format!("Snitch: {}\n", str_cell(rows, &r, "snitch")));
        if rows.metadata.columns.iter().any(|c| c.name == "range_ownership") {
            shell.print("Range ownership:");
            for (token, hosts) in cell(rows, &r, "range_ownership").map(|b| map_of_lists(b)).unwrap_or_default() {
                shell.print(&format!(" {token:>39}  [{}]", hosts.join(", ")));
            }
            shell.print("");
        }
    }
}

// --- ScyllaDB's cqlsh: the driver's metadata -------------------------------------------------

/// Keyspace names in `system_schema.keyspaces` scan order (the driver's metadata order).
pub async fn keyspace_names(shell: &Shell) -> Vec<String> {
    shell
        .conn
        .strings("SELECT keyspace_name FROM system_schema.keyspaces")
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| r.into_iter().next().flatten())
        .collect()
}

/// `(keyspace, name)` rows of a schema table, in scan order.
async fn objects(shell: &Shell, table: &str, column: &str) -> Vec<(String, String)> {
    shell
        .conn
        .strings(&format!("SELECT keyspace_name, {column} FROM system_schema.{table}"))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let mut it = r.into_iter();
            Some((it.next().flatten()?, it.next().flatten()?))
        })
        .collect()
}

/// Function signatures as the driver keys them: `name(type,type)`.
async fn signatures(shell: &Shell, table: &str, column: &str) -> Vec<(String, String)> {
    let q = format!("SELECT keyspace_name, {column}, argument_types FROM system_schema.{table}");
    let Ok(Some(rows)) = shell.conn.all_rows(&q, Consistency::One).await else { return vec![] };
    rows.to_vec()
        .iter()
        .map(|r| {
            let args: Vec<String> = cell(&rows, r, "argument_types")
                .and_then(|b| nosql_types::collection_elements(b).ok())
                .unwrap_or_default()
                .iter()
                .map(|a| a.map(|a| String::from_utf8_lossy(a).into_owned()).unwrap_or_default())
                .collect();
            (str_cell(&rows, r, "keyspace_name"), format!("{}({})", str_cell(&rows, r, column), args.join(",")))
        })
        .collect()
}

fn columnized_by_keyspace(shell: &mut Shell, keyspaces: &[String], items: &[(String, String)], current: Option<&str>, quote: bool) {
    let names_in = |ks: &str| -> Vec<String> {
        items.iter().filter(|(k, _)| k == ks).map(|(_, n)| if quote { lex::protect_name(n) } else { n.clone() }).collect()
    };
    shell.print("");
    match current {
        None => {
            for ks in keyspaces {
                let name = lex::protect_name(ks);
                shell.print(&format!("Keyspace {name}"));
                shell.print(&format!("---------{}", "-".repeat(name.chars().count())));
                shell.print_raw(&columnize(&names_in(ks), 80));
                shell.print("");
            }
        }
        Some(ks) => {
            shell.print_raw(&columnize(&names_in(ks), 80));
            shell.print("");
        }
    }
}

/// Orders `(keyspace, name)` items as the driver's metadata holds them.
fn in_meta_order(shell: &Shell, items: Vec<(String, String)>, keyspaces: &[String]) -> Vec<(String, String)> {
    let mut out = vec![];
    for ks in keyspaces {
        let names: Vec<String> = items.iter().filter(|(k, _)| k == ks).map(|(_, n)| n.clone()).collect();
        out.extend(shell.meta.tables(ks, names).into_iter().map(|n| (ks.clone(), n)));
    }
    out
}

async fn client_side(shell: &mut Shell, what: What, src: &str, toks: &[Token]) {
    let keyspaces = shell.meta.keyspaces(keyspace_names(shell).await);
    let current = shell.keyspace.clone();
    let has_ks = |k: &str| keyspaces.iter().any(|x| x == k);
    match what {
        What::Keyspaces => {
            let names: Vec<String> = keyspaces.iter().map(|k| lex::protect_name(k)).collect();
            shell.print("");
            shell.print_raw(&columnize(&names, 80));
            shell.print("");
        }
        What::Tables => {
            let items = objects(shell, "tables", "table_name").await;
            let items = in_meta_order(shell, items, &keyspaces);
            columnized_by_keyspace(shell, &keyspaces, &items, current.as_deref(), true);
        }
        What::Types => {
            let items = objects(shell, "types", "type_name").await;
            columnized_by_keyspace(shell, &keyspaces, &items, current.as_deref(), true);
        }
        What::Functions => {
            let items = signatures(shell, "functions", "function_name").await;
            columnized_by_keyspace(shell, &keyspaces, &items, current.as_deref(), false);
        }
        What::Aggregates => {
            let items = signatures(shell, "aggregates", "aggregate_name").await;
            columnized_by_keyspace(shell, &keyspaces, &items, current.as_deref(), false);
        }
        What::Cluster => {
            shell.print(&format!("\nCluster: {}", shell.versions.cluster_name));
            let part = shell
                .conn
                .strings("SELECT partitioner FROM system.local WHERE key = 'local'")
                .await
                .ok()
                .and_then(|r| r.into_iter().next())
                .and_then(|r| r.into_iter().next().flatten())
                .unwrap_or_default();
            let part = part.strip_prefix("org.apache.cassandra.dht.").unwrap_or(&part).to_string();
            shell.print(&format!("Partitioner: {part}\n"));
            if let Some(ks) = current.as_deref()
                && ks != "system"
            {
                shell.print("Range ownership:");
                let host = shell.rpc_address().await;
                for t in super::export::local_tokens(shell).await {
                    shell.print(&format!(" {t:>39}  [{host}]"));
                }
                shell.print("");
            }
        }
        What::Keyspace(ks) => {
            let Some(ks) = ks.or(current.clone()) else { return shell.printerr("Not in any keyspace.") };
            shell.print("");
            if !has_ks(&ks) {
                return shell.printerr(&format!("Keyspace {} not found.", super::session::py_repr(&ks)));
            }
            let out = super::export::keyspace(shell, &ks, true).await;
            shell.print_raw(&out);
            shell.print("");
        }
        What::Schema { full } => {
            shell.print("");
            for ks in &keyspaces {
                if full || !super::export::is_system(ks) {
                    let out = super::export::keyspace(shell, ks, true).await;
                    shell.print_raw(&out);
                    shell.print("");
                }
            }
        }
        What::Table(k, name) => {
            let Some(ks) = k.or(current.clone()) else { return shell.printerr("No keyspace specified and no current keyspace") };
            shell.print("");
            if !has_ks(&ks) {
                return shell.printerr(&format!("Keyspace {} not found.", super::session::py_repr(&ks)));
            }
            match super::export::table(shell, &ks, &name, true).await {
                Some(out) => {
                    shell.print_raw(&out);
                    shell.print("");
                }
                None => shell.printerr(&format!("Column family {name} not found")),
            }
        }
        What::Index(k, name) => {
            let Some(ks) = k.or(current.clone()) else { return shell.printerr("Keyspace None not found.") };
            shell.print("");
            match super::export::index(shell, &ks, &name).await {
                Some(out) => {
                    shell.print_raw(&out);
                    shell.print("");
                }
                None => shell.printerr(&format!("Index {name} not found")),
            }
        }
        What::View(k, name) => {
            let Some(ks) = k.or(current.clone()) else { return shell.printerr("No keyspace specified and no current keyspace") };
            shell.print("");
            match super::export::view(shell, &ks, &name, true).await {
                Some(out) => {
                    shell.print_raw(&out);
                    shell.print("");
                }
                None => shell.printerr(&format!("Materialized view '{name}' not found")),
            }
        }
        What::Type(k, name) => {
            let Some(ks) = k.or(current.clone()) else { return shell.printerr("No keyspace specified and no current keyspace") };
            shell.print("");
            match super::export::user_type(shell, &ks, &name).await {
                Some(out) => {
                    shell.print_raw(&out);
                    shell.print("");
                }
                None => shell.printerr(&format!("User type {name} not found")),
            }
        }
        What::Function(k, name) | What::Aggregate(k, name) => {
            let agg = toks.get(1).is_some_and(|t| t.text(src).eq_ignore_ascii_case("aggregate"));
            let Some(ks) = k.or(current.clone()) else { return shell.printerr("No keyspace specified and no current keyspace") };
            shell.print("");
            let stmt = format!("DESCRIBE {} {}.{}", if agg { "AGGREGATE" } else { "FUNCTION" }, lex::protect_name(&ks), lex::protect_name(&name));
            match fetch(shell, &stmt).await {
                Ok((Some(rows), _)) if !rows.is_empty() => {
                    for r in rows.to_vec() {
                        shell.print(&str_cell(&rows, &r, "create_statement"));
                    }
                    shell.print("");
                }
                _ => {
                    let kind = if agg { "aggregate" } else { "function" };
                    shell.printerr(&format!("User defined {kind} {name} not found"));
                }
            }
        }
        What::Object(k, name) => {
            shell.print("");
            let (ks, obj) = match k {
                Some(ks) => (ks, Some(name)),
                None if has_ks(&name) => (name, None),
                None => match current.clone() {
                    Some(c) => (c, Some(name)),
                    None => return shell.printerr(&format!("'{name}' not found in keyspaces")),
                },
            };
            let Some(obj) = obj else {
                let out = super::export::keyspace(shell, &ks, true).await;
                shell.print_raw(&out);
                shell.print("");
                return;
            };
            let out = match super::export::table(shell, &ks, &obj, true).await {
                Some(o) => Some(o),
                None => match super::export::index(shell, &ks, &obj).await {
                    Some(o) => Some(o),
                    None => super::export::view(shell, &ks, &obj, true).await,
                },
            };
            match out {
                Some(o) => {
                    shell.print_raw(&o);
                    shell.print("");
                }
                None => shell.printerr(&format!("'{obj}' not found in keyspace '{ks}'")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<What> {
        let toks = lex::lex(s).unwrap();
        parse(s, &toks)
    }

    #[test]
    fn parses_the_cqlsh_grammar() {
        assert_eq!(p("DESC KEYSPACES"), Some(What::Keyspaces));
        assert_eq!(p("DESC KEYSPACE"), Some(What::Keyspace(None)));
        assert_eq!(p("describe only keyspace \"Ks\";"), Some(What::Keyspace(Some("Ks".into()))));
        assert_eq!(p("DESC TABLE ks.T"), Some(What::Table(Some("ks".into()), "t".into())));
        assert_eq!(p("DESC t"), Some(What::Object(None, "t".into())));
        assert_eq!(p("DESC FULL SCHEMA"), Some(What::Schema { full: true }));
        assert_eq!(p("DESC MATERIALIZED VIEW ks.mv"), Some(What::View(Some("ks".into()), "mv".into())));
        assert_eq!(p("DESC TYPE addr WITH INTERNALS"), Some(What::Type(None, "addr".into())));
        assert_eq!(p("DESC"), None);
        assert_eq!(p("DESC foo bar"), None);
        assert_eq!(p("DESC TABLE"), None);
        assert_eq!(p("DESC FULL"), None);
    }
}
