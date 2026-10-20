//! Mapping tables, queries, cursors, schemas and the database to XML and XML Schema
//! (xml.c's `table_to_xml` family, SQL/XML:2008 sections 9.11 to 9.17).

use super::{FnCtx, Registry, text_arg};
use crate::error::{Error, Frame, Result, code};
use crate::exec::Row;
use crate::sql::plan::OutCol;
use crate::types::xml::map_sql_identifier_to_xml_name;
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

const NAMESPACE_XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
const NAMESPACE_XSD: &str = "http://www.w3.org/2001/XMLSchema";

/// The options every mapping takes.
struct Opts<'a> {
    nulls: bool,
    tableforest: bool,
    targetns: &'a str,
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A catalog query's rows.
fn catalog(c: &FnCtx, sql: &str) -> Result<Vec<Row>> {
    Ok(crate::plpgsql::spi_query(c.exec, sql)?.0)
}

fn oid_of(v: &Value) -> u32 {
    match v {
        Value::Oid(o) => *o,
        Value::Int4(i) => *i as u32,
        Value::Int8(i) => *i as u32,
        _ => 0,
    }
}

fn str_of(v: &Value) -> String {
    match v {
        Value::Text(s) => s.to_string(),
        Value::Null => String::new(),
        v => crate::types::text::format_value(v, &DataType::Text, &Default::default()),
    }
}

/// A column of a row type: its name and type oid.
type Att = (String, u32);

fn query_to_oid_list(c: &FnCtx, sql: &str) -> Result<Vec<u32>> {
    Ok(catalog(c, sql)?.iter().map(|r| oid_of(&r[0])).collect())
}

const VISIBLE_SCHEMAS: &str = "SELECT oid FROM pg_catalog.pg_namespace WHERE pg_catalog.has_schema_privilege (oid, 'USAGE') AND NOT (nspname ~ '^pg_' OR nspname = 'information_schema')";

fn schema_visible_tables(c: &FnCtx, nspid: u32) -> Result<Vec<u32>> {
    query_to_oid_list(
        c,
        &format!(
            "SELECT oid FROM pg_catalog.pg_class WHERE relnamespace = {nspid} AND relkind IN ('r','m','v') AND pg_catalog.has_table_privilege (oid, 'SELECT') ORDER BY relname;"
        ),
    )
}

fn database_visible_schemas(c: &FnCtx) -> Result<Vec<u32>> {
    query_to_oid_list(c, &format!("{VISIBLE_SCHEMAS} ORDER BY nspname;"))
}

fn database_visible_tables(c: &FnCtx) -> Result<Vec<u32>> {
    query_to_oid_list(
        c,
        &format!(
            "SELECT oid FROM pg_catalog.pg_class WHERE relkind IN ('r','m','v') AND pg_catalog.has_table_privilege(pg_class.oid, 'SELECT') AND relnamespace IN ({VISIBLE_SCHEMAS}) ORDER BY oid;"
        ),
    )
}

fn database_name(c: &FnCtx) -> String {
    c.exec.sess.database.clone()
}

fn namespace_name(c: &FnCtx, nspid: u32) -> Result<String> {
    let rows = catalog(
        c,
        &format!(
            "SELECT nspname::pg_catalog.text FROM pg_catalog.pg_namespace WHERE oid = {nspid}"
        ),
    )?;
    Ok(rows.first().map(|r| str_of(&r[0])).unwrap_or_default())
}

/// `LookupExplicitNamespace`.
fn lookup_namespace(c: &FnCtx, name: &str) -> Result<u32> {
    let rows = catalog(
        c,
        &format!(
            "SELECT oid, pg_catalog.has_schema_privilege(oid, 'USAGE') FROM pg_catalog.pg_namespace WHERE nspname = {}",
            lit(name)
        ),
    )?;
    let Some(r) = rows.first() else {
        return Err(Error::new(
            code::INVALID_SCHEMA_NAME,
            format!("schema \"{name}\" does not exist"),
        ));
    };
    if !matches!(r[1], Value::Bool(true)) {
        return Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for schema {name}"),
        ));
    }
    Ok(oid_of(&r[0]))
}

/// A relation's name, schema oid and its `regclassout` text.
fn relation(c: &FnCtx, relid: u32) -> Result<(String, u32, String)> {
    let rows = catalog(
        c,
        &format!(
            "SELECT relname::pg_catalog.text, relnamespace, oid::pg_catalog.regclass::pg_catalog.text FROM pg_catalog.pg_class WHERE oid = {relid}"
        ),
    )?;
    let Some(r) = rows.first() else {
        return Err(crate::error::internal(format!(
            "cache lookup failed for relation {relid}"
        )));
    };
    Ok((str_of(&r[0]), oid_of(&r[1]), str_of(&r[2])))
}

/// A relation's columns (`rd_att`, without dropped ones).
fn relation_atts(c: &FnCtx, relid: u32) -> Result<Vec<Att>> {
    let rows = catalog(
        c,
        &format!(
            "SELECT attname::pg_catalog.text, atttypid FROM pg_catalog.pg_attribute WHERE attrelid = {relid} AND attnum > 0 AND NOT attisdropped ORDER BY attnum"
        ),
    )?;
    Ok(rows
        .iter()
        .map(|r| (str_of(&r[0]), oid_of(&r[1])))
        .collect())
}

/// The type a result column has, as a tuple descriptor would give it.
fn col_type_oid(col: &OutCol) -> u32 {
    col.domain.unwrap_or_else(|| col.ty.oid())
}

fn query_atts(cols: &[OutCol]) -> Vec<Att> {
    cols.iter()
        .map(|c| (c.name.clone(), col_type_oid(c)))
        .collect()
}

/// A pg_type row: name, schema name, typtype, typbasetype, typtypmod.
fn type_row(c: &FnCtx, oid: u32) -> Result<(String, String, String, u32, i32)> {
    let rows = catalog(
        c,
        &format!(
            "SELECT t.typname::pg_catalog.text, n.nspname::pg_catalog.text, t.typtype::pg_catalog.text, t.typbasetype, t.typtypmod FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace WHERE t.oid = {oid}"
        ),
    )?;
    let Some(r) = rows.first() else {
        return Err(crate::error::internal(format!(
            "cache lookup failed for type {oid}"
        )));
    };
    let typmod = match r[4] {
        Value::Int4(i) => i,
        _ => -1,
    };
    Ok((
        str_of(&r[0]),
        str_of(&r[1]),
        str_of(&r[2]),
        oid_of(&r[3]),
        typmod,
    ))
}

/// `getBaseTypeAndTypmod`.
fn base_type(c: &FnCtx, mut oid: u32) -> Result<(u32, i32)> {
    let mut typmod = -1;
    loop {
        let (_, _, typtype, base, tm) = type_row(c, oid)?;
        if typtype != "d" {
            return Ok((oid, typmod));
        }
        oid = base;
        typmod = tm;
    }
}

fn multipart_name(parts: &[Option<&str>]) -> String {
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if let Some(p) = p {
            if i > 0 {
                out.push('.');
            }
            out.push_str(&map_sql_identifier_to_xml_name(p, true, true));
        }
    }
    out
}

mod oids {
    pub const BOOL: u32 = 16;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const TEXT: u32 = 25;
    pub const XML: u32 = 142;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const DATE: u32 = 1082;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const TIMETZ: u32 = 1266;
    pub const BYTEA: u32 = 17;
    pub const NUMERIC: u32 = 1700;
}

/// `map_sql_type_to_xml_name`.
fn type_xml_name(c: &FnCtx, oid: u32, typmod: i32) -> Result<String> {
    use oids::*;
    let with = |plain: &str, n: Option<String>| {
        if typmod == -1 {
            plain.to_string()
        } else {
            format!("{plain}_{}", n.unwrap())
        }
    };
    Ok(match oid {
        BPCHAR => with("CHAR", Some((typmod - 4).to_string())),
        VARCHAR => with("VARCHAR", Some((typmod - 4).to_string())),
        NUMERIC => with(
            "NUMERIC",
            Some(format!(
                "{}_{}",
                ((typmod - 4) >> 16) & 0xffff,
                (typmod - 4) & 0xffff
            )),
        ),
        INT4 => "INTEGER".into(),
        INT2 => "SMALLINT".into(),
        INT8 => "BIGINT".into(),
        FLOAT4 => "REAL".into(),
        FLOAT8 => "DOUBLE".into(),
        BOOL => "BOOLEAN".into(),
        TIME => with("TIME", Some(typmod.to_string())),
        TIMETZ => with("TIME_WTZ", Some(typmod.to_string())),
        TIMESTAMP => with("TIMESTAMP", Some(typmod.to_string())),
        TIMESTAMPTZ => with("TIMESTAMP_WTZ", Some(typmod.to_string())),
        DATE => "DATE".into(),
        XML => "XML".into(),
        _ => {
            let (name, nsp, typtype, _, _) = type_row(c, oid)?;
            let db = database_name(c);
            multipart_name(&[
                Some(if typtype == "d" { "Domain" } else { "UDT" }),
                Some(&db),
                Some(&nsp),
                Some(&name),
            ])
        }
    })
}

/// `map_sql_type_to_xmlschema_type` (always with typmod -1).
fn type_xmlschema_type(c: &FnCtx, oid: u32) -> Result<String> {
    use oids::*;
    if oid == XML {
        return Ok("<xsd:complexType mixed=\"true\">\n  <xsd:sequence>\n    <xsd:any name=\"element\" minOccurs=\"0\" maxOccurs=\"unbounded\" processContents=\"skip\"/>\n  </xsd:sequence>\n</xsd:complexType>\n".into());
    }
    let mut out = format!("<xsd:simpleType name=\"{}\">\n", type_xml_name(c, oid, -1)?);
    let nd = "\\p{Nd}";
    let restriction =
        |base: &str| format!("  <xsd:restriction base=\"xsd:{base}\"></xsd:restriction>\n");
    let range = |base: &str, max: String, min: String| {
        format!(
            "  <xsd:restriction base=\"xsd:{base}\">\n    <xsd:maxInclusive value=\"{max}\"/>\n    <xsd:minInclusive value=\"{min}\"/>\n  </xsd:restriction>\n"
        )
    };
    match oid {
        BPCHAR | VARCHAR | TEXT => out.push_str("  <xsd:restriction base=\"xsd:string\">\n  </xsd:restriction>\n"),
        BYTEA => {
            let base = if c.exec.fmt().xml_binary_hex { "hexBinary" } else { "base64Binary" };
            out.push_str(&format!("  <xsd:restriction base=\"xsd:{base}\">\n  </xsd:restriction>\n"));
        }
        NUMERIC => {}
        INT2 => out.push_str(&range("short", i16::MAX.to_string(), i16::MIN.to_string())),
        INT4 => out.push_str(&range("int", i32::MAX.to_string(), i32::MIN.to_string())),
        INT8 => out.push_str(&range("long", i64::MAX.to_string(), i64::MIN.to_string())),
        FLOAT4 => out.push_str(&restriction("float")),
        FLOAT8 => out.push_str(&restriction("double")),
        BOOL => out.push_str(&restriction("boolean")),
        TIME | TIMETZ => {
            let tz = if oid == TIMETZ { format!("(\\+|-){nd}{{2}}:{nd}{{2}}") } else { String::new() };
            out.push_str(&format!("  <xsd:restriction base=\"xsd:time\">\n    <xsd:pattern value=\"{nd}{{2}}:{nd}{{2}}:{nd}{{2}}(.{nd}+)?{tz}\"/>\n  </xsd:restriction>\n"));
        }
        TIMESTAMP | TIMESTAMPTZ => {
            let tz = if oid == TIMESTAMPTZ { format!("(\\+|-){nd}{{2}}:{nd}{{2}}") } else { String::new() };
            out.push_str(&format!(
                "  <xsd:restriction base=\"xsd:dateTime\">\n    <xsd:pattern value=\"{nd}{{4}}-{nd}{{2}}-{nd}{{2}}T{nd}{{2}}:{nd}{{2}}:{nd}{{2}}(.{nd}+)?{tz}\"/>\n  </xsd:restriction>\n"
            ));
        }
        DATE => out.push_str(&format!("  <xsd:restriction base=\"xsd:date\">\n    <xsd:pattern value=\"{nd}{{4}}-{nd}{{2}}-{nd}{{2}}\"/>\n  </xsd:restriction>\n")),
        _ => {
            let (_, _, typtype, _, _) = type_row(c, oid)?;
            if typtype == "d" {
                let (base, typmod) = base_type(c, oid)?;
                out.push_str(&format!("  <xsd:restriction base=\"{}\"/>\n", type_xml_name(c, base, typmod)?));
            }
        }
    }
    out.push_str("</xsd:simpleType>\n");
    Ok(out)
}

/// `map_sql_typecoll_to_xmlschema_types`.
fn typecoll_types(c: &FnCtx, descs: &[Vec<Att>]) -> Result<String> {
    let mut unique: Vec<u32> = Vec::new();
    for d in descs {
        for (_, t) in d {
            if !unique.contains(t) {
                unique.push(*t);
            }
        }
    }
    let mut i = 0;
    while i < unique.len() {
        let (base, _) = base_type(c, unique[i])?;
        if base != unique[i] && !unique.contains(&base) {
            unique.push(base);
        }
        i += 1;
    }
    let mut out = String::new();
    for t in unique {
        out.push_str(&type_xmlschema_type(c, t)?);
        out.push('\n');
    }
    Ok(out)
}

fn xsd_schema_start(out: &mut String, targetns: &str) {
    out.push_str(&format!("<xsd:schema\n    xmlns:xsd=\"{NAMESPACE_XSD}\""));
    if !targetns.is_empty() {
        out.push_str(&format!(
            "\n    targetNamespace=\"{targetns}\"\n    elementFormDefault=\"qualified\""
        ));
    }
    out.push_str(">\n\n");
}

fn xsd_schema_end(out: &mut String) {
    out.push_str("</xsd:schema>");
}

/// `map_sql_table_to_xmlschema`: `rel` is the relation's (name, schema name), if any.
fn table_xmlschema(c: &FnCtx, atts: &[Att], rel: Option<(&str, &str)>, o: &Opts) -> Result<String> {
    let (xmltn, tabletype, rowtype) = match rel {
        Some((name, nsp)) => {
            let db = database_name(c);
            (
                map_sql_identifier_to_xml_name(name, true, false),
                multipart_name(&[Some("TableType"), Some(&db), Some(nsp), Some(name)]),
                multipart_name(&[Some("RowType"), Some(&db), Some(nsp), Some(name)]),
            )
        }
        None => (
            (if o.tableforest { "row" } else { "table" }).to_string(),
            "TableType".to_string(),
            "RowType".to_string(),
        ),
    };
    let mut out = String::new();
    xsd_schema_start(&mut out, o.targetns);
    out.push_str(&typecoll_types(c, std::slice::from_ref(&atts.to_vec()))?);
    out.push_str(&format!(
        "<xsd:complexType name=\"{rowtype}\">\n  <xsd:sequence>\n"
    ));
    for (name, ty) in atts {
        out.push_str(&format!(
            "    <xsd:element name=\"{}\" type=\"{}\"{}></xsd:element>\n",
            map_sql_identifier_to_xml_name(name, true, false),
            type_xml_name(c, *ty, -1)?,
            if o.nulls {
                " nillable=\"true\""
            } else {
                " minOccurs=\"0\""
            }
        ));
    }
    out.push_str("  </xsd:sequence>\n</xsd:complexType>\n\n");
    if !o.tableforest {
        out.push_str(&format!(
            "<xsd:complexType name=\"{tabletype}\">\n  <xsd:sequence>\n    <xsd:element name=\"row\" type=\"{rowtype}\" minOccurs=\"0\" maxOccurs=\"unbounded\"/>\n  </xsd:sequence>\n</xsd:complexType>\n\n"
        ));
        out.push_str(&format!(
            "<xsd:element name=\"{xmltn}\" type=\"{tabletype}\"/>\n\n"
        ));
    } else {
        out.push_str(&format!(
            "<xsd:element name=\"{xmltn}\" type=\"{rowtype}\"/>\n\n"
        ));
    }
    xsd_schema_end(&mut out);
    Ok(out)
}

/// `xmldata_root_element_start`.
fn root_start(
    out: &mut String,
    eltname: &str,
    xmlschema: Option<&str>,
    targetns: &str,
    top_level: bool,
) {
    out.push_str(&format!("<{eltname}"));
    if top_level {
        out.push_str(&format!(" xmlns:xsi=\"{NAMESPACE_XSI}\""));
        if !targetns.is_empty() {
            out.push_str(&format!(" xmlns=\"{targetns}\""));
        }
    }
    if xmlschema.is_some() {
        if !targetns.is_empty() {
            out.push_str(&format!(" xsi:schemaLocation=\"{targetns} #\""));
        } else {
            out.push_str(" xsi:noNamespaceSchemaLocation=\"#\"");
        }
    }
    out.push_str(">\n");
}

fn root_end(out: &mut String, eltname: &str) {
    out.push_str(&format!("</{eltname}>\n"));
}

/// `SPI_sql_row_to_xmlelement`.
fn row_to_xmlelement(
    c: &FnCtx,
    out: &mut String,
    row: &Row,
    cols: &[OutCol],
    tablename: Option<&str>,
    o: &Opts,
    top_level: bool,
) -> Result<()> {
    let xmltn = match tablename {
        Some(t) => map_sql_identifier_to_xml_name(t, true, false),
        None => (if o.tableforest { "row" } else { "table" }).to_string(),
    };
    if o.tableforest {
        root_start(out, &xmltn, None, o.targetns, top_level);
    } else {
        out.push_str("<row>\n");
    }
    for (v, col) in row.iter().zip(cols) {
        let colname = map_sql_identifier_to_xml_name(&col.name, true, false);
        if v.is_null() {
            if o.nulls {
                out.push_str(&format!("  <{colname} xsi:nil=\"true\"/>\n"));
            }
        } else {
            let s = super::xml::map_sql_value_to_xml_value(v, &col.ty, true, c.exec.fmt())?;
            out.push_str(&format!("  <{colname}>{s}</{colname}>\n"));
        }
    }
    if o.tableforest {
        root_end(out, &xmltn);
        out.push('\n');
    } else {
        out.push_str("</row>\n\n");
    }
    Ok(())
}

/// `CreateCommandName` of a statement a read-only SPI_execute refuses, or None for a plain
/// SELECT.
fn not_read_only(node: &NodeEnum) -> Option<String> {
    use NodeEnum as N;
    if let Some(tag) = crate::session::ddl::write_command(node) {
        return Some(tag);
    }
    match node {
        N::SelectStmt(s) => {
            if let Some(N::LockingClause(l)) =
                s.locking_clause.first().and_then(|n| n.node.as_ref())
                && !s.from_clause.is_empty()
            {
                let strength = pb::LockClauseStrength::try_from(l.strength)
                    .unwrap_or(pb::LockClauseStrength::LcsForupdate);
                return Some(
                    match strength {
                        pb::LockClauseStrength::LcsForkeyshare => "SELECT FOR KEY SHARE",
                        pb::LockClauseStrength::LcsForshare => "SELECT FOR SHARE",
                        pb::LockClauseStrength::LcsFornokeyupdate => "SELECT FOR NO KEY UPDATE",
                        _ => "SELECT FOR UPDATE",
                    }
                    .into(),
                );
            }
            let modifying = s.with_clause.as_ref().is_some_and(|w| {
                w.ctes.iter().any(|c| matches!(c.node.as_ref(), Some(N::CommonTableExpr(c)) if !matches!(c.ctequery.as_deref().and_then(|q| q.node.as_ref()), Some(N::SelectStmt(_)))))
            });
            modifying.then(|| "SELECT".into())
        }
        N::VariableShowStmt(_) => Some("SHOW".into()),
        N::ExplainStmt(_) => Some("EXPLAIN".into()),
        N::VariableSetStmt(s)
            if s.kind == pb::VariableSetKind::VarReset as i32
                || s.kind == pb::VariableSetKind::VarResetAll as i32 =>
        {
            Some("RESET".into())
        }
        N::VariableSetStmt(_) => Some("SET".into()),
        N::TransactionStmt(_) => Some("BEGIN".into()),
        N::CheckPointStmt(_) => Some("CHECKPOINT".into()),
        N::DeclareCursorStmt(_) => Some("DECLARE CURSOR".into()),
        N::FetchStmt(f) => Some((if f.ismove { "MOVE" } else { "FETCH" }).into()),
        N::ClosePortalStmt(_) => Some("CLOSE CURSOR".into()),
        N::ListenStmt(_) => Some("LISTEN".into()),
        N::NotifyStmt(_) => Some("NOTIFY".into()),
        N::VacuumStmt(v) => Some((if v.is_vacuumcmd { "VACUUM" } else { "ANALYZE" }).into()),
        _ => Some("???".into()),
    }
}

/// `SPI_execute(query, true, 0)`: the last statement's rows, which must come from a SELECT.
fn execute_read_only(c: &FnCtx, sql: &str) -> Result<(Vec<Row>, Vec<OutCol>)> {
    let sess = c.exec.sess;
    let frame = || Frame::Query {
        kind: "SQL statement",
        text: sql.to_string(),
        shift: 0,
    };
    let stmts = sess.framed(frame(), |_| crate::plpgsql::parse_spi(c.exec, sql))?;
    let mut last = None;
    for (i, st) in stmts.iter().enumerate() {
        let Some(node) = st.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
            continue;
        };
        if let Some(tag) = not_read_only(node) {
            return sess.framed(frame(), |_| {
                Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("{tag} is not allowed in a non-volatile function"),
                ))
            });
        }
        let text = if stmts.len() == 1 {
            sql
        } else {
            let start = st.stmt_location.max(0) as usize;
            let end = if st.stmt_len == 0 {
                sql.len()
            } else {
                start + st.stmt_len as usize
            };
            sql.get(start..end).unwrap_or(sql)
        };
        let r = sess.framed(frame(), |_| crate::plpgsql::spi_query(c.exec, text))?;
        if i + 1 == stmts.len() {
            last = Some(r);
        }
    }
    last.ok_or_else(|| Error::new(code::DATA_EXCEPTION, "invalid query"))
}

/// `SPI_prepare` + `SPI_cursor_open(read only)`: the query's columns.
fn query_columns(c: &FnCtx, sql: &str) -> Result<Vec<OutCol>> {
    let sess = c.exec.sess;
    let frame = || Frame::Query {
        kind: "SQL statement",
        text: sql.to_string(),
        shift: 0,
    };
    let stmts = sess.framed(frame(), |_| crate::plpgsql::parse_spi(c.exec, sql))?;
    if let [st] = &stmts[..]
        && let Some(node) = st.stmt.as_ref().and_then(|s| s.node.as_ref())
        && matches!(
            node,
            NodeEnum::VariableShowStmt(_) | NodeEnum::ExplainStmt(_)
        )
    {
        let tag = not_read_only(node).unwrap_or_default();
        return sess.framed(frame(), |_| {
            Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("{tag} is not allowed in a non-volatile function"),
            ))
        });
    }
    Ok(sess
        .framed(frame(), |_| crate::plpgsql::spi_query(c.exec, sql))?
        .1)
}

/// `query_to_xml_internal`.
fn query_to_xml_internal(
    c: &FnCtx,
    sql: &str,
    tablename: Option<&str>,
    xmlschema: Option<&str>,
    o: &Opts,
    top_level: bool,
) -> Result<String> {
    let xmltn = match tablename {
        Some(t) => map_sql_identifier_to_xml_name(t, true, false),
        None => "table".into(),
    };
    let mut out = String::new();
    let (rows, cols) = execute_read_only(c, sql)?;
    if !o.tableforest {
        root_start(&mut out, &xmltn, xmlschema, o.targetns, top_level);
        out.push('\n');
    }
    if let Some(s) = xmlschema {
        out.push_str(&format!("{s}\n\n"));
    }
    for r in &rows {
        row_to_xmlelement(c, &mut out, r, &cols, tablename, o, top_level)?;
    }
    if !o.tableforest {
        root_end(&mut out, &xmltn);
    }
    Ok(out)
}

/// `table_to_xml_internal`.
fn table_to_xml_internal(
    c: &FnCtx,
    relid: u32,
    xmlschema: Option<&str>,
    o: &Opts,
    top_level: bool,
) -> Result<String> {
    let (name, _, regclass) = relation(c, relid)?;
    query_to_xml_internal(
        c,
        &format!("SELECT * FROM {regclass}"),
        Some(&name),
        xmlschema,
        o,
        top_level,
    )
}

fn table_xmlschema_of(c: &FnCtx, relid: u32, o: &Opts) -> Result<String> {
    let (name, nsp, _) = relation(c, relid)?;
    let nsp = namespace_name(c, nsp)?;
    let atts = relation_atts(c, relid)?;
    table_xmlschema(c, &atts, Some((&name, &nsp)), o)
}

/// `schema_to_xml_internal`.
fn schema_to_xml_internal(
    c: &FnCtx,
    nspid: u32,
    xmlschema: Option<&str>,
    o: &Opts,
    top_level: bool,
) -> Result<String> {
    let xmlsn = map_sql_identifier_to_xml_name(&namespace_name(c, nspid)?, true, false);
    let mut out = String::new();
    root_start(&mut out, &xmlsn, xmlschema, o.targetns, top_level);
    out.push('\n');
    if let Some(s) = xmlschema {
        out.push_str(&format!("{s}\n\n"));
    }
    for relid in schema_visible_tables(c, nspid)? {
        out.push_str(&table_to_xml_internal(c, relid, None, o, false)?);
        out.push('\n');
    }
    root_end(&mut out, &xmlsn);
    Ok(out)
}

/// `map_sql_schema_to_xmlschema_types`.
fn schema_xmlschema_types(c: &FnCtx, nspid: u32, relids: &[u32], o: &Opts) -> Result<String> {
    let db = database_name(c);
    let nspname = namespace_name(c, nspid)?;
    let xmlsn = map_sql_identifier_to_xml_name(&nspname, true, false);
    let schematype = multipart_name(&[Some("SchemaType"), Some(&db), Some(&nspname)]);
    let mut out = format!("<xsd:complexType name=\"{schematype}\">\n");
    out.push_str(if o.tableforest {
        "  <xsd:sequence>\n"
    } else {
        "  <xsd:all>\n"
    });
    for &relid in relids {
        let (relname, _, _) = relation(c, relid)?;
        let xmltn = map_sql_identifier_to_xml_name(&relname, true, false);
        let tabletype = multipart_name(&[
            Some(if o.tableforest {
                "RowType"
            } else {
                "TableType"
            }),
            Some(&db),
            Some(&nspname),
            Some(&relname),
        ]);
        if o.tableforest {
            out.push_str(&format!("    <xsd:element name=\"{xmltn}\" type=\"{tabletype}\" minOccurs=\"0\" maxOccurs=\"unbounded\"/>\n"));
        } else {
            out.push_str(&format!(
                "    <xsd:element name=\"{xmltn}\" type=\"{tabletype}\"/>\n"
            ));
        }
    }
    out.push_str(if o.tableforest {
        "  </xsd:sequence>\n"
    } else {
        "  </xsd:all>\n"
    });
    out.push_str("</xsd:complexType>\n\n");
    out.push_str(&format!(
        "<xsd:element name=\"{xmlsn}\" type=\"{schematype}\"/>\n\n"
    ));
    Ok(out)
}

/// `schema_to_xmlschema_internal`.
fn schema_to_xmlschema_internal(c: &FnCtx, schemaname: &str, o: &Opts) -> Result<String> {
    let nspid = lookup_namespace(c, schemaname)?;
    let mut out = String::new();
    xsd_schema_start(&mut out, o.targetns);
    let relids = schema_visible_tables(c, nspid)?;
    let descs = relids
        .iter()
        .map(|&r| relation_atts(c, r))
        .collect::<Result<Vec<_>>>()?;
    out.push_str(&typecoll_types(c, &descs)?);
    out.push_str(&schema_xmlschema_types(c, nspid, &relids, o)?);
    xsd_schema_end(&mut out);
    Ok(out)
}

/// `database_to_xml_internal`.
fn database_to_xml_internal(c: &FnCtx, xmlschema: Option<&str>, o: &Opts) -> Result<String> {
    let xmlcn = map_sql_identifier_to_xml_name(&database_name(c), true, false);
    let mut out = String::new();
    root_start(&mut out, &xmlcn, xmlschema, o.targetns, true);
    out.push('\n');
    if let Some(s) = xmlschema {
        out.push_str(&format!("{s}\n\n"));
    }
    for nspid in database_visible_schemas(c)? {
        out.push_str(&schema_to_xml_internal(c, nspid, None, o, false)?);
        out.push('\n');
    }
    root_end(&mut out, &xmlcn);
    Ok(out)
}

/// `database_to_xmlschema_internal`.
fn database_to_xmlschema_internal(c: &FnCtx, o: &Opts) -> Result<String> {
    let mut out = String::new();
    xsd_schema_start(&mut out, o.targetns);
    let relids = database_visible_tables(c)?;
    let nspids = database_visible_schemas(c)?;
    let descs = relids
        .iter()
        .map(|&r| relation_atts(c, r))
        .collect::<Result<Vec<_>>>()?;
    out.push_str(&typecoll_types(c, &descs)?);
    // map_sql_catalog_to_xmlschema_types
    let db = database_name(c);
    let xmlcn = map_sql_identifier_to_xml_name(&db, true, false);
    let catalogtype = multipart_name(&[Some("CatalogType"), Some(&db)]);
    out.push_str(&format!(
        "<xsd:complexType name=\"{catalogtype}\">\n  <xsd:all>\n"
    ));
    for nspid in nspids {
        let nspname = namespace_name(c, nspid)?;
        let xmlsn = map_sql_identifier_to_xml_name(&nspname, true, false);
        let schematype = multipart_name(&[Some("SchemaType"), Some(&db), Some(&nspname)]);
        out.push_str(&format!(
            "    <xsd:element name=\"{xmlsn}\" type=\"{schematype}\"/>\n"
        ));
    }
    out.push_str("  </xsd:all>\n</xsd:complexType>\n\n");
    out.push_str(&format!(
        "<xsd:element name=\"{xmlcn}\" type=\"{catalogtype}\"/>\n\n"
    ));
    xsd_schema_end(&mut out);
    Ok(out)
}

fn b(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

fn opts(a: &[Value]) -> Opts<'_> {
    let n = a.len();
    Opts {
        nulls: b(&a[n - 3]),
        tableforest: b(&a[n - 2]),
        targetns: text_arg(&a[n - 1]),
    }
}

fn xml(s: String) -> Result<Value> {
    Ok(Value::text(s))
}

/// A cursor's columns, or "cursor does not exist".
fn with_cursor<T>(
    c: &FnCtx,
    name: &str,
    f: impl FnOnce(&mut crate::session::cursor::Cursor) -> Result<T>,
) -> Result<T> {
    let mut cursors = c.exec.sess.cursors.borrow_mut();
    match cursors.get_mut(name) {
        Some(cur) => f(cur),
        None => Err(Error::new(
            code::INVALID_CURSOR_NAME,
            format!("cursor \"{name}\" does not exist"),
        )),
    }
}

fn table_to_xml(a: &[Value], c: &FnCtx) -> Result<Value> {
    xml(table_to_xml_internal(
        c,
        oid_of(&a[0]),
        None,
        &opts(a),
        true,
    )?)
}

fn query_to_xml(a: &[Value], c: &FnCtx) -> Result<Value> {
    xml(query_to_xml_internal(
        c,
        text_arg(&a[0]),
        None,
        None,
        &opts(a),
        true,
    )?)
}

fn cursor_to_xml(a: &[Value], c: &FnCtx) -> Result<Value> {
    let o = opts(a);
    let name = text_arg(&a[0]);
    let count = match a[1] {
        Value::Int4(n) => n as i64,
        _ => 0,
    };
    let mut out = String::new();
    if !o.tableforest {
        root_start(&mut out, "table", None, o.targetns, true);
        out.push('\n');
    }
    let Some((rows, cols, _)) = crate::session::cursor::fetch(
        c.exec,
        name,
        pb::FetchDirection::FetchForward,
        count,
        true,
    )?
    else {
        return Err(Error::new(
            code::INVALID_CURSOR_NAME,
            format!("cursor \"{name}\" does not exist"),
        ));
    };
    for r in &rows {
        row_to_xmlelement(c, &mut out, r, &cols, None, &o, true)?;
    }
    if !o.tableforest {
        root_end(&mut out, "table");
    }
    xml(out)
}

fn table_to_xmlschema(a: &[Value], c: &FnCtx) -> Result<Value> {
    xml(table_xmlschema_of(c, oid_of(&a[0]), &opts(a))?)
}

fn query_to_xmlschema(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cols = query_columns(c, text_arg(&a[0]))?;
    xml(table_xmlschema(c, &query_atts(&cols), None, &opts(a))?)
}

fn cursor_to_xmlschema(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cols = with_cursor(c, text_arg(&a[0]), |cur| Ok(cur.cols.clone()))?;
    xml(table_xmlschema(c, &query_atts(&cols), None, &opts(a))?)
}

fn table_to_xml_and_xmlschema(a: &[Value], c: &FnCtx) -> Result<Value> {
    let o = opts(a);
    let relid = oid_of(&a[0]);
    let schema = table_xmlschema_of(c, relid, &o)?;
    xml(table_to_xml_internal(c, relid, Some(&schema), &o, true)?)
}

fn query_to_xml_and_xmlschema(a: &[Value], c: &FnCtx) -> Result<Value> {
    let o = opts(a);
    let sql = text_arg(&a[0]);
    let cols = query_columns(c, sql)?;
    let schema = table_xmlschema(c, &query_atts(&cols), None, &o)?;
    xml(query_to_xml_internal(
        c,
        sql,
        None,
        Some(&schema),
        &o,
        true,
    )?)
}

fn schema_to_xml(a: &[Value], c: &FnCtx) -> Result<Value> {
    let nspid = lookup_namespace(c, text_arg(&a[0]))?;
    xml(schema_to_xml_internal(c, nspid, None, &opts(a), true)?)
}

fn schema_to_xmlschema(a: &[Value], c: &FnCtx) -> Result<Value> {
    xml(schema_to_xmlschema_internal(c, text_arg(&a[0]), &opts(a))?)
}

fn schema_to_xml_and_xmlschema(a: &[Value], c: &FnCtx) -> Result<Value> {
    let o = opts(a);
    let name = text_arg(&a[0]);
    let nspid = lookup_namespace(c, name)?;
    let schema = schema_to_xmlschema_internal(c, name, &o)?;
    xml(schema_to_xml_internal(c, nspid, Some(&schema), &o, true)?)
}

fn database_to_xml(a: &[Value], c: &FnCtx) -> Result<Value> {
    xml(database_to_xml_internal(c, None, &opts(a))?)
}

fn database_to_xmlschema(a: &[Value], c: &FnCtx) -> Result<Value> {
    xml(database_to_xmlschema_internal(c, &opts(a))?)
}

fn database_to_xml_and_xmlschema(a: &[Value], c: &FnCtx) -> Result<Value> {
    let o = opts(a);
    let schema = database_to_xmlschema_internal(c, &o)?;
    xml(database_to_xml_internal(c, Some(&schema), &o)?)
}

pub fn register(r: &mut Registry) {
    r.from_pg(2923, Some(table_to_xml), None);
    r.from_pg(2924, Some(query_to_xml), None);
    r.from_pg(2925, Some(cursor_to_xml), None);
    r.from_pg(2926, Some(table_to_xmlschema), None);
    r.from_pg(2927, Some(query_to_xmlschema), None);
    r.from_pg(2928, Some(cursor_to_xmlschema), None);
    r.from_pg(2929, Some(table_to_xml_and_xmlschema), None);
    r.from_pg(2930, Some(query_to_xml_and_xmlschema), None);
    r.from_pg(2933, Some(schema_to_xml), None);
    r.from_pg(2934, Some(schema_to_xmlschema), None);
    r.from_pg(2935, Some(schema_to_xml_and_xmlschema), None);
    r.from_pg(2936, Some(database_to_xml), None);
    r.from_pg(2937, Some(database_to_xmlschema), None);
    r.from_pg(2938, Some(database_to_xml_and_xmlschema), None);
}
