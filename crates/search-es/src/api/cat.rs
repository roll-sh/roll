//! The `_cat` APIs: tables rendered as aligned text, or as JSON/YAML/CBOR arrays of objects.
//!
//! Every cat API builds a [`Table`] — its static column definitions ([`cat_columns`]) plus one
//! row of [`Cell`]s per item — and [`render`] reproduces Elasticsearch's `RestTable`:
//!
//! - `h` picks columns by name or alias (wildcards allowed, unknown names silently dropped),
//!   otherwise the columns marked default are shown; `pri` adds a column's `pri.` sibling;
//!   `ts=false` hides the `epoch`/`timestamp` columns.
//! - `s` sorts by columns (`:asc`/`:desc`), comparing cells of the same kind by value and
//!   anything else by its text, like `TableIndexComparator`.
//! - `bytes` and `time` change how sizes and durations print.
//! - `format` (or an `Accept` with a structured type) returns an array of objects whose values
//!   are the cells' texts (`null` for missing values); otherwise text with `v` adding a header
//!   row, columns padded to their widest cell, right-aligned where the column says so and the
//!   last column left unpadded.
//! - `help` prints the column list (`name | aliases | description`).
//!
//! Stats columns read from [`stats`], which reports zeros until the engine reports numbers.

use super::cat_columns as cols;
use super::cat_columns_os as cols_os;
use crate::cluster::Dialect;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::IndexMeta;
use crate::meta::resolve::IndicesOptions;
use crate::rest::{Ctx, Reply};
use crate::units::{ByteSize, TimeValue};
use crate::xcontent::filter::{FilterPath, glob_match};
use crate::xcontent::{self, MediaType, XContentType};
use bytes::Bytes;
use std::cmp::Ordering;

/// A cat API's columns in the request's dialect.
macro_rules! columns {
    ($ctx:expr, $es:ident, $os:ident) => {
        if $ctx.cluster.dialect == Dialect::OpenSearch { cols_os::$os } else { cols::$es }
    };
    ($ctx:expr, $name:ident) => {
        columns!($ctx, $name, $name)
    };
}

/// When a column exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    Always,
    /// Only for `compatible-with=8` requests (`_cat/shards` `sync_id`).
    CompatV8,
    /// Only with `detailed=true` (`_cat/tasks` `description`).
    Detailed,
}

/// One column of a cat table.
#[derive(Debug)]
pub struct Column {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub desc: Option<&'static str>,
    /// Shown when `h` isn't given.
    pub default: bool,
    /// `text-align:right`.
    pub right: bool,
    /// The flag (`pri`) that adds the `<flag>.<name>` column next to this one.
    pub sibling: Option<&'static str>,
    pub cond: Cond,
}

/// A table cell. Cells of the same kind sort by value, others by their text.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Null,
    Str(String),
    Int(i64),
    /// A `ByteSizeValue` (affected by `bytes=`).
    Bytes(i64),
    /// A `TimeValue` in milliseconds (affected by `time=`).
    Time(TimeValue),
    /// A number shown with a fixed text but sorted by value (`66.7%`, `0.0`).
    Num(String, f64),
}

impl Cell {
    pub fn str(s: impl Into<String>) -> Cell {
        Cell::Str(s.into())
    }

    pub fn opt_str(s: Option<impl Into<String>>) -> Cell {
        s.map(|s| Cell::Str(s.into())).unwrap_or(Cell::Null)
    }

    fn render(&self, bytes: Option<&str>, time: Option<&str>) -> Option<String> {
        Some(match self {
            Cell::Null => return None,
            Cell::Str(s) => s.clone(),
            Cell::Int(i) => i.to_string(),
            Cell::Num(s, _) => s.clone(),
            Cell::Bytes(b) => {
                let div = match bytes {
                    Some("b") => 1,
                    Some("k" | "kb") => 1 << 10,
                    Some("m" | "mb") => 1 << 20,
                    Some("g" | "gb") => 1 << 30,
                    Some("t" | "tb") => 1 << 40,
                    Some("p" | "pb") => 1 << 50,
                    _ => return Some(ByteSize(*b).human()),
                };
                (b / div).to_string()
            }
            Cell::Time(t) => {
                let nanos = t.nanos();
                let v = match time {
                    Some("nanos") => nanos,
                    Some("micros") => nanos / 1_000,
                    Some("ms") => nanos / 1_000_000,
                    Some("s") => nanos / 1_000_000_000,
                    Some("m") => nanos / 60_000_000_000,
                    Some("h") => nanos / 3_600_000_000_000,
                    Some("d") => nanos / 86_400_000_000_000,
                    _ => return Some(t.human()),
                };
                v.to_string()
            }
        })
    }

    fn compare(&self, other: &Cell) -> Ordering {
        match (self, other) {
            (Cell::Null, Cell::Null) => Ordering::Equal,
            (Cell::Null, _) => Ordering::Less,
            (_, Cell::Null) => Ordering::Greater,
            (Cell::Str(a), Cell::Str(b)) => java_str_cmp(a, b),
            (Cell::Int(a), Cell::Int(b)) => a.cmp(b),
            (Cell::Bytes(a), Cell::Bytes(b)) => a.cmp(b),
            (Cell::Time(a), Cell::Time(b)) => a.nanos().cmp(&b.nanos()),
            (Cell::Num(_, a), Cell::Num(_, b)) => a.total_cmp(b),
            (a, b) => java_str_cmp(&a.render(None, None).unwrap_or_default(), &b.render(None, None).unwrap_or_default()),
        }
    }
}

/// Java's `String.compareTo` (UTF-16 code units).
pub fn java_str_cmp(a: &str, b: &str) -> Ordering {
    if a.is_ascii() && b.is_ascii() {
        return a.cmp(b);
    }
    a.encode_utf16().cmp(b.encode_utf16())
}

/// A cat table: column definitions and rows aligned with them.
pub struct Table {
    pub columns: &'static [Column],
    pub rows: Vec<Vec<Cell>>,
    /// Which conditional columns exist for this request.
    pub compat_v8: bool,
    pub detailed: bool,
}

impl Table {
    pub fn new(columns: &'static [Column]) -> Table {
        Table { columns, rows: Vec::new(), compat_v8: false, detailed: false }
    }

    fn exists(&self, c: &Column) -> bool {
        match c.cond {
            Cond::Always => true,
            Cond::CompatV8 => self.compat_v8,
            Cond::Detailed => self.detailed,
        }
    }

    /// Indices (into `columns`) of the columns that exist.
    fn present(&self) -> Vec<usize> {
        (0..self.columns.len()).filter(|&i| self.exists(&self.columns[i])).collect()
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name && self.exists(c))
    }

    /// Adds a row from a function giving each column's cell by name. Columns that don't exist
    /// for this request get `Null` (they are never displayed).
    pub fn push_row(&mut self, mut cell: impl FnMut(&'static str) -> Cell) {
        let row = self.columns.iter().map(|c| if self.exists(c) { cell(c.name) } else { Cell::Null }).collect();
        self.rows.push(row);
    }
}

struct Display {
    col: usize,
    title: String,
}

fn display_headers(t: &Table, ctx: &Ctx) -> Vec<Display> {
    let ts = ctx.bool_param("ts", true).unwrap_or(true);
    let ts_ok = |name: &str| ts || !matches!(name, "epoch" | "timestamp");
    let present = t.present();
    let mut out = Vec::new();
    let Some(h) = ctx.query("h") else {
        for i in present {
            let c = &t.columns[i];
            if c.default && ts_ok(c.name) {
                out.push(Display { col: i, title: c.name.to_string() });
            }
        }
        return out;
    };
    // Expand wildcards into column names, keeping first-seen order and dropping duplicates.
    let mut wanted: Vec<String> = Vec::new();
    for item in h.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if item.contains('*') {
            for &i in &present {
                let c = &t.columns[i];
                if (glob_match(item, c.name) || c.aliases.iter().any(|a| glob_match(item, a))) && !wanted.iter().any(|w| w == c.name) {
                    wanted.push(c.name.to_string());
                }
            }
        } else if !wanted.iter().any(|w| w == item) {
            wanted.push(item.to_string());
        }
    }
    let pri = ctx.bool_param("pri", false).unwrap_or(false);
    for w in wanted {
        let found = match t.find(&w) {
            Some(i) => Some(Display { col: i, title: w.clone() }),
            None => present
                .iter()
                .find(|&&i| t.columns[i].aliases.contains(&w.as_str()))
                .map(|&i| Display { col: i, title: w.clone() }),
        };
        let Some(d) = found else { continue };
        if !ts_ok(t.columns[d.col].name) {
            continue;
        }
        let sibling = t.columns[d.col].sibling;
        let col_name = t.columns[d.col].name;
        let title = d.title.clone();
        out.push(d);
        if let Some(flag) = sibling
            && pri
            && flag == "pri"
            && let Some(si) = t.find(&format!("{flag}.{col_name}"))
        {
            out.push(Display { col: si, title: format!("{flag}.{title}") });
        }
    }
    out
}

fn row_order(t: &Table, ctx: &Ctx) -> EsResult<Vec<usize>> {
    let mut order: Vec<usize> = (0..t.rows.len()).collect();
    let Some(s) = ctx.query("s") else { return Ok(order) };
    let mut keys: Vec<(usize, bool)> = Vec::new();
    for item in s.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, rev) = if let Some(n) = item.strip_suffix(":desc") {
            (n, true)
        } else if let Some(n) = item.strip_suffix(":asc") {
            (n, false)
        } else {
            (item, false)
        };
        let col = t.find(name).or_else(|| t.present().into_iter().find(|&i| t.columns[i].aliases.contains(&name)));
        match col {
            Some(c) => keys.push((c, rev)),
            None => return Err(EsError::illegal_argument(format!("Unable to sort by unknown sort key `{name}`"))),
        }
    }
    order.sort_by(|&a, &b| {
        for &(c, rev) in &keys {
            let o = t.rows[a][c].compare(&t.rows[b][c]);
            if o != Ordering::Equal {
                return if rev { o.reverse() } else { o };
            }
        }
        Ordering::Equal
    });
    Ok(order)
}

/// Pads `s` to `width` (Java's `RestTable.pad`; the pad count is a signed byte there too).
fn pad(out: &mut String, s: Option<&str>, width: usize, right: bool, last: bool) {
    let len = s.map(|s| s.encode_utf16().count()).unwrap_or(0);
    let left_over = (width.saturating_sub(len)) as u8 as i8;
    if left_over > 0 && right {
        out.extend(std::iter::repeat_n(' ', left_over as usize));
        if let Some(s) = s {
            out.push_str(s);
        }
    } else {
        if let Some(s) = s {
            out.push_str(s);
        }
        if !last && left_over > 0 {
            out.extend(std::iter::repeat_n(' ', left_over as usize));
        }
    }
}

fn help_text(t: &Table, dialect: Dialect) -> String {
    let present = t.present();
    let alias = |c: &Column| c.aliases.join(",");
    let desc = |c: &Column| c.desc.unwrap_or("not available").to_string();
    let mut w = [0usize; 3];
    for &i in &present {
        let c = &t.columns[i];
        w[0] = w[0].max(c.name.encode_utf16().count());
        w[1] = w[1].max(alias(c).encode_utf16().count());
        w[2] = w[2].max(desc(c).encode_utf16().count());
    }
    let _ = dialect;
    let mut out = String::new();
    for &i in &present {
        let c = &t.columns[i];
        pad(&mut out, Some(c.name), w[0], false, false);
        out.push_str(" | ");
        pad(&mut out, Some(&alias(c)), w[1], false, false);
        out.push_str(" | ");
        pad(&mut out, Some(&desc(c)), w[2], false, false);
        out.push('\n');
    }
    out
}

pub const TEXT_PLAIN: &str = "text/plain; charset=UTF-8";

/// Renders a table for the request: `(content type, body)`.
pub fn render(ctx: &Ctx, t: &Table, accept: Option<&MediaType>) -> EsResult<(String, Bytes)> {
    if ctx.bool_param("help", false)? {
        return Ok((TEXT_PLAIN.to_string(), Bytes::from(help_text(t, ctx.cluster.dialect))));
    }
    let bytes = ctx.query("bytes");
    let time = ctx.query("time");
    let structured = match ctx.query("format") {
        Some(f) => XContentType::from_format(f),
        None => accept.and_then(MediaType::xcontent),
    };
    let headers = display_headers(t, ctx);
    let order = row_order(t, ctx)?;
    if let Some(ty) = structured {
        let ty = if ty == XContentType::Smile { XContentType::Json } else { ty };
        let mut rows = Vec::with_capacity(order.len());
        for &r in &order {
            let mut m = Map::new();
            for h in &headers {
                m.insert(h.title.clone(), t.rows[r][h.col].render(bytes, time).map(Value::Str).unwrap_or(Value::Null));
            }
            rows.push(Value::Obj(m));
        }
        let mut v = Value::Arr(rows);
        if let Some(fp) = ctx.query("filter_path") {
            match FilterPath::parse(fp) {
                Ok(Some(f)) => v = f.apply(v),
                Ok(None) => {}
                Err(msg) => return Err(EsError::illegal_argument(msg)),
            }
        }
        let pretty = ctx.query("pretty").is_some_and(|p| p != "false");
        return Ok((ty.content_type(ctx.response_compat), Bytes::from(xcontent::render(&v, ty, pretty))));
    }
    let verbose = ctx.bool_param("v", false)?;
    let rendered: Vec<Vec<Option<String>>> =
        order.iter().map(|&r| headers.iter().map(|h| t.rows[r][h.col].render(bytes, time)).collect()).collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| if verbose { h.title.encode_utf16().count() } else { 0 }).collect();
    for row in &rendered {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.as_deref().map(|s| s.encode_utf16().count()).unwrap_or(0));
        }
    }
    let last = headers.len().saturating_sub(1);
    let mut out = String::new();
    if verbose {
        for (i, h) in headers.iter().enumerate() {
            pad(&mut out, Some(&h.title), widths[i], t.columns[h.col].right, i == last);
            if i != last {
                out.push(' ');
            }
        }
        out.push('\n');
    }
    for row in &rendered {
        for (i, c) in row.iter().enumerate() {
            pad(&mut out, c.as_deref(), widths[i], t.columns[headers[i].col].right, i == last);
            if i != last {
                out.push(' ');
            }
        }
        out.push('\n');
    }
    Ok((TEXT_PLAIN.to_string(), Bytes::from(out)))
}

/// Stats values reported by cat tables. Until the engine reports numbers every counter is
/// zero, every size `0b` and every duration `0s`.
pub mod stats {
    use super::Cell;
    use crate::units::TimeValue;

    /// The zero value of a stats column, typed by its name as Elasticsearch types it.
    pub fn zero(column: &str) -> Cell {
        let base = column.strip_prefix("pri.").unwrap_or(column);
        if base.ends_with("_in_bytes") || base.ends_with("value_count") {
            return Cell::Int(0);
        }
        if base.ends_with("time") {
            return Cell::Time(TimeValue::ZERO);
        }
        if base.ends_with("size") || base.ends_with("memory") || base == "memory.total" || base == "store" || base == "dataset" {
            return Cell::Bytes(0);
        }
        if base.starts_with("seq_no.") {
            return Cell::Int(-1);
        }
        Cell::Int(0)
    }
}

/// `epoch`/`timestamp` cells for tables with time columns.
fn time_cells(now_ms: i64, name: &str) -> Option<Cell> {
    match name {
        "epoch" => Some(Cell::Int(now_ms.div_euclid(1000))),
        "timestamp" => {
            let secs = now_ms.div_euclid(1000).rem_euclid(86_400);
            Some(Cell::str(format!("{:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)))
        }
        _ => None,
    }
}

/// The description of the system index descriptor an index matches (OpenSearch's
/// `system.description` column).
fn system_description(name: &str) -> Option<&'static str> {
    if name.starts_with(".tasks") {
        Some("Task Result Index")
    } else if name.starts_with(".synonyms") {
        Some("Synonyms index for synonym sets")
    } else {
        None
    }
}

fn index_options(ctx: &Ctx, defaults: IndicesOptions) -> EsResult<Vec<String>> {
    let opts = ctx.indices_options(defaults)?;
    let exprs = ctx.param("index").map(crate::meta::resolve::split_expressions).unwrap_or_default();
    crate::meta::resolve::resolve(&ctx.state(), &exprs, &opts, ctx.now_ms)
}

fn table(ctx: &Ctx, columns: &'static [Column]) -> Table {
    let mut t = Table::new(columns);
    t.compat_v8 = ctx.compat == Some(8) || ctx.compat == Some(7);
    t.detailed = ctx.bool_param("detailed", false).unwrap_or(false);
    t
}

fn finish(t: Table) -> EsResult<Reply> {
    Ok(Reply::Table(t))
}

/// `strict_date_time` of a millisecond timestamp.
fn date_string(ms: i64) -> String {
    let f = crate::time::DateFormatter::new("strict_date_time").expect("built-in format");
    f.format_millis(ms)
}

pub fn indices(ctx: &Ctx) -> EsResult<Reply> {
    let health_filter = match ctx.query("health") {
        None => None,
        Some(h @ ("green" | "yellow" | "red")) => Some(h.to_string()),
        Some(other) => return Err(EsError::illegal_argument(format!("unknown cluster health status [{other}]"))),
    };
    let pri_only = ctx.bool_param("pri", false)?;
    let _ = pri_only;
    // OpenSearch's `system=true` lists only system indices (hidden ones too, unless
    // `expand_wildcards` says otherwise).
    let system_only = ctx.cluster.dialect == Dialect::OpenSearch && ctx.bool_param("system", false)?;
    let defaults = if system_only { IndicesOptions { expand_hidden: true, ..IndicesOptions::STRICT_EXPAND } } else { IndicesOptions::STRICT_EXPAND };
    let mut names = index_options(ctx, defaults)?;
    if system_only {
        let st = ctx.state();
        names.retain(|n| st.metadata.indices.get(n).is_some_and(|i| i.system));
    }
    // Rows follow the settings response's `HashMap` of indices.
    let order = crate::json::java_hash_order(&names, 16);
    let names: Vec<String> = order.into_iter().map(|i| names[i].clone()).collect();
    let st = ctx.state();
    let mut t = table(ctx, columns!(ctx, INDICES));
    for n in names {
        let idx = &st.metadata.indices[&n];
        let health = super::indices::index_health(ctx.cluster, &st, idx);
        if health_filter.as_deref().is_some_and(|h| h != health) {
            continue;
        }
        let closed = idx.is_closed();
        t.push_row(|c| match c {
            "health" => Cell::str(health),
            "status" => Cell::str(idx.state.as_str()),
            "index" => Cell::str(&idx.name),
            "uuid" => Cell::str(&idx.uuid),
            "pri" => Cell::Int(idx.number_of_shards() as i64),
            "rep" => Cell::Int(idx.number_of_replicas() as i64),
            "creation.date" => Cell::Int(idx.creation_date()),
            "creation.date.string" => Cell::str(date_string(idx.creation_date())),
            "system" => Cell::str(if idx.system { "true" } else { "false" }),
            "system.description" => match system_description(&idx.name) {
                Some(d) if idx.system => Cell::str(d),
                _ => Cell::Null,
            },
            _ if closed => Cell::Null,
            "docs.count" | "docs.deleted" => Cell::Int(0),
            other => stats::zero(other),
        });
    }
    finish(t)
}

pub fn aliases(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let patterns: Vec<String> = ctx.list_param("name").unwrap_or_default();
    // `GetAliasesRequest`'s default options expand open, closed and hidden indices.
    let opts = ctx.indices_options(IndicesOptions { expand_closed: true, expand_hidden: true, ..IndicesOptions::STRICT_EXPAND_OPEN })?;
    let mut t = table(ctx, columns!(ctx, ALIASES));
    let mut rows: Vec<(&str, &IndexMeta, &crate::meta::AliasMeta)> = Vec::new();
    for idx in st.metadata.indices.values() {
        if !(if idx.is_closed() { opts.expand_closed } else { opts.expand_open }) || (idx.is_hidden() && !opts.expand_hidden) {
            continue;
        }
        for (a, am) in &idx.aliases {
            let wanted = patterns.is_empty() || super::aliases::alias_selected(&patterns, a);
            if !wanted {
                continue;
            }
            rows.push((a.as_str(), idx.as_ref(), am));
        }
    }
    for (alias, idx, am) in rows {
        t.push_row(|c| match c {
            "alias" => Cell::str(alias),
            "index" => Cell::str(&idx.name),
            "filter" => Cell::str(if am.filter.is_some() { "*" } else { "-" }),
            "routing.index" => Cell::str(am.index_routing.as_deref().filter(|s| !s.is_empty()).unwrap_or("-")),
            "routing.search" => Cell::str(am.search_routing.as_deref().filter(|s| !s.is_empty()).unwrap_or("-")),
            "is_write_index" => Cell::str(am.is_write_index.map(|b| b.to_string()).unwrap_or_else(|| "-".into())),
            _ => Cell::Null,
        });
    }
    finish(t)
}

pub fn health(ctx: &Ctx) -> EsResult<Reply> {
    let h = super::cluster::compute_health(ctx.cluster, &ctx.state(), None);
    let mut t = table(ctx, columns!(ctx, HEALTH));
    let name = ctx.cluster.name.clone();
    let now = ctx.now_ms;
    t.push_row(|c| {
        if let Some(cell) = time_cells(now, c) {
            return cell;
        }
        match c {
            "cluster" => Cell::str(&name),
            "status" => Cell::str(h.status.as_str()),
            "node.total" | "node.data" => Cell::Int(1),
            "shards" => Cell::Int(h.active_shards as i64),
            "pri" => Cell::Int(h.active_primary_shards as i64),
            "relo" | "init" | "unassign.pri" | "pending_tasks" => Cell::Int(0),
            "unassign" => Cell::Int(h.unassigned_shards as i64),
            "max_task_wait_time" => Cell::str("-"),
            "active_shards_percent" => Cell::Num(format!("{:.1}%", h.active_shards_percent()), h.active_shards_percent()),
            _ => Cell::Null,
        }
    });
    finish(t)
}

pub fn nodes(ctx: &Ctx) -> EsResult<Reply> {
    let node = &ctx.cluster.node;
    let m = crate::api::nodes::NodeMetrics::sample(ctx.cluster);
    let full_id = ctx.bool_param("full_id", false)?;
    let mut t = table(ctx, columns!(ctx, NODES));
    let id = if full_id { node.id.clone() } else { node.id.chars().take(4).collect() };
    t.push_row(|c| match c {
        "id" => Cell::str(&id),
        "pid" => Cell::Int(m.pid as i64),
        "ip" => Cell::str(&node.ip),
        "port" => Cell::str(node.transport_address.rsplit(':').next().unwrap_or("9300")),
        "http_address" => Cell::str(&node.http_address),
        "version" => Cell::str(crate::version::ES_VERSION),
        "type" => Cell::str("docker"),
        "build" => Cell::str(&crate::version::ES_BUILD_HASH[..7.min(crate::version::ES_BUILD_HASH.len())]),
        "jdk" => Cell::str("27"),
        "disk.total" => Cell::Bytes(m.disk_total),
        "disk.used" => Cell::Bytes(m.disk_total - m.disk_avail),
        "disk.avail" => Cell::Bytes(m.disk_avail),
        "disk.used_percent" => {
            let p = if m.disk_total > 0 { 100.0 * (m.disk_total - m.disk_avail) as f64 / m.disk_total as f64 } else { 0.0 };
            Cell::Num(format!("{p:.2}"), p)
        }
        "heap.current" => Cell::Bytes(m.rss),
        "heap.percent" => Cell::Int(m.heap_percent()),
        "heap.max" => Cell::Bytes(m.heap_max),
        "ram.current" => Cell::Bytes(m.mem_used),
        "ram.percent" => Cell::Int(m.ram_percent()),
        "ram.max" => Cell::Bytes(m.mem_total),
        "file_desc.current" => Cell::Int(m.open_fds),
        "file_desc.percent" => Cell::Int(if m.max_fds > 0 { m.open_fds * 100 / m.max_fds } else { 0 }),
        "file_desc.max" => Cell::Int(m.max_fds),
        "cpu" => Cell::Int(m.cpu_percent),
        "load_1m" => Cell::Num(format!("{:.2}", m.load[0]), m.load[0]),
        "load_5m" => Cell::Num(format!("{:.2}", m.load[1]), m.load[1]),
        "load_15m" => Cell::Num(format!("{:.2}", m.load[2]), m.load[2]),
        "available_processors" => Cell::Int(m.processors as i64),
        "uptime" => Cell::Time(TimeValue::millis(ctx.cluster.uptime_ms() as i64)),
        "node.role" => Cell::str(crate::api::nodes::role_abbreviations(ctx.cluster.dialect)),
        "master" | "cluster_manager" => Cell::str("*"),
        "node.roles" => Cell::str(crate::api::nodes::roles(ctx.cluster.dialect).join(",")),
        "name" => Cell::str(&node.name),
        "shard_stats.total_count" => Cell::Int(super::cluster::compute_health(ctx.cluster, &ctx.state(), None).active_shards as i64),
        "mappings.total_count" => Cell::Int(ctx.state().metadata.indices.len() as i64),
        "mappings.total_estimated_overhead_in_bytes" => Cell::Bytes(ctx.state().metadata.indices.len() as i64 * 1024),
        "script.compilations" | "script.cache_evictions" | "script.compilation_limit_triggered" => Cell::Int(0),
        other => stats::zero(other),
    });
    finish(t)
}

pub fn shards(ctx: &Ctx) -> EsResult<Reply> {
    let names = index_options(ctx, IndicesOptions::STRICT_EXPAND)?;
    let st = ctx.state();
    let node = &ctx.cluster.node;
    let mut t = table(ctx, columns!(ctx, SHARDS));
    let now = ctx.now_ms;
    for n in names {
        let idx = &st.metadata.indices[&n];
        if idx.is_closed() && idx.number_of_replicas() == 0 && false {
            continue;
        }
        for shard in 0..idx.number_of_shards() {
            for copy in 0..=idx.number_of_replicas() {
                let primary = copy == 0;
                let assigned = primary;
                let created = idx.creation_date();
                t.push_row(|c| match c {
                    "index" => Cell::str(&idx.name),
                    "shard" => Cell::Int(shard as i64),
                    "prirep" => Cell::str(if primary { "p" } else { "r" }),
                    "state" => Cell::str(if assigned { "STARTED" } else { "UNASSIGNED" }),
                    "ip" if assigned => Cell::str(&node.ip),
                    "id" if assigned => Cell::str(&node.id),
                    "node" if assigned => Cell::str(&node.name),
                    "unassigned.reason" if !assigned => Cell::str("INDEX_CREATED"),
                    "unassigned.at" if !assigned => Cell::str(date_string(created)),
                    "unassigned.for" if !assigned => Cell::Time(TimeValue::millis((now - created).max(0))),
                    "recoverysource.type" if !assigned => Cell::str("peer"),
                    "path.data" | "path.state" if assigned => Cell::str("/data"),
                    "sync_id" | "unassigned.details" => Cell::Null,
                    "unassigned.reason" | "unassigned.at" | "unassigned.for" | "recoverysource.type" => Cell::Null,
                    _ if !assigned || idx.is_closed() => Cell::Null,
                    "docs" => Cell::Int(0),
                    "store" | "dataset" => Cell::Bytes(0),
                    other => stats::zero(other),
                });
            }
        }
    }
    finish(t)
}

pub fn templates(ctx: &Ctx) -> EsResult<Reply> {
    if ctx.param("name").is_some_and(|n| n.contains(',')) {
        return Err(EsError::illegal_argument("template name may not contain ','"));
    }
    let st = ctx.state();
    let pattern = ctx.param("name").map(str::to_string);
    let selected = |n: &str| pattern.as_deref().is_none_or(|p| glob_match(p, n));
    let mut t = table(ctx, columns!(ctx, TEMPLATES));
    for (name, tpl) in &st.metadata.templates {
        if !selected(name) {
            continue;
        }
        t.push_row(|c| match c {
            "name" => Cell::str(name),
            "index_patterns" => Cell::str(format!("[{}]", tpl.index_patterns.join(", "))),
            "order" => Cell::Int(tpl.order),
            "version" => tpl.version.map(Cell::Int).unwrap_or(Cell::Null),
            "composed_of" => Cell::str(""),
            _ => Cell::Null,
        });
    }
    for (name, tpl) in &st.metadata.index_templates {
        if !selected(name) {
            continue;
        }
        t.push_row(|c| match c {
            "name" => Cell::str(name),
            "index_patterns" => Cell::str(format!("[{}]", tpl.index_patterns.join(", "))),
            "order" => Cell::Int(tpl.priority_or_zero()),
            "version" => tpl.version.map(Cell::Int).unwrap_or(Cell::Null),
            "composed_of" => Cell::str(format!("[{}]", tpl.composed_of.join(", "))),
            _ => Cell::Null,
        });
    }
    finish(t)
}

pub fn count(ctx: &Ctx) -> EsResult<Reply> {
    // Resolving validates the index expressions; documents arrive with the engine.
    index_options(ctx, IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED)?;
    let mut t = table(ctx, columns!(ctx, COUNT));
    let now = ctx.now_ms;
    t.push_row(|c| time_cells(now, c).unwrap_or(Cell::Int(0)));
    finish(t)
}

pub fn master(ctx: &Ctx) -> EsResult<Reply> {
    let node = &ctx.cluster.node;
    let mut t = table(ctx, columns!(ctx, MASTER, CLUSTER_MANAGER));
    t.push_row(|c| match c {
        "id" => Cell::str(&node.id),
        "host" => Cell::str(&node.host),
        "ip" => Cell::str(&node.ip),
        "node" => Cell::str(&node.name),
        _ => Cell::Null,
    });
    finish(t)
}

pub fn allocation(ctx: &Ctx) -> EsResult<Reply> {
    let node = &ctx.cluster.node;
    if let Some(ids) = ctx.list_param("node_id")
        && !super::nodes::selects_node(ctx, &ids)
    {
        return finish(table(ctx, columns!(ctx, ALLOCATION)));
    }
    let h = super::cluster::compute_health(ctx.cluster, &ctx.state(), None);
    let m = crate::api::nodes::NodeMetrics::sample(ctx.cluster);
    let mut t = table(ctx, columns!(ctx, ALLOCATION));
    t.push_row(|c| match c {
        "shards" => Cell::Int(h.active_shards as i64),
        "shards.undesired" => Cell::Int(0),
        "write_load.forecast" => Cell::Num("0.0".into(), 0.0),
        "disk.indices.forecast" | "disk.indices" => Cell::Bytes(0),
        "disk.used" => Cell::Bytes(m.disk_total - m.disk_avail),
        "disk.avail" => Cell::Bytes(m.disk_avail),
        "disk.total" => Cell::Bytes(m.disk_total),
        "disk.percent" => Cell::Int(if m.disk_total > 0 { (100 * (m.disk_total - m.disk_avail) / m.disk_total).max(0) } else { 0 }),
        "host" => Cell::str(&node.host),
        "ip" => Cell::str(&node.ip),
        "node" => Cell::str(&node.name),
        "node.role" => Cell::str(crate::api::nodes::role_abbreviations(ctx.cluster.dialect)),
        _ => Cell::Null,
    });
    if h.unassigned_shards > 0 {
        t.push_row(|c| match c {
            "shards" => Cell::Int(h.unassigned_shards as i64),
            "node" => Cell::str("UNASSIGNED"),
            _ => Cell::Null,
        });
    }
    finish(t)
}

pub fn pending_tasks(ctx: &Ctx) -> EsResult<Reply> {
    finish(table(ctx, columns!(ctx, PENDING_TASKS)))
}

pub fn segments(ctx: &Ctx) -> EsResult<Reply> {
    index_options(ctx, IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED)?;
    finish(table(ctx, columns!(ctx, SEGMENTS)))
}

pub fn recovery(ctx: &Ctx) -> EsResult<Reply> {
    let names = index_options(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    let active_only = ctx.bool_param("active_only", false)?;
    let st = ctx.state();
    let node = &ctx.cluster.node;
    let mut t = table(ctx, columns!(ctx, RECOVERY));
    if !active_only {
        for n in names {
            let idx = &st.metadata.indices[&n];
            let closed = idx.is_closed();
            for shard in 0..idx.number_of_shards() {
                t.push_row(|c| match c {
                    "index" => Cell::str(&idx.name),
                    "shard" => Cell::Int(shard as i64),
                    "start_time" => Cell::Int(idx.creation_date()),
                    "start_time_millis" => Cell::Int(idx.creation_date()),
                    "stop_time" | "stop_time_millis" => Cell::Int(idx.creation_date()),
                    "time" => Cell::Time(TimeValue::ZERO),
                    "type" => Cell::str(if closed { "existing_store" } else { "empty_store" }),
                    "stage" => Cell::str("done"),
                    "source_host" | "source_node" | "repository" | "snapshot" => Cell::str("n/a"),
                    "target_host" => Cell::str(&node.ip),
                    "target_node" => Cell::str(&node.name),
                    "files" | "files_recovered" | "files_total" | "translog_ops" | "translog_ops_recovered" => Cell::Int(0),
                    "files_percent" | "bytes_percent" => Cell::str("0.0%"),
                    "translog_ops_percent" => Cell::str("100.0%"),
                    "bytes" | "bytes_recovered" | "bytes_total" => Cell::Bytes(0),
                    _ => Cell::Null,
                });
            }
        }
    }
    finish(t)
}

/// One thread pool as a node reports it.
pub struct ThreadPool {
    pub name: &'static str,
    pub fixed: bool,
    /// OpenSearch's search pools are `resizable` (fixed, with a size that can change).
    pub resizable: bool,
    /// `size` of fixed pools, `max` of scaling ones.
    pub size: i64,
    pub core: i64,
    pub queue_size: i64,
    pub keep_alive: &'static str,
}

/// The thread pools of a node with `processors` processors, sized with Elasticsearch's formulas
/// (`ThreadPool.Names`, `DefaultBuiltInExecutorBuilders`).
pub fn thread_pools(processors: usize) -> Vec<ThreadPool> {
    thread_pools_for(Dialect::Elasticsearch, processors)
}

/// The thread pools of a node of a dialect (OpenSearch's set and formulas differ).
pub fn thread_pools_for(d: Dialect, processors: usize) -> Vec<ThreadPool> {
    let p = processors.max(1) as i64;
    let half = (p + 1) / 2;
    let half_max = |m: i64| half.clamp(1, m);
    let fixed = |name, size: i64, queue| ThreadPool { name, fixed: true, resizable: false, size, core: size, queue_size: queue, keep_alive: "" };
    let resizable = |name, size: i64, queue| ThreadPool { name, fixed: true, resizable: true, size, core: size, queue_size: queue, keep_alive: "" };
    let scaling = |name, core, max: i64, keep_alive| ThreadPool { name, fixed: false, resizable: false, size: max, core, queue_size: -1, keep_alive };
    if d == Dialect::OpenSearch {
        return vec![
            fixed("analyze", 1, 16),
            scaling("fetch_shard_started", 1, 2 * p, "5m"),
            scaling("fetch_shard_store", 1, 2 * p, "5m"),
            scaling("flush", 1, half_max(5), "5m"),
            fixed("force_merge", (p / 8).max(1), -1),
            scaling("generic", 4, (4 * p).clamp(128, 512), "30s"),
            fixed("get", p, 1000),
            resizable("index_searcher", 2 * p, 1000),
            fixed("listener", half_max(10), -1),
            scaling("management", 1, 5, "5m"),
            scaling("merge", 1, p, "5m"),
            scaling("refresh", 1, half_max(10), "5m"),
            scaling("remote_download", 1, 2 * p, "5m"),
            scaling("remote_purge", 1, half_max(10), "5m"),
            scaling("remote_recovery", 1, 2 * p, "5m"),
            scaling("remote_refresh_retry", 1, half_max(10), "5m"),
            fixed("remote_state_checksum", half + 2, 1000),
            fixed("remote_state_read", (4 * p).clamp(4, 32), 120000),
            resizable("search", p * 3 / 2 + 1, 1000),
            resizable("search_throttled", 1, 100),
            scaling("snapshot", 1, half_max(5), "5m"),
            scaling("snapshot_deletion", 1, 4 * p, "5m"),
            resizable("stream_search", p * 3 / 2 + 1, 1000),
            fixed("system_read", half_max(5), 2000),
            fixed("system_write", half_max(5), 1000),
            fixed("translog_recovery", p, -1),
            fixed("translog_sync", 4 * p, 10000),
            scaling("translog_transfer", 1, half_max(10), "5m"),
            scaling("warmer", 1, half_max(5), "5m"),
            fixed("write", p, 10000),
        ];
    }
    vec![
        fixed("analyze", 1, 16),
        fixed("auto_complete", (p / 4).max(1), 100),
        fixed("cluster_coordination", 1, -1),
        scaling("fetch_shard_started", 1, 2 * p, "5m"),
        scaling("fetch_shard_store", 1, 2 * p, "5m"),
        scaling("flush", 1, half_max(5), "5m"),
        fixed("force_merge", (p / 8).max(1), -1),
        scaling("generic", 4, (4 * p).clamp(128, 512), "30s"),
        fixed("get", p * 3 / 2 + 1, 1000),
        scaling("management", 1, p.min(5), "5m"),
        scaling("merge", 1, p, "5m"),
        scaling("refresh", 1, half_max(10), "5m"),
        fixed("search", p * 3 / 2 + 1, 1000),
        fixed("search_coordination", half, 1000),
        scaling("snapshot", 1, half_max(10), "5m"),
        scaling("snapshot_meta", 1, (3 * p).min(50), "30s"),
        fixed("system_critical_read", half_max(5), 2000),
        fixed("system_critical_write", half_max(5), 1500),
        fixed("system_read", half_max(5), 2000),
        fixed("system_write", half_max(5), 1000),
        fixed("system_write_coordination", half_max(5), 1000),
        scaling("warmer", 1, half_max(5), "5m"),
        fixed("write", p, 10000),
        fixed("write_coordination", p, 10000),
    ]
}

pub fn thread_pool(ctx: &Ctx) -> EsResult<Reply> {
    let node = &ctx.cluster.node;
    let patterns = ctx.list_param("thread_pool_patterns");
    let m = crate::api::nodes::NodeMetrics::sample(ctx.cluster);
    let mut t = table(ctx, columns!(ctx, THREAD_POOL));
    for tp in thread_pools_for(ctx.cluster.dialect, m.processors) {
        if let Some(p) = &patterns
            && !p.iter().any(|p| glob_match(p, tp.name))
        {
            continue;
        }
        let ty = if tp.resizable {
            "resizable"
        } else if tp.fixed {
            "fixed"
        } else {
            "scaling"
        };
        // OpenSearch times waits in its search pools (`-1`: not tracked).
        let waits = matches!(tp.name, "search" | "search_throttled" | "index_searcher" | "stream_search");
        let name = tp.name;
        t.push_row(|c| match c {
            "node_name" => Cell::str(&node.name),
            "node_id" => Cell::str(&node.id),
            "ephemeral_node_id" => Cell::str(&node.ephemeral_id),
            "pid" => Cell::Int(m.pid as i64),
            "host" => Cell::str(&node.host),
            "ip" => Cell::str(&node.ip),
            "port" => Cell::Int(9300),
            "name" => Cell::str(name),
            "type" => Cell::str(ty),
            "active" | "queue" | "rejected" | "largest" | "completed" => Cell::Int(0),
            "pool_size" => Cell::Int(0),
            "total_wait_time" => {
                if waits {
                    Cell::Time(TimeValue::millis(0))
                } else {
                    Cell::Time(TimeValue::MINUS_ONE)
                }
            }
            "queue_size" => Cell::Int(tp.queue_size),
            "core" => if tp.fixed { Cell::Null } else { Cell::Int(tp.core) },
            "max" => if tp.fixed { Cell::Null } else { Cell::Int(tp.size) },
            "size" => if tp.fixed { Cell::Int(tp.size) } else { Cell::Null },
            "keep_alive" => {
                if tp.fixed {
                    Cell::Null
                } else {
                    TimeValue::parse(tp.keep_alive, "keep_alive").map(Cell::Time).unwrap_or(Cell::Null)
                }
            }
            _ => Cell::Null,
        });
    }
    finish(t)
}

/// `_cat/segment_replication` (OpenSearch): no index replicates segments.
pub fn segment_replication(ctx: &Ctx) -> EsResult<Reply> {
    finish(table(ctx, cols_os::SEGMENT_REPLICATION))
}

pub fn plugins(ctx: &Ctx) -> EsResult<Reply> {
    finish(table(ctx, columns!(ctx, PLUGINS)))
}

pub fn nodeattrs(ctx: &Ctx) -> EsResult<Reply> {
    let node = &ctx.cluster.node;
    let m = crate::api::nodes::NodeMetrics::sample(ctx.cluster);
    let mut t = table(ctx, columns!(ctx, NODEATTRS));
    for (k, v) in crate::api::nodes::node_attributes(ctx.cluster, &m) {
        t.push_row(|c| match c {
            "node" => Cell::str(&node.name),
            "id" => Cell::str(&node.id),
            "pid" => Cell::Int(m.pid as i64),
            "host" => Cell::str(&node.host),
            "ip" => Cell::str(&node.ip),
            "port" => Cell::Int(9300),
            "attr" => Cell::str(&k),
            "value" => Cell::str(&v),
            _ => Cell::Null,
        });
    }
    finish(t)
}

pub fn fielddata(ctx: &Ctx) -> EsResult<Reply> {
    finish(table(ctx, columns!(ctx, FIELDDATA)))
}

/// `_cat/tasks`: the node runs no background tasks between requests, so the list holds the
/// listing request itself, as on an idle Elasticsearch node.
pub fn tasks(ctx: &Ctx) -> EsResult<Reply> {
    let node = &ctx.cluster.node;
    let mut t = table(ctx, columns!(ctx, TASKS));
    let now = ctx.now_ms;
    let opaque = ctx.request.header("x-opaque-id").map(str::to_string);
    let id = (now.rem_euclid(1_000_000)) as u64;
    t.push_row(|c| {
        if let Some(cell) = time_cells(now, c) {
            return match c {
                "timestamp" => cell,
                _ => Cell::Null,
            };
        }
        match c {
            "id" | "task_id" => Cell::str(format!("{}:{id}", node.id)),
            "action" => Cell::str("cluster:monitor/tasks/lists"),
            "parent_task_id" => Cell::str("-"),
            "type" => Cell::str("transport"),
            "start_time" => Cell::Int(now),
            "running_time" => Cell::Time(TimeValue { duration: 100, unit: crate::units::TimeUnit::Micros }),
            "running_time_ns" => Cell::Int(100_000),
            "x_opaque_id" => Cell::opt_str(opaque.clone()),
            "ip" => Cell::str(&node.ip),
            "port" => Cell::Int(9300),
            "node" => Cell::str(&node.name),
            "version" => Cell::str(crate::version::ES_VERSION),
            "description" => Cell::str(""),
            _ => Cell::Null,
        }
    });
    finish(t)
}

/// `_cat/component_templates`.
pub fn component_templates(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let pattern = ctx.param("name").map(str::to_string);
    let mut t = table(ctx, cols::COMPONENT_TEMPLATES);
    for (name, ct) in &st.metadata.component_templates {
        if pattern.as_deref().is_some_and(|p| !glob_match(p, name)) {
            continue;
        }
        // `RestCatComponentTemplateAction.countMappingInTemplate`: entries of each top-level
        // mapping value.
        let mapping_count: i64 = ct
            .template
            .mappings
            .as_ref()
            .map(|m| {
                m.values()
                    .map(|v| match v {
                        Value::Obj(o) => o.len() as i64,
                        Value::Arr(a) => a.len() as i64,
                        _ => 1,
                    })
                    .sum()
            })
            .unwrap_or(0);
        let users: Vec<&String> = st.metadata.index_templates.iter().filter(|(_, it)| it.composed_of.iter().any(|c| c == name)).map(|(n, _)| n).collect();
        let order = crate::json::java_hash_order(&users, 16);
        let included: Vec<&str> = order.into_iter().map(|i| users[i].as_str()).collect();
        t.push_row(|c| match c {
            "name" => Cell::str(name),
            "version" => ct.version.map(Cell::Int).unwrap_or(Cell::Null),
            "alias_count" => Cell::Int(ct.template.aliases.as_ref().map(|a| a.len() as i64).unwrap_or(0)),
            "mapping_count" => Cell::Int(mapping_count),
            "settings_count" => Cell::Int(ct.template.settings.as_ref().map(|s| s.len() as i64).unwrap_or(0)),
            "metadata_count" => Cell::Int(ct.meta.as_ref().map(|m| m.len() as i64).unwrap_or(0)),
            "included_in" => Cell::str(format!("[{}]", included.join(", "))),
            _ => Cell::Null,
        });
    }
    finish(t)
}

/// `_cat/circuit_breaker`: the node's breakers (limits from its memory, nothing tripped).
pub fn circuit_breaker(ctx: &Ctx) -> EsResult<Reply> {
    let node = &ctx.cluster.node;
    let m = crate::api::nodes::NodeMetrics::sample(ctx.cluster);
    let patterns = ctx.list_param("circuit_breaker_patterns");
    let mut t = table(ctx, cols::CIRCUIT_BREAKER);
    for (name, limit, overhead) in crate::api::nodes::breakers(&m) {
        if let Some(p) = &patterns
            && !p.iter().any(|p| glob_match(p, name))
        {
            continue;
        }
        t.push_row(|c| match c {
            "node_id" => Cell::str(&node.id),
            "node_name" => Cell::str(&node.name),
            "breaker" => Cell::str(name),
            "limit" => Cell::Bytes(limit),
            "limit_bytes" => Cell::Int(limit),
            "estimated" => Cell::Bytes(0),
            "estimated_bytes" => Cell::Int(0),
            "tripped" => Cell::Int(0),
            "overhead" => Cell::Num(crate::json::write::java_double(overhead), overhead),
            _ => Cell::Null,
        });
    }
    finish(t)
}

/// `_cat/repositories`: no snapshot repositories (snapshots aren't supported).
pub fn repositories(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let mut t = table(ctx, columns!(ctx, REPOSITORIES));
    for (name, ty) in super::snapshots::repository_rows(&st) {
        t.push_row(|c| match c {
            "id" => Cell::str(&name),
            "type" => Cell::str(&ty),
            _ => Cell::Null,
        });
    }
    finish(t)
}

/// `_cat/snapshots/{repository}`: every named repository is missing.
pub fn snapshots(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let rows = super::snapshots::snapshot_rows(&st, &ctx.list_param("repository").unwrap_or_default())?;
    let mut t = table(ctx, columns!(ctx, SNAPSHOTS));
    let hms = |ms: i64| crate::time::DateFormatter::new("HH:mm:ss").map(|f| f.format_millis(ms)).unwrap_or_default();
    for (repo, s) in rows {
        let failed = s.total_shards - s.successful_shards;
        t.push_row(|c| match c {
            "id" => Cell::str(&s.name),
            "repository" => Cell::str(&repo),
            "status" => Cell::str(&s.state),
            "start_epoch" => Cell::Int(s.start_ms / 1000),
            "start_time" => Cell::str(hms(s.start_ms)),
            "end_epoch" => Cell::Int(s.end_ms / 1000),
            "end_time" => Cell::str(hms(s.end_ms)),
            "duration" => Cell::Time(crate::units::TimeValue::millis((s.end_ms - s.start_ms).max(0))),
            "indices" => Cell::Int(s.indices.len() as i64),
            "successful_shards" => Cell::Int(s.successful_shards),
            "failed_shards" => Cell::Int(failed),
            "total_shards" => Cell::Int(s.total_shards),
            "reason" => Cell::Null,
            _ => Cell::Null,
        });
    }
    finish(t)
}

/// `GET _cat`: the list of cat endpoints.
pub fn help(_ctx: &Ctx) -> EsResult<Reply> {
    const TEXT: &str = "=^.^=\n/_cat/allocation\n/_cat/shards\n/_cat/shards/{index}\n/_cat/master\n/_cat/nodes\n/_cat/tasks\n/_cat/indices\n/_cat/indices/{index}\n/_cat/segments\n/_cat/segments/{index}\n/_cat/count\n/_cat/count/{index}\n/_cat/recovery\n/_cat/recovery/{index}\n/_cat/health\n/_cat/pending_tasks\n/_cat/aliases\n/_cat/aliases/{alias}\n/_cat/thread_pool\n/_cat/thread_pool/{thread_pools}\n/_cat/circuit_breaker\n/_cat/circuit_breaker/{circuit_breaker_patterns}\n/_cat/plugins\n/_cat/fielddata\n/_cat/fielddata/{fields}\n/_cat/nodeattrs\n/_cat/repositories\n/_cat/snapshots/{repository}\n/_cat/templates\n/_cat/component_templates\n/_cat/ml/anomaly_detectors\n/_cat/ml/anomaly_detectors/{job_id}\n/_cat/ml/datafeeds\n/_cat/ml/datafeeds/{datafeed_id}\n/_cat/ml/trained_models\n/_cat/ml/trained_models/{model_id}\n/_cat/ml/data_frame/analytics\n/_cat/ml/data_frame/analytics/{id}\n/_cat/transforms\n/_cat/transforms/{transform_id}\n";
    Ok(Reply::Raw(200, TEXT_PLAIN.to_string(), Bytes::from_static(TEXT.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_are_complete() {
        for (name, t) in [("indices", cols::INDICES), ("nodes", cols::NODES), ("shards", cols::SHARDS), ("health", cols::HEALTH)] {
            assert!(!t.is_empty(), "{name}");
            for c in t {
                if let Some(s) = c.sibling {
                    assert!(t.iter().any(|o| o.name == format!("{s}.{}", c.name)), "{name}: sibling of {}", c.name);
                }
            }
        }
    }

    #[test]
    fn cells_render_like_rest_table() {
        assert_eq!(Cell::Bytes(1536).render(None, None).unwrap(), "1.5kb");
        assert_eq!(Cell::Bytes(1536).render(Some("b"), None).unwrap(), "1536");
        assert_eq!(Cell::Bytes(1536).render(Some("kb"), None).unwrap(), "1");
        assert_eq!(Cell::Time(TimeValue::millis(1500)).render(None, None).unwrap(), "1.5s");
        assert_eq!(Cell::Time(TimeValue::millis(1500)).render(None, Some("ms")).unwrap(), "1500");
        assert_eq!(Cell::Null.render(None, None), None);
        assert_eq!(Cell::Int(2).compare(&Cell::Int(10)), Ordering::Less);
        assert_eq!(Cell::str("2").compare(&Cell::str("10")), Ordering::Greater);
        assert_eq!(Cell::Null.compare(&Cell::Int(0)), Ordering::Less);
    }

    #[test]
    fn padding() {
        let mut s = String::new();
        pad(&mut s, Some("ab"), 4, true, false);
        pad(&mut s, Some("c"), 3, false, false);
        pad(&mut s, Some("d"), 3, false, true);
        assert_eq!(s, "  abc  d");
    }

    #[test]
    fn stat_zeros() {
        assert_eq!(stats::zero("pri.store.size"), Cell::Bytes(0));
        assert_eq!(stats::zero("get.exists_time"), Cell::Time(TimeValue::ZERO));
        assert_eq!(stats::zero("bulk.total_size_in_bytes"), Cell::Int(0));
        assert_eq!(stats::zero("segments.memory"), Cell::Bytes(0));
        assert_eq!(stats::zero("refresh.total"), Cell::Int(0));
        assert_eq!(stats::zero("memory.total"), Cell::Bytes(0));
    }
}
