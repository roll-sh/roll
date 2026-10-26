//! IMPORT FOREIGN SCHEMA (postgresImportForeignSchema): the remote schema's tables as
//! CREATE FOREIGN TABLE commands.

use super::connection;
use super::deparse::string_literal;
use crate::catalog::quote_ident;
use crate::catalog::userobj::ForeignServer;
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::remote::conn::Status;

/// What IMPORT FOREIGN SCHEMA asked for.
pub struct Import<'a> {
    pub remote_schema: &'a str,
    /// LIMIT TO (Some(true)) / EXCEPT (Some(false)) these tables.
    pub list: Option<(bool, Vec<String>)>,
    pub options: Vec<(String, String)>,
}

/// The commands, one per remote table: (table name, CREATE FOREIGN TABLE text).
pub fn commands(ctx: &ExecCtx, srv: &ForeignServer, imp: &Import) -> Result<Vec<(String, String)>> {
    let (mut collate, mut default, mut generated, mut not_null) = (true, false, true, true);
    for (k, v) in &imp.options {
        let b = || crate::ext::dblink::def_boolean(k, v);
        match k.as_str() {
            "import_collate" => collate = b()?,
            "import_default" => default = b()?,
            "import_generated" => generated = b()?,
            "import_not_null" => not_null = b()?,
            _ => return Err(Error::new(code::FDW_INVALID_OPTION_NAME, format!("invalid option \"{k}\""))),
        }
    }
    let m = connection::user_mapping(&ctx.catalog(), ctx.current_user(), srv.oid)?;
    let key = connection::get_connection(ctx, &m, false)?;
    let intr = crate::remote::interrupts(ctx);
    let query = |sql: &str| {
        connection::with_conn(ctx, key, |c| {
            let r = c.exec(sql, &intr)?;
            if r.status != Status::TuplesOk {
                return Err(connection::report(&r, c, Some(sql)));
            }
            Ok(r)
        })
    };
    let check = format!("SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = {}", string_literal(imp.remote_schema));
    if query(&check)?.rows.len() != 1 {
        return Err(Error::new(
            "HV00C",
            format!("schema \"{}\" is not present on foreign server \"{}\"", imp.remote_schema, srv.name),
        ));
    }
    let mut sql = String::from(
        "SELECT relname,   attname,   format_type(atttypid, atttypmod),   attnotnull,   pg_get_expr(adbin, adrelid),   attgenerated, ",
    );
    sql.push_str(if collate { "  collname,   collnsp.nspname " } else { "  NULL, NULL " });
    sql.push_str(
        "FROM pg_class c   JOIN pg_namespace n ON     relnamespace = n.oid   LEFT JOIN pg_attribute a ON     attrelid = c.oid AND attnum > 0       AND NOT attisdropped   LEFT JOIN pg_attrdef ad ON     adrelid = c.oid AND adnum = attnum ",
    );
    if collate {
        sql.push_str("  LEFT JOIN pg_collation coll ON     coll.oid = attcollation   LEFT JOIN pg_namespace collnsp ON     collnsp.oid = collnamespace ");
    }
    sql.push_str(&format!("WHERE c.relkind IN ('r','v','f','m','p')   AND n.nspname = {}", string_literal(imp.remote_schema)));
    if !matches!(imp.list, Some((true, _))) {
        sql.push_str(" AND NOT c.relispartition ");
    }
    if let Some((limit, tables)) = &imp.list {
        sql.push_str(&format!(
            " AND c.relname {}IN ({})",
            if *limit { "" } else { "NOT " },
            tables.iter().map(|t| string_literal(t)).collect::<Vec<_>>().join(", ")
        ));
    }
    sql.push_str(" ORDER BY c.relname, a.attnum");
    let res = query(&sql)?;
    let mut out = Vec::new();
    let mut i = 0;
    let rows = &res.rows;
    let get = |r: usize, c: usize| rows[r].get(c).cloned().flatten();
    while i < rows.len() {
        let table = get(i, 0).unwrap_or_default();
        let mut cols = Vec::new();
        while i < rows.len() && get(i, 0).as_deref() == Some(table.as_str()) {
            if let Some(att) = get(i, 1) {
                let mut c = format!("  {} {}", quote_ident(&att), get(i, 2).unwrap_or_default());
                c.push_str(&format!(" OPTIONS (column_name {})", string_literal(&att)));
                if let (true, Some(cn), Some(cs)) = (collate, get(i, 6), get(i, 7)) {
                    c.push_str(&format!(" COLLATE {}.{}", quote_ident(&cs), quote_ident(&cn)));
                }
                let gener = get(i, 5).filter(|g| !g.is_empty());
                if let (true, Some(d), None) = (default, get(i, 4), &gener) {
                    c.push_str(&format!(" DEFAULT {d}"));
                }
                if generated && gener.as_deref() == Some("s") {
                    c.push_str(&format!(" GENERATED ALWAYS AS ({}) STORED", get(i, 4).unwrap_or_default()));
                }
                if not_null && get(i, 3).as_deref() == Some("t") {
                    c.push_str(" NOT NULL");
                }
                cols.push(c);
            }
            i += 1;
        }
        let text = format!(
            "CREATE FOREIGN TABLE {} (\n{}\n) SERVER {}\nOPTIONS (schema_name {}, table_name {});",
            quote_ident(&table),
            cols.join(",\n"),
            quote_ident(&srv.name),
            string_literal(imp.remote_schema),
            string_literal(&table)
        );
        out.push((table, text));
    }
    Ok(out)
}
