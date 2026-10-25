//! tablefunc (`contrib/tablefunc/tablefunc.c`): normal_rand, crosstab (row-ordered and
//! hash-based) and connectby. The queries run as SPI runs them; the result rows are built
//! from text as BuildTupleFromCStrings builds them, by the column types the call expects.

use super::native::Native;
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::{FnCtx, SrfIter, text_arg};
use crate::types::{DataType, Value};

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(Native::set(match sym {
        "normal_rand" => normal_rand,
        "crosstab" => crosstab,
        "crosstab_hash" => crosstab_hash,
        "connectby_text" => |a, c| connectby_entry(a, c, false),
        "connectby_text_serial" => |a, c| connectby_entry(a, c, true),
        _ => return None,
    }))
}

fn rows(v: Vec<Vec<Value>>) -> Result<SrfIter> {
    Ok(Box::new(v.into_iter().map(Ok)))
}

fn normal_rand(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let n = a[0].as_i64().unwrap_or(0);
    if n < 0 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "number of rows cannot be negative"));
    }
    let (mean, stddev) = (a[1].as_f64().unwrap_or(0.0), a[2].as_f64().unwrap_or(0.0));
    let mut out = Vec::with_capacity(n as usize);
    let mut carry: Option<f64> = None;
    for _ in 0..n {
        let v = match carry.take() {
            Some(v) => v,
            None => {
                // get_normal_pair: Knuth's polar method.
                let (x1, x2) = loop {
                    let v1 = 2.0 * c.exec.random() - 1.0;
                    let v2 = 2.0 * c.exec.random() - 1.0;
                    let s = v1 * v1 + v2 * v2;
                    if s < 1.0 {
                        if s == 0.0 {
                            break (0.0, 0.0);
                        }
                        let s = ((-2.0 * s.ln()) / s).sqrt();
                        break (v1 * s, v2 * s);
                    }
                };
                carry = Some(mean + stddev * x2);
                mean + stddev * x1
            }
        };
        out.push(vec![Value::Float8(v)]);
    }
    rows(out)
}

/// The rows (as text, NULL as None) and column types of an SPI query.
struct Spi {
    rows: Vec<Vec<Option<String>>>,
    types: Vec<DataType>,
}

fn spi(sql: &str, c: &FnCtx) -> Result<Spi> {
    let (rows, cols) = crate::plpgsql::spi_query(c.exec, sql)?;
    let types: Vec<DataType> = cols.iter().map(|col| col.ty.clone()).collect();
    let rows = rows
        .into_iter()
        .map(|r| {
            r.iter()
                .zip(&types)
                .map(|(v, t)| (!v.is_null()).then(|| crate::types::text::format_value(v, t, c.exec.fmt())))
                .collect()
        })
        .collect();
    Ok(Spi { rows, types })
}

/// The columns the call returns (its result row type).
fn result_cols(c: &FnCtx) -> Vec<DataType> {
    match c.ret {
        DataType::Row(r) => r.fields.iter().map(|f| f.1.clone()).collect(),
        t => vec![t.clone()],
    }
}

/// BuildTupleFromCStrings.
fn build(values: &[Option<String>], types: &[DataType], c: &FnCtx) -> Result<Vec<Value>> {
    types
        .iter()
        .enumerate()
        .map(|(i, t)| match values.get(i).cloned().flatten() {
            None => Ok(Value::Null),
            Some(s) => crate::types::text::parse_value(&s, t, c.exec.fmt()),
        })
        .collect()
}

fn same_type(c: &FnCtx, ret: &DataType, sql: &DataType) -> bool {
    let cat = c.exec.catalog();
    crate::catalog::system::type_oid(&cat, ret) == crate::catalog::system::type_oid(&cat, sql)
        && (ret.typmod() < 0 || ret.typmod() == sql.typmod())
}

fn mismatch(msg: &str, detail: String) -> Error {
    Error::new(code::DATATYPE_MISMATCH, msg.to_string()).detail(detail)
}

/// `crosstab(text [, int])` and crosstabN: categories in source order, one row per row name.
fn crosstab(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let src = spi(text_arg(&a[0]), c)?;
    if src.rows.is_empty() {
        return rows(Vec::new());
    }
    if src.types.len() != 3 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid crosstab source data query")
            .detail("The query must return 3 columns: row_name, category, and value."));
    }
    let ret = result_cols(c);
    compat_crosstab(c, &ret, &src.types)?;
    let ncat = ret.len() - 1;
    let max = src.rows.len();
    let mut out = Vec::new();
    let mut last: Option<Option<String>> = None;
    let mut call = 0usize;
    while call < max {
        let mut skip = false;
        let mut values: Vec<Option<String>> = vec![None; 1 + ncat];
        for i in 0..ncat {
            if call >= max {
                break;
            }
            let rowid = src.rows[call][0].clone();
            if i == 0 {
                values[0] = rowid.clone();
                if last.as_ref().is_some_and(|l| *l == rowid) {
                    skip = true;
                    break;
                }
            }
            if rowid == values[0] {
                values[1 + i] = src.rows[call][2].clone();
                if i < ncat - 1 {
                    call += 1;
                }
            } else {
                // (Unsigned in C: the counter wraps and the loop's ++ brings it back.)
                call = call.wrapping_sub(1);
                break;
            }
        }
        if !skip {
            out.push(build(&values, &ret, c)?);
        }
        last = Some(values[0].clone());
        call = call.wrapping_add(1);
    }
    rows(out)
}

/// compatCrosstabTupleDescs.
fn compat_crosstab(c: &FnCtx, ret: &[DataType], sql: &[DataType]) -> Result<()> {
    const MSG: &str = "invalid crosstab return type";
    if ret.len() < 2 {
        return Err(mismatch(MSG, "Return row must have at least two columns.".into()));
    }
    if !same_type(c, &ret[0], &sql[0]) {
        return Err(mismatch(MSG, format!(
            "Source row_name datatype {} does not match return row_name datatype {}.",
            sql[0].display(),
            ret[0].display()
        )));
    }
    for (i, r) in ret.iter().enumerate().skip(1) {
        if !same_type(c, r, &sql[2]) {
            return Err(mismatch(MSG, format!(
                "Source value datatype {} does not match return value datatype {} in column {}.",
                sql[2].display(),
                r.display(),
                i + 1
            )));
        }
    }
    Ok(())
}

/// `crosstab(text, text)`: the categories from the second query, by name.
fn crosstab_hash(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let ret = result_cols(c);
    if ret.len() < 2 {
        return Err(mismatch("invalid crosstab return type", "Return row must have at least two columns.".into()));
    }
    // load_categories_hash (keys are cut to MAX_CATNAME_LEN - 1 bytes).
    let cats_q = spi(text_arg(&a[1]), c)?;
    let mut cats: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
    if !cats_q.rows.is_empty() {
        if cats_q.types.len() != 1 {
            return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid crosstab categories query")
                .detail("The query must return one column."));
        }
        for (i, r) in cats_q.rows.iter().enumerate() {
            let Some(name) = &r[0] else {
                return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "crosstab category value must not be null"));
            };
            let key = cat_key(name);
            if cats.insert(key, i).is_some() {
                return Err(Error::new(code::DUPLICATE_OBJECT, "duplicate category name"));
            }
        }
    }
    let src = spi(text_arg(&a[0]), c)?;
    let mut out = Vec::new();
    if src.rows.is_empty() {
        return rows(out);
    }
    if cats.is_empty() {
        return Err(Error::new(code::CARDINALITY_VIOLATION, "crosstab categories query must return at least one row"));
    }
    let ncols = src.types.len();
    if ncols < 3 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid crosstab source data query")
            .detail("The query must return at least 3 columns: row_name, category, and value."));
    }
    let result_ncols = (ncols - 2) + cats.len();
    if ret.len() != result_ncols {
        return Err(mismatch(
            "invalid crosstab return type",
            format!("Return row must have {result_ncols} columns, not {}.", ret.len()),
        ));
    }
    let mut values: Vec<Option<String>> = vec![None; result_ncols];
    let mut last: Option<Option<String>> = None;
    for r in &src.rows {
        let rowid = r[0].clone();
        if last.as_ref().is_none_or(|l| *l != rowid) {
            if last.is_some() {
                out.push(build(&values, &ret, c)?);
                values = vec![None; result_ncols];
            }
            values[0] = rowid.clone();
            values[1..ncols - 2].clone_from_slice(&r[1..ncols - 2]);
        }
        if let Some(cat) = &r[ncols - 2]
            && let Some(&idx) = cats.get(&cat_key(cat))
        {
            values[idx + ncols - 2] = r[ncols - 1].clone();
        }
        last = Some(rowid);
    }
    out.push(build(&values, &ret, c)?);
    rows(out)
}

/// A category's hash key: snprintf into MAX_CATNAME_LEN - 1 (NAMEDATALEN - 1) bytes, a NUL last.
fn cat_key(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    b[..b.len().min(62)].to_vec()
}

fn connectby_entry(a: &[Value], c: &FnCtx, serial: bool) -> Result<SrfIter> {
    let s = |i: usize| text_arg(&a[i]).to_string();
    let (relname, key, parent) = (s(0), s(1), s(2));
    let (orderby, start, depth_at) = if serial { (Some(s(3)), s(4), 5) } else { (None, s(3), 4) };
    let max_depth = a[depth_at].as_i64().unwrap_or(0);
    let show_branch = a.len() == depth_at + 2;
    let delim = if show_branch { s(depth_at + 1) } else { "~".to_string() };
    let ret = result_cols(c);
    validate_connectby(c, &ret, show_branch, serial)?;
    let cb = Connectby {
        key,
        parent,
        relname,
        orderby,
        delim,
        max_depth,
        show_branch,
        ret,
    };
    let mut out = Vec::new();
    let mut n = 1;
    cb.recurse(&start, &start, 0, &mut n, &mut out, c)?;
    rows(out)
}

/// validateConnectbyTupleDesc.
fn validate_connectby(c: &FnCtx, td: &[DataType], show_branch: bool, show_serial: bool) -> Result<()> {
    const MSG: &str = "invalid connectby return type";
    let expected = if show_branch { 4 } else { 3 } + show_serial as usize;
    if td.len() != expected {
        return Err(mismatch(MSG, format!("Return row must have {expected} columns, not {}.", td.len())));
    }
    let cat = c.exec.catalog();
    let oid = |t: &DataType| crate::catalog::system::type_oid(&cat, t);
    if oid(&td[2]) != crate::types::oid::INT4 {
        return Err(mismatch(MSG, "Third return column (depth) must be type integer.".into()));
    }
    if show_branch && oid(&td[3]) != crate::types::oid::TEXT {
        return Err(mismatch(MSG, "Fourth return column (branch) must be type text.".into()));
    }
    if show_branch && show_serial && oid(&td[4]) != crate::types::oid::INT4 {
        return Err(mismatch(MSG, "Fifth return column (serial) must be type integer.".into()));
    }
    if !show_branch && show_serial && oid(&td[3]) != crate::types::oid::INT4 {
        return Err(mismatch(MSG, "Fourth return column (serial) must be type integer.".into()));
    }
    Ok(())
}

struct Connectby {
    key: String,
    parent: String,
    relname: String,
    orderby: Option<String>,
    delim: String,
    max_depth: i64,
    show_branch: bool,
    ret: Vec<DataType>,
}

impl Connectby {
    /// One row of the result: key, parent, level, [branch], [serial].
    fn row(&self, key: Option<String>, parent: Option<String>, level: i64, branch: &str, serial: &mut i32, c: &FnCtx) -> Result<Vec<Value>> {
        let mut v = vec![key, parent, Some(level.to_string())];
        if self.show_branch {
            v.push(Some(branch.to_string()));
        }
        if self.orderby.is_some() {
            v.push(Some(serial.to_string()));
            *serial += 1;
        }
        build(&v, &self.ret, c)
    }

    /// build_tuplestore_recursively.
    #[allow(clippy::too_many_arguments)]
    fn recurse(&self, start: &str, branch: &str, level: i64, serial: &mut i32, out: &mut Vec<Vec<Value>>, c: &FnCtx) -> Result<()> {
        if self.max_depth > 0 && level > self.max_depth {
            return Ok(());
        }
        let lit = crate::sql::func::string::quote_literal_str(start);
        let mut sql = format!(
            "SELECT {k}, {p} FROM {r} WHERE {p} = {lit} AND {k} IS NOT NULL AND {k} <> {p}",
            k = self.key,
            p = self.parent,
            r = self.relname
        );
        if let Some(o) = &self.orderby {
            sql.push_str(&format!(" ORDER BY {o}"));
        }
        let mut level = level;
        if level == 0 {
            out.push(self.row(Some(start.to_string()), None, 0, start, serial, c)?);
            level += 1;
        }
        let q = spi(&sql, c)?;
        if q.rows.is_empty() {
            return Ok(());
        }
        compat_connectby(c, &self.ret, &q.types)?;
        let d = &self.delim;
        for r in &q.rows {
            let mut branchstr = branch.to_string();
            let chk_branch = format!("{d}{branch}{d}");
            let (key, parent) = (r[0].clone(), r[1].clone());
            if let Some(k) = &key {
                if chk_branch.contains(&format!("{d}{k}{d}")) {
                    return Err(Error::new(code::INVALID_RECURSION, "infinite recursion detected"));
                }
                branchstr.push_str(&format!("{d}{k}"));
            }
            out.push(self.row(key.clone(), parent, level, &branchstr, serial, c)?);
            if let Some(k) = &key {
                self.recurse(k, &branchstr, level + 1, serial, out, c)?;
            }
        }
        Ok(())
    }
}

/// compatConnectbyTupleDescs.
fn compat_connectby(c: &FnCtx, ret: &[DataType], sql: &[DataType]) -> Result<()> {
    const MSG: &str = "invalid connectby return type";
    if sql.len() < 2 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid connectby source data query")
            .detail("The query must return at least two columns."));
    }
    if !same_type(c, &ret[0], &sql[0]) {
        return Err(mismatch(MSG, format!(
            "Source key type {} does not match return key type {}.",
            sql[0].display(),
            ret[0].display()
        )));
    }
    if !same_type(c, &ret[1], &sql[1]) {
        return Err(mismatch(MSG, format!(
            "Source parent key type {} does not match return parent key type {}.",
            sql[1].display(),
            ret[1].display()
        )));
    }
    Ok(())
}
