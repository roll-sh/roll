//! Cassandra SASI (`org.apache.cassandra.index.sasi.SASIIndex`): option validation
//! (`SASIIndex.validateOptions`, `IndexMode`, the analyzers' `validate`), which operators an index
//! serves (`ColumnIndex.supports`), the `LIKE` forms (`LikeRestriction.makeSpecific`) and how a
//! value matches (`Expression.isSatisfiedBy` through the index's analyzer). Ported from Apache
//! Cassandra 5.0 (Apache-2.0).
//!
//! SASI has no index table here. A query with an `=` or `LIKE` on a literal (text) SASI column
//! scans the base table ([`Scan`]); once values are bound, [`Scan`] replaces those restrictions in
//! the row filter with [`Matcher`]s that compare *analyzed* terms (case folding, tokens), so the
//! results are exactly the rows SASI's post-filter accepts. Numeric SASI restrictions (`=`,
//! ranges) are plain comparisons and stay in the core's row filter.
//!
//! Analyzers: `NoOpAnalyzer`, `NonTokenizingAnalyzer` (case options), `DelimiterAnalyzer` and
//! `StandardAnalyzer`. The standard tokenizer splits words like Unicode word boundaries
//! (UAX #29) do for letters, digits and the usual in-word punctuation; it does not apply
//! stemming or stop words (`tokenization_enable_stemming`, `tokenization_skip_stop_words`) or
//! NFC normalization.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use nosql_storage::PartitionKey;
use nosql_storage::read::{ColumnFilter, Limits, RangeRead, ReadOptions, Resume};
use nosql_types::CqlType;

use super::{IndexClass, IndexDef};
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::{IndexRead, IndexScan};
use nosql_storage::sstable::cursor::KeyBound;

use crate::prepare::restrictions::{
    BoundPred, Partitions, Pred, Restrictions, RowFilter, Target, ValueMatcher,
};
use crate::prepare::terms::EvalCtx;
use crate::schema::{ColumnKind, Table};
use crate::statements::paging::PagingState;
use crate::statements::select::SelectPlan;

const ANALYZERS: &str = "org.apache.cassandra.index.sasi.analyzer.";

/// `OnDiskIndexBuilder.Mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Prefix,
    Contains,
    Sparse,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        match s.to_uppercase().as_str() {
            "PREFIX" => Some(Mode::Prefix),
            "CONTAINS" => Some(Mode::Contains),
            "SPARSE" => Some(Mode::Sparse),
            _ => None,
        }
    }

    /// `Mode.supports`.
    fn supports(self, op: Op) -> bool {
        match self {
            Mode::Prefix => matches!(op, Op::Eq | Op::Match | Op::Prefix | Op::Range),
            Mode::Contains => matches!(
                op,
                Op::Eq | Op::Match | Op::Contains | Op::Prefix | Op::Suffix
            ),
            Mode::Sparse => matches!(op, Op::Eq | Op::Range),
        }
    }
}

/// `Expression.Op`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    /// `LIKE 'term'`.
    Match,
    /// `LIKE 'term%'`.
    Prefix,
    /// `LIKE '%term'`.
    Suffix,
    /// `LIKE '%term%'`.
    Contains,
    Range,
    In,
}

impl Op {
    /// The `Operator.toString` of a `LIKE` form.
    fn like_text(self) -> &'static str {
        match self {
            Op::Prefix => "LIKE '<term>%'",
            Op::Suffix => "LIKE '%<term>'",
            Op::Contains => "LIKE '%<term>%'",
            _ => "LIKE '<term>'",
        }
    }
}

/// The analyzer of an index (`IndexMode.getAnalyzer`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Analyzer {
    NoOp,
    NonTokenizing {
        case_sensitive: bool,
        lower: bool,
        upper: bool,
    },
    Standard {
        lower: bool,
        upper: bool,
    },
    Delimiter(char),
}

/// An index's mode and analysis (`IndexMode` + `ColumnIndex`).
#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    /// Text semantics (`is_literal`, by default for text and ascii columns).
    pub literal: bool,
    /// `IndexMode.isAnalyzed`.
    pub analyzed: bool,
    pub analyzer: Analyzer,
    pub ty: CqlType,
}

fn is_text(ty: &CqlType) -> bool {
    matches!(ty, CqlType::Text | CqlType::Ascii)
}

/// `Boolean.parseBoolean`.
fn flag(opts: &BTreeMap<String, String>, key: &str) -> Option<bool> {
    opts.get(key).map(|v| v.eq_ignore_ascii_case("true"))
}

/// The analyzer classes SASI ships (by simple name).
fn analyzer_class(name: &str) -> Option<&str> {
    name.strip_prefix(ANALYZERS).filter(|n| {
        matches!(
            *n,
            "NoOpAnalyzer" | "NonTokenizingAnalyzer" | "StandardAnalyzer" | "DelimiterAnalyzer"
        )
    })
}

impl Config {
    /// The configuration of an index on a column of type `ty` (options as stored).
    pub fn of(ty: &CqlType, opts: &BTreeMap<String, String>) -> Config {
        let ty = ty.clone();
        let mode = opts
            .get("mode")
            .and_then(|m| Mode::parse(m))
            .unwrap_or(Mode::Prefix);
        let class = opts.get("analyzer_class").and_then(|c| analyzer_class(c));
        let analyzed = match class {
            Some(_) => flag(opts, "analyzed").unwrap_or(true),
            None => flag(opts, "analyzed").unwrap_or(false),
        };
        let literal = flag(opts, "is_literal").unwrap_or_else(|| is_text(&ty));
        let analyzer = match (analyzed, class) {
            (false, _) => Analyzer::NoOp,
            (true, Some("NonTokenizingAnalyzer")) => Analyzer::NonTokenizing {
                case_sensitive: flag(opts, "case_sensitive").unwrap_or(true),
                lower: flag(opts, "normalize_lowercase").unwrap_or(false),
                upper: flag(opts, "normalize_uppercase").unwrap_or(false),
            },
            (true, Some("DelimiterAnalyzer")) => Analyzer::Delimiter(
                opts.get("delimiter")
                    .and_then(|d| d.chars().next())
                    .unwrap_or(','),
            ),
            (true, Some("StandardAnalyzer")) => Analyzer::standard(opts),
            (true, Some(_)) => Analyzer::NoOp,
            (true, None) if is_text(&ty) => Analyzer::standard(opts),
            (true, None) => Analyzer::NoOp,
        };
        Config {
            mode,
            literal,
            analyzed,
            analyzer,
            ty,
        }
    }

    pub fn of_def(table: &Table, def: &IndexDef) -> Config {
        Config::of(&table.columns[def.column].ty, &def.options)
    }

    fn tokenized(&self) -> bool {
        matches!(
            self.analyzer,
            Analyzer::Standard { .. } | Analyzer::Delimiter(_)
        )
    }

    /// `ColumnIndex.supports`.
    pub fn supports(&self, op: Op) -> bool {
        !(self.tokenized() && op == Op::Eq)
            && op != Op::In
            && !(self.tokenized() && self.mode == Mode::Contains && op == Op::Prefix)
            && !(self.literal && op == Op::Range)
            && self.mode.supports(op)
    }

    /// Whether the index serves a restriction (`SASIIndex.supportsExpression`); a `LIKE` of any
    /// form needs a literal index, the form itself is checked once the value is bound.
    pub fn supports_pred(&self, pred: &Pred) -> bool {
        match pred {
            Pred::Eq(_) => self.supports(Op::Eq),
            Pred::Slice { .. } => self.supports(Op::Range),
            Pred::Like(_) => self.literal,
            _ => false,
        }
    }

    /// The analyzed terms of a value (`AbstractAnalyzer.reset` + iteration).
    pub fn analyze(&self, v: &[u8]) -> Vec<Bytes> {
        match &self.analyzer {
            Analyzer::NoOp => vec![Bytes::copy_from_slice(v)],
            Analyzer::NonTokenizing {
                case_sensitive,
                lower,
                upper,
            } => {
                if !is_text(&self.ty) {
                    return Vec::new();
                }
                let s = String::from_utf8_lossy(v);
                let s = if !case_sensitive || *lower {
                    s.to_lowercase()
                } else if *upper {
                    s.to_uppercase()
                } else {
                    s.into_owned()
                };
                vec![Bytes::from(s.into_bytes())]
            }
            Analyzer::Delimiter(d) => String::from_utf8_lossy(v)
                .split(*d)
                .filter(|p| !p.is_empty())
                .map(|p| Bytes::copy_from_slice(p.as_bytes()))
                .collect(),
            Analyzer::Standard { lower, upper } => standard_tokens(&String::from_utf8_lossy(v))
                .into_iter()
                .filter(|t| t.chars().count() <= 255)
                .map(|t| {
                    let t = if *lower {
                        t.to_lowercase()
                    } else if *upper {
                        t.to_uppercase()
                    } else {
                        t
                    };
                    Bytes::from(t.into_bytes())
                })
                .collect(),
        }
    }
}

impl Analyzer {
    fn standard(opts: &BTreeMap<String, String>) -> Analyzer {
        Analyzer::Standard {
            lower: flag(opts, "tokenization_normalize_lowercase").unwrap_or(false),
            upper: flag(opts, "tokenization_normalize_uppercase").unwrap_or(false),
        }
    }
}

/// Words of `s` (`StandardTokenizerImpl`, UAX #29 word boundaries for letters and digits):
/// letters, digits and `_` form words; `'` and `.` join letters, `.` and `,` join digits;
/// ideographs and kana are one token each.
fn standard_tokens(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let single = |c: char| matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF);
    let word = |c: char| (c.is_alphanumeric() || c == '_') && !single(c);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if single(c) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.push(c.to_string());
        } else if word(c) {
            cur.push(c);
        } else if !cur.is_empty() && i + 1 < chars.len() && word(chars[i + 1]) {
            let prev = chars[i - 1];
            let next = chars[i + 1];
            let letters = prev.is_alphabetic() && next.is_alphabetic();
            let digits = prev.is_numeric() && next.is_numeric();
            let joins = match c {
                '\'' | '\u{2019}' | ':' | '\u{00B7}' => letters,
                '.' => letters || digits,
                ',' | ';' => digits,
                _ => false,
            };
            if joins {
                cur.push(c);
            } else {
                out.push(std::mem::take(&mut cur));
            }
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        i += 1;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `SASIIndex.validateOptions` for a new index on column `ci` with the statement's options.
pub fn validate(
    table: &Table,
    ci: usize,
    opts: &BTreeMap<String, String>,
    raw: &[(String, String)],
) -> Result<()> {
    let col = &table.columns[ci];
    if col.ty.is_multicell() {
        return Err(Error::config(
            "complex columns are not yet supported by SASI",
        ));
    }
    if col.kind == ColumnKind::PartitionKey {
        return Err(Error::config(
            "partition key columns are not yet supported by SASI",
        ));
    }
    // `IndexMode.validateAnalyzer`.
    if let Some(class) = opts.get("analyzer_class") {
        let Some(simple) = analyzer_class(class) else {
            if class == &format!("{ANALYZERS}AbstractAnalyzer") {
                return Err(Error::config(
                    "Unable to initialize analyzer class option specified [AbstractAnalyzer]",
                ));
            }
            return Err(Error::config(if jdk_class(class) {
                format!(
                    "Invalid analyzer class '{class}': must extend or implement {ANALYZERS}AbstractAnalyzer"
                )
            } else {
                format!("Unable to find analyzer class '{class}'")
            }));
        };
        if simple != "NoOpAnalyzer" && !is_text(&col.ty) {
            return Err(Error::config(format!(
                "{simple} does not support type {}",
                nosql_types::cql_name(&col.ty)
            )));
        }
        if simple == "NonTokenizingAnalyzer"
            && opts.contains_key("case_sensitive")
            && (opts.contains_key("normalize_lowercase")
                || opts.contains_key("normalize_uppercase"))
        {
            return Err(Error::config(
                "case_sensitive option cannot be specified together with either normalize_lowercase or normalize_uppercase",
            ));
        }
    }
    // `IndexMode.getMode`.
    if let Some(m) = opts.get("mode")
        && Mode::parse(m).is_none()
    {
        return Err(Error::config(format!("Incorrect index mode: {m}")));
    }
    if let Some(v) = opts.get("max_compaction_flush_memory_in_mb")
        && java_long(v).is_none()
    {
        let mut all: Vec<(String, String)> = raw.to_vec();
        all.push(("class_name".into(), super::SASI_CLASS.into()));
        let target = all.len();
        all.push((
            "target".into(),
            nosql_types::quote_identifier(&col.name, nosql_types::Dialect::Cassandra).into_owned(),
        ));
        return Err(Error::config(format!(
            "Failed to validate custom indexer options: {}",
            java_map(&all, target)
        )));
    }
    let cfg = Config::of(&col.ty, opts);
    // `DelimiterTokenizingOptions` rejects other delimiters only when the index starts (the
    // reference stores the index, then fails); we refuse it with the same answer.
    if matches!(cfg.analyzer, Analyzer::Delimiter(_))
        && opts
            .get("delimiter")
            .is_some_and(|d| d.chars().count() != 1)
    {
        return Err(Error::server(
            "java.lang.RuntimeException: java.lang.reflect.InvocationTargetException",
        ));
    }
    if cfg.mode == Mode::Sparse {
        if cfg.literal {
            return Err(Error::config(
                "SPARSE mode is only supported on non-literal columns.",
            ));
        }
        if cfg.analyzed {
            return Err(Error::config("SPARSE mode doesn't support analyzers."));
        }
    }
    Ok(())
}

/// A class the JVM would find but that is no analyzer: JDK classes (`java.lang.String`).
fn jdk_class(name: &str) -> bool {
    (name.starts_with("java.") || name.starts_with("javax."))
        && name
            .rsplit('.')
            .next()
            .and_then(|s| s.chars().next())
            .is_some_and(|c| c.is_ascii_uppercase())
}

/// `Long.parseLong`.
fn java_long(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// How Cassandra prints a custom index's options (`IndexAttributes` copies the statement's
/// options into a `HashMap` and adds `class_name`; `IndexMetadata.fromIndexTargets` copies that
/// and adds `target`): `{k=v, ...}`. `entries`: the statement's options, `class_name`, then
/// `target` at index `target`.
fn java_map(entries: &[(String, String)], target: usize) -> String {
    use crate::views::java::{HashOrder, string_hash};
    let mut first = HashOrder::new(string_hash);
    for (k, _) in &entries[..target] {
        first.insert(k);
    }
    let mut second = HashOrder::copy(&first, 0);
    second.insert(&entries[target].0);
    let value = |k: &str| {
        entries
            .iter()
            .rev()
            .find(|(x, _)| x == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    };
    let parts: Vec<String> = second.keys().map(|k| format!("{k}={}", value(k))).collect();
    format!("{{{}}}", parts.join(", "))
}

/// `LikeRestriction.makeSpecific`: the `LIKE` form of a pattern and its term.
pub fn like_form(pattern: &[u8]) -> Result<(Op, &[u8])> {
    let n = pattern.len();
    let ends = pattern.last() == Some(&b'%');
    let starts = pattern.first() == Some(&b'%');
    let (op, begin, end) = match (starts, ends) {
        (true, true) => (Op::Contains, 1, n - 1),
        (false, true) => (Op::Prefix, 0, n - 1),
        (true, false) => (Op::Suffix, 1, n),
        (false, false) => (Op::Match, 0, n),
    };
    if end == 0 || begin >= end {
        return Err(Error::invalid("LIKE value can't be empty."));
    }
    Ok((op, &pattern[begin..end]))
}

/// A literal SASI restriction on one column: every analyzed term of the query must match a
/// term of the value (`Operation` ANDs a column's expressions; `Expression.validateStringValue`).
#[derive(Debug)]
pub struct Matcher {
    cfg: Config,
    op: Op,
    terms: Vec<Bytes>,
}

impl ValueMatcher for Matcher {
    fn matches(&self, value: &[u8]) -> bool {
        let tokens = self.cfg.analyze(value);
        self.terms.iter().all(|t| {
            tokens.iter().any(|v| match self.op {
                Op::Prefix => v.starts_with(t),
                Op::Suffix => v.ends_with(t),
                Op::Contains => t.is_empty() || v.windows(t.len()).any(|w| w == &t[..]),
                _ => nosql_types::compare(&self.cfg.ty, v, t).is_eq(),
            })
        })
    }
}

/// The SASI indexes of a column a query restricts with `=` or `LIKE`.
#[derive(Debug)]
struct Column {
    column: usize,
    name: String,
    /// The restriction is `=` (its bound form is a one-value `In`).
    eq: bool,
    configs: Vec<Config>,
}

/// A read served by SASI: the base table in token order, with the literal restrictions
/// evaluated by [`Matcher`]s.
pub struct Scan {
    columns: Vec<Column>,
}

impl Scan {
    /// The scan of a query, when a SASI index serves an `=` or `LIKE` on a literal column.
    pub fn plan(table: &Table, defs: &[IndexDef], r: &Restrictions) -> Option<Scan> {
        let mut preds: Vec<(usize, &Pred)> = Vec::new();
        for (pos, ps) in r.pk.iter().enumerate() {
            preds.extend(ps.iter().map(|p| (table.partition_key[pos], p)));
        }
        for (pos, ps) in r.ck.iter().enumerate() {
            preds.extend(ps.iter().map(|p| (table.clustering[pos], p)));
        }
        for (ci, ps) in &r.others {
            preds.extend(ps.iter().map(|p| (*ci, p)));
        }
        let mut columns: Vec<Column> = Vec::new();
        for (ci, p) in preds {
            if !matches!(p, Pred::Eq(_) | Pred::Like(_)) || columns.iter().any(|c| c.column == ci) {
                continue;
            }
            let configs: Vec<Config> = defs
                .iter()
                .filter(|d| d.class == IndexClass::Sasi && d.column == ci)
                .map(|d| Config::of_def(table, d))
                .filter(|c| c.literal && c.supports_pred(p))
                .collect();
            if configs.is_empty() {
                continue;
            }
            columns.push(Column {
                column: ci,
                name: table.columns[ci].name.clone(),
                eq: matches!(p, Pred::Eq(_)),
                configs,
            });
        }
        (!columns.is_empty()).then_some(Scan { columns })
    }
}

impl IndexScan for Scan {
    fn adjust_filter(&self, filter: &mut RowFilter) -> Result<()> {
        for (target, pred) in filter.preds.iter_mut() {
            let Target::Column(ci) = target else {
                continue;
            };
            let Some(col) = self.columns.iter().find(|c| c.column == *ci) else {
                continue;
            };
            let (op, term): (Op, Bytes) = match pred {
                BoundPred::Like(pattern) => {
                    let (op, term) = like_form(pattern.as_bytes())?;
                    (op, Bytes::copy_from_slice(term))
                }
                BoundPred::In(vals) if col.eq && vals.len() == 1 => (Op::Eq, vals[0].clone()),
                _ => continue,
            };
            // `IndexRegistry.getBestIndexFor` (the first index that serves the form).
            let Some(cfg) = col.configs.iter().find(|c| c.supports(op)) else {
                return Err(Error::invalid(format!(
                    "{} {} {} is only supported on properly indexed columns",
                    col.name,
                    op.like_text(),
                    String::from_utf8_lossy(&term)
                )));
            };
            let mut terms = cfg.analyze(&term);
            // `=` keeps the last term as its bound (`Operation.analyzeGroup`).
            if op == Op::Eq && terms.len() > 1 {
                terms.drain(..terms.len() - 1);
            }
            *pred = BoundPred::Matcher(Arc::new(Matcher {
                cfg: cfg.clone(),
                op,
                terms,
            }));
        }
        Ok(())
    }

    fn open(
        &self,
        db: &Arc<Database>,
        plan: &SelectPlan,
        cx: &EvalCtx<'_>,
        after: Option<&PagingState>,
        _page: Option<u64>,
    ) -> Result<IndexRead> {
        let t = plan.table.clone();
        let now = cx.now.unwrap_or_else(|| db.now_secs());
        let (start, end) = match plan.restrictions.partitions(&t, cx, None)? {
            Partitions::Range { start, end } => (start, end),
            Partitions::Keys(_) => (KeyBound::Unbounded, KeyBound::Unbounded),
        };
        let resume = after.map(|s| Resume {
            key: PartitionKey::new(nosql_types::token(&s.key), s.key.clone()),
            after: s.clustering.clone(),
            returned_in_partition: s.in_partition,
        });
        let r = RangeRead {
            start,
            end,
            resume,
            clustering: plan.restrictions.clustering(&t, cx, None)?,
            reversed: false,
            columns: ColumnFilter::all(),
            limits: Limits::default(),
            options: ReadOptions::at(now),
        };
        let stream = db
            .storage_table(&t)?
            .scan(r)
            .map(|c| c.map_err(Error::internal))
            .boxed();
        Ok(IndexRead::rows(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_forms() {
        assert_eq!(like_form(b"ab%").unwrap(), (Op::Prefix, &b"ab"[..]));
        assert_eq!(like_form(b"%ab").unwrap(), (Op::Suffix, &b"ab"[..]));
        assert_eq!(like_form(b"%ab%").unwrap(), (Op::Contains, &b"ab"[..]));
        assert_eq!(like_form(b"a%b").unwrap(), (Op::Match, &b"a%b"[..]));
        assert_eq!(like_form(b"%%%").unwrap(), (Op::Contains, &b"%"[..]));
        for empty in [&b""[..], b"%", b"%%"] {
            assert_eq!(
                like_form(empty).unwrap_err().message,
                "LIKE value can't be empty."
            );
        }
    }

    #[test]
    fn standard_tokenizer() {
        assert_eq!(
            standard_tokens("The quick-brown fox's 3.14, 1,000 U.S.A_x"),
            ["The", "quick", "brown", "fox's", "3.14", "1,000", "U.S.A_x"]
        );
        assert_eq!(standard_tokens("日本 ok"), ["日", "本", "ok"]);
    }

    #[test]
    fn java_map_order() {
        let e = |kv: &[(&str, &str)]| -> Vec<(String, String)> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            java_map(
                &e(&[
                    ("mode", "PREFIX"),
                    ("max_compaction_flush_memory_in_mb", "x"),
                    ("class_name", "c"),
                    ("target", "txt")
                ]),
                3
            ),
            "{mode=PREFIX, max_compaction_flush_memory_in_mb=x, class_name=c, target=txt}"
        );
    }
}
