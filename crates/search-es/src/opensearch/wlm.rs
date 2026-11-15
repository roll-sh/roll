//! `GET _list/wlm_stats`: OpenSearch's workload management statistics as a paginated table
//! (`RestWlmStatsAction`'s tabular mode).
//!
//! roll runs every request of a cluster in one workload group, OpenSearch's
//! `DEFAULT_WORKLOAD_GROUP`, so the table has one row per node and group (one here) and then
//! the "No more pages available" footer OpenSearch prints on the last page. Completions are the
//! cluster's shard query phases; there is no admission control, so rejections and
//! cancellations are zero, and resource usage is reported as `0.0` (a shared node must not
//! leak its host's figures, the same rule node stats follow).
//!
//! The table is not a `_cat` table (no `h`, `s` or `help`, and a `|` column between every two
//! columns, which the JSON form repeats as duplicate `"|"` keys), so it is rendered here.

use crate::error::{EsError, EsResult};
use crate::rest::{Ctx, Reply};
use bytes::Bytes;
use std::sync::atomic::Ordering;

const MAX_PAGE_SIZE: i64 = 100;
const DEFAULT_PAGE_SIZE: i64 = 10;

const HEADERS: [&str; 7] = ["NODE_ID", "WORKLOAD_GROUP_ID", "TOTAL_COMPLETIONS", "TOTAL_REJECTIONS", "TOTAL_CANCELLATIONS", "CPU_USAGE", "MEMORY_USAGE"];

fn parse_exception(reason: &str) -> EsError {
    EsError::new(400, "parse_exception", reason)
}

pub fn list(ctx: &Ctx) -> EsResult<Reply> {
    // `prepareRequest` reads the page size, then the sort field, then the order.
    let size = ctx.int_param("size", DEFAULT_PAGE_SIZE)?;
    if size <= 0 || size > MAX_PAGE_SIZE {
        return Err(parse_exception(&format!("Invalid value for 'size'. Allowed range: 1 to {MAX_PAGE_SIZE}")));
    }
    let sort = ctx.query("sort").unwrap_or("node_id");
    if !sort.eq_ignore_ascii_case("node_id") && !sort.eq_ignore_ascii_case("workload_group") {
        return Err(parse_exception("Invalid value for 'sort'. Allowed: 'node_id', 'workload_group'")
            .caused_by(EsError::illegal_argument(format!("Invalid sort field: {sort}. Allowed values: workload_group, node_id"))));
    }
    let order = ctx.query("order").unwrap_or("asc");
    if !order.eq_ignore_ascii_case("asc") && !order.eq_ignore_ascii_case("desc") {
        return Err(parse_exception("Invalid value for 'order'. Allowed: 'asc', 'desc'")
            .caused_by(EsError::illegal_argument(format!("Invalid sort order: {order}. Allowed values: asc, desc"))));
    }
    let verbose = ctx.bool_param("v", false)?;
    if let Some(token) = ctx.query("next_token") {
        // This node never hands out a next token (one row always fits a page), so any token is
        // one OpenSearch's pagination strategy can't resume from.
        let _ = token;
        let body = crate::obj! {
            "error" => "Pagination state has changed (e.g., new workload groups added or removed). Please restart pagination from the beginning by omitting the 'next_token' parameter.",
            "details" => "Invalid pagination token format",
        };
        return Ok(Reply::Raw(400, "application/json; charset=UTF-8".into(), Bytes::from(body.to_json())));
    }
    let st = ctx.state();
    let completions: u64 = st
        .metadata
        .indices
        .values()
        .filter_map(|i| ctx.cluster.engine.loaded(&i.uuid))
        .flat_map(|e| e.shards.clone())
        .map(|s| s.counters.query_total.load(Ordering::Relaxed))
        .sum();
    let rows: Vec<[String; 7]> = vec![
        [ctx.cluster.node.id.clone(), "DEFAULT_WORKLOAD_GROUP".into(), completions.to_string(), "0".into(), "0".into(), "0.0".into(), "0.0".into()],
        // The last page's footer.
        ["No more pages available".into(), "-".into(), "-".into(), "-".into(), "-".into(), "-".into(), "-".into()],
    ];
    let format = ctx.query("format").map(str::to_ascii_lowercase);
    let pretty = ctx.query("pretty").is_some_and(|p| p != "false");
    match format.as_deref() {
        Some("json") | Some("smile") | Some("cbor") => Ok(Reply::Raw(200, "application/json; charset=UTF-8".into(), Bytes::from(json_table(&rows, pretty)))),
        Some("yaml") => Ok(Reply::Raw(200, "application/yaml".into(), Bytes::from(yaml_table(&rows)))),
        _ => Ok(Reply::Raw(200, crate::api::cat::TEXT_PLAIN.into(), Bytes::from(text_table(&rows, verbose)))),
    }
}

/// The cells of a row with the `|` separator columns between them.
fn with_bars(cells: &[String; 7]) -> impl Iterator<Item = (&'static str, &str)> {
    HEADERS.iter().zip(cells.iter()).enumerate().flat_map(|(i, (h, c))| {
        let bar = (i > 0).then_some(("|", "|"));
        bar.into_iter().chain(std::iter::once((*h, c.as_str())))
    })
}

/// `RestTable.buildTextPlainResponse`: columns padded to their widest cell (the header too
/// with `v`), one space apart, the last column unpadded.
fn text_table(rows: &[[String; 7]], verbose: bool) -> String {
    let header: [String; 7] = HEADERS.map(String::from);
    let cols: Vec<Vec<&str>> = std::iter::once(&header).chain(rows.iter()).map(|r| with_bars(r).map(|(_, c)| c).collect()).collect();
    let n = cols[0].len();
    let mut widths = vec![0usize; n];
    for (ri, r) in cols.iter().enumerate() {
        if ri == 0 && !verbose {
            continue;
        }
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.encode_utf16().count());
        }
    }
    let mut out = String::new();
    for (ri, r) in cols.iter().enumerate() {
        if ri == 0 && !verbose {
            continue;
        }
        for (i, c) in r.iter().enumerate() {
            out.push_str(c);
            if i + 1 < n {
                out.extend(std::iter::repeat_n(' ', widths[i] - c.encode_utf16().count()));
                out.push(' ');
            }
        }
        out.push('\n');
    }
    out
}

fn json_str(s: &str) -> String {
    crate::json::Value::Str(s.to_string()).to_json()
}

/// The JSON form: one object per row, every cell a string, the `|` keys repeated as
/// OpenSearch's XContent writer repeats them.
fn json_table(rows: &[[String; 7]], pretty: bool) -> String {
    let mut out = String::from("[");
    for (ri, r) in rows.iter().enumerate() {
        if ri > 0 {
            out.push(',');
        }
        out.push_str(if pretty { "\n  {" } else { "{" });
        for (i, (k, v)) in with_bars(r).enumerate() {
            if i > 0 {
                out.push(',');
            }
            if pretty {
                out.push_str("\n    ");
                out.push_str(&json_str(k));
                out.push_str(" : ");
            } else {
                out.push_str(&json_str(k));
                out.push(':');
            }
            out.push_str(&json_str(v));
        }
        out.push_str(if pretty { "\n  }" } else { "}" });
    }
    out.push_str(if pretty { "\n]\n" } else { "]" });
    out
}

fn yaml_table(rows: &[[String; 7]]) -> String {
    let mut out = String::from("---\n");
    for r in rows {
        for (i, (k, v)) in with_bars(r).enumerate() {
            out.push_str(if i == 0 { "- " } else { "  " });
            out.push_str(if k == "|" { "'|'" } else { k });
            out.push_str(": ");
            out.push_str(&json_str(v));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<[String; 7]> {
        vec![
            ["AUhQ2AO0RPKsAndJYN3F6w".into(), "DEFAULT_WORKLOAD_GROUP".into(), "105".into(), "0".into(), "0".into(), "0.04867839813232422".into(), "0.005534251686185598".into()],
            ["No more pages available".into(), "-".into(), "-".into(), "-".into(), "-".into(), "-".into(), "-".into()],
        ]
    }

    // OpenSearch 3.9.0's output for the same rows.
    #[test]
    fn text_like_opensearch() {
        assert_eq!(
            text_table(&rows(), true),
            "NODE_ID                 | WORKLOAD_GROUP_ID      | TOTAL_COMPLETIONS | TOTAL_REJECTIONS | TOTAL_CANCELLATIONS | CPU_USAGE           | MEMORY_USAGE\n\
             AUhQ2AO0RPKsAndJYN3F6w  | DEFAULT_WORKLOAD_GROUP | 105               | 0                | 0                   | 0.04867839813232422 | 0.005534251686185598\n\
             No more pages available | -                      | -                 | -                | -                   | -                   | -\n"
        );
        assert_eq!(
            text_table(&rows(), false),
            "AUhQ2AO0RPKsAndJYN3F6w  | DEFAULT_WORKLOAD_GROUP | 105 | 0 | 0 | 0.04867839813232422 | 0.005534251686185598\n\
             No more pages available | -                      | -   | - | - | -                   | -\n"
        );
    }

    #[test]
    fn json_repeats_the_bar_keys() {
        let j = json_table(&rows()[1..], false);
        assert_eq!(
            j,
            r#"[{"NODE_ID":"No more pages available","|":"|","WORKLOAD_GROUP_ID":"-","|":"|","TOTAL_COMPLETIONS":"-","|":"|","TOTAL_REJECTIONS":"-","|":"|","TOTAL_CANCELLATIONS":"-","|":"|","CPU_USAGE":"-","|":"|","MEMORY_USAGE":"-"}]"#
        );
        assert!(json_table(&rows(), true).starts_with("[\n  {\n    \"NODE_ID\" : \"AUhQ2AO0RPKsAndJYN3F6w\",\n    \"|\" : \"|\","));
        assert!(yaml_table(&rows()[1..]).starts_with("---\n- NODE_ID: \"No more pages available\"\n  '|': \"|\"\n"));
    }
}
