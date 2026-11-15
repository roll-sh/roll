//! `include` / `exclude` of terms-like aggregations (Elasticsearch's `IncludeExclude`): Lucene
//! regular expressions, exact value lists and hash partitions.
//!
//! Regexes compile to byte DFAs (`search-codec`'s Lucene `RegExp` port, `ALL` flags) once at
//! parse time; `include - exclude` is a DFA intersection with the complement. For ordinal
//! fields the accepted terms are resolved per segment dictionary (an intersection walk that
//! skips dead prefixes) into a bit set, so collection tests one bit per value.

use std::collections::BTreeSet;
use std::sync::Arc;

use search_codec::automaton::{Automaton, Dfa};
use search_codec::automaton::regexp;
use search_codec::points::encode;

use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::json::Json;
use crate::util::{floor_mod, mix64, murmur3_x86_32};
use crate::xparser::{Token, XParser};

const HASH_PARTITIONING_SEED: u32 = 31;

/// A parsed `include` / `exclude`.
#[derive(Clone, Debug)]
pub struct IncludeExclude {
    pub include: Option<String>,
    pub exclude: Option<String>,
    pub include_values: Option<BTreeSet<Vec<u8>>>,
    pub exclude_values: Option<BTreeSet<Vec<u8>>>,
    /// `(partition, num_partitions)`.
    pub partition: Option<(i32, i32)>,
    /// `include` minus `exclude` as one DFA (regex-based only).
    automaton: Option<Arc<Dfa>>,
}

fn compile_regexp(pattern: &str) -> Result<Dfa> {
    regexp::compile(pattern, regexp::ALL, false, search_codec::automaton::dfa::DEFAULT_WORK_LIMIT).map_err(|e| match e {
        regexp::RegexpError::Syntax(s) => AggError::illegal_argument(s),
        regexp::RegexpError::TooComplex(t) => AggError::new(400, "too_complex_to_determinize_exception", t.to_string()),
    })
}

impl IncludeExclude {
    fn build(
        include: Option<String>,
        exclude: Option<String>,
        include_values: Option<BTreeSet<Vec<u8>>>,
        exclude_values: Option<BTreeSet<Vec<u8>>>,
    ) -> Result<IncludeExclude> {
        let automaton = if include.is_some() || exclude.is_some() {
            let inc = match &include {
                Some(p) => compile_regexp(p)?,
                None => compile_regexp(".*")?,
            };
            let a = match &exclude {
                Some(p) => Dfa::intersection(&inc, &compile_regexp(p)?.complement()),
                None => inc,
            };
            Some(Arc::new(a))
        } else {
            None
        };
        Ok(IncludeExclude { include, exclude, include_values, exclude_values, partition: None, automaton })
    }

    /// `parseInclude` (the parser is on the value).
    pub fn parse_include(p: &mut XParser<'_>) -> Result<IncludeExclude> {
        match p.token() {
            Token::String => IncludeExclude::build(Some(p.text()?), None, None, None),
            Token::StartArray => IncludeExclude::build(None, None, Some(parse_set(p)?), None),
            Token::StartObject => {
                let mut partition = None;
                let mut num = None;
                let mut name = String::new();
                loop {
                    match p.next_token() {
                        Some(Token::EndObject) | None => break,
                        Some(Token::FieldName) => name = p.current_name().unwrap_or("").to_string(),
                        Some(_) => match name.as_str() {
                            "num_partitions" => num = Some(p.int_value()?),
                            "partition" => partition = Some(p.int_value()?),
                            other => {
                                return Err(AggError::new(400, "parse_exception", format!("Unknown parameter in Include/Exclude clause: {other}")));
                            }
                        },
                    }
                }
                let partition = partition.ok_or_else(|| AggError::illegal_argument("Missing [partition] parameter for partition-based include"))?;
                let num = num.ok_or_else(|| AggError::illegal_argument("Missing [num_partitions] parameter for partition-based include"))?;
                if partition < 0 || partition >= num {
                    return Err(AggError::illegal_argument(format!("Partition must be >=0 and < numPartition which is {num}")));
                }
                Ok(IncludeExclude { include: None, exclude: None, include_values: None, exclude_values: None, partition: Some((partition, num)), automaton: None })
            }
            t => Err(AggError::illegal_argument(format!("Unrecognized token for an include [{t}]"))),
        }
    }

    /// `parseExclude`.
    pub fn parse_exclude(p: &mut XParser<'_>) -> Result<IncludeExclude> {
        match p.token() {
            Token::String => IncludeExclude::build(None, Some(p.text()?), None, None),
            Token::StartArray => IncludeExclude::build(None, None, None, Some(parse_set(p)?)),
            t => Err(AggError::illegal_argument(format!("Unrecognized token for an exclude [{t}]"))),
        }
    }

    /// `IncludeExclude.merge(include, exclude)`.
    pub fn merge(include: Option<IncludeExclude>, exclude: Option<IncludeExclude>) -> Result<Option<IncludeExclude>> {
        match (include, exclude) {
            (None, e) => Ok(e),
            (i, None) => Ok(i),
            (Some(i), Some(e)) => {
                if i.partition.is_some() {
                    return Err(AggError::illegal_argument("Cannot specify any excludes when using a partition-based include"));
                }
                Ok(Some(IncludeExclude::build(i.include, e.exclude, i.include_values, e.exclude_values)?))
            }
        }
    }

    pub fn is_regex_based(&self) -> bool {
        self.include.is_some() || self.exclude.is_some()
    }

    pub fn is_partition_based(&self) -> bool {
        self.partition.is_some()
    }

    /// The include/exclude fields as `toXContent` prints them.
    pub fn to_json(&self, out: &mut Json) {
        if let Some((p, n)) = self.partition {
            out.push("include", Json::obj().with("partition", p).with("num_partitions", n));
            return;
        }
        if let Some(i) = &self.include {
            out.push("include", i.as_str());
        }
        if let Some(v) = &self.include_values {
            out.push("include", Json::Arr(v.iter().map(|b| Json::Str(String::from_utf8_lossy(b).into_owned())).collect()));
        }
        if let Some(e) = &self.exclude {
            out.push("exclude", e.as_str());
        }
        if let Some(v) = &self.exclude_values {
            out.push("exclude", Json::Arr(v.iter().map(|b| Json::Str(String::from_utf8_lossy(b).into_owned())).collect()));
        }
    }

    /// A filter over byte terms (`convertToStringFilter` / ordinals filters, with values parsed
    /// through the field's format, e.g. IPs).
    pub fn string_filter(&self, format: &DocValueFormat) -> Result<StringFilter> {
        if let Some((p, n)) = self.partition {
            return Ok(StringFilter::Partition(p, n));
        }
        let parse = |set: &Option<BTreeSet<Vec<u8>>>| -> Result<Option<hashbrown::HashSet<Vec<u8>>>> {
            match set {
                None => Ok(None),
                Some(s) => {
                    let mut out = hashbrown::HashSet::with_capacity(s.len());
                    for v in s {
                        out.insert(format.parse_bytes(&Json::Str(String::from_utf8_lossy(v).into_owned()))?);
                    }
                    Ok(Some(out))
                }
            }
        };
        Ok(StringFilter::SetAndRegex { valids: parse(&self.include_values)?, invalids: parse(&self.exclude_values)?, automaton: self.automaton.clone() })
    }

    /// `convertToLongFilter(format)`.
    pub fn long_filter(&self, format: &DocValueFormat) -> Result<LongFilter> {
        if let Some((p, n)) = self.partition {
            return Ok(LongFilter::Partition(p, n));
        }
        let parse = |set: &Option<BTreeSet<Vec<u8>>>| -> Result<Option<hashbrown::HashSet<i64>>> {
            match set {
                None => Ok(None),
                Some(s) => {
                    let mut out = hashbrown::HashSet::with_capacity(s.len());
                    for v in s {
                        out.insert(format.parse_long(&String::from_utf8_lossy(v), false, 0)?);
                    }
                    Ok(Some(out))
                }
            }
        };
        Ok(LongFilter::Set { valids: parse(&self.include_values)?, invalids: parse(&self.exclude_values)? })
    }

    /// `convertToDoubleFilter()`: values as sortable longs of their doubles.
    pub fn double_filter(&self) -> Result<LongFilter> {
        if let Some((p, n)) = self.partition {
            return Ok(LongFilter::Partition(p, n));
        }
        let parse = |set: &Option<BTreeSet<Vec<u8>>>| -> Result<Option<hashbrown::HashSet<i64>>> {
            match set {
                None => Ok(None),
                Some(s) => {
                    let mut out = hashbrown::HashSet::with_capacity(s.len());
                    for v in s {
                        let t = String::from_utf8_lossy(v);
                        let d = crate::xparser::java_parse_double(&t)?;
                        out.insert(encode::f64_sortable(d));
                    }
                    Ok(Some(out))
                }
            }
        };
        Ok(LongFilter::Set { valids: parse(&self.include_values)?, invalids: parse(&self.exclude_values)? })
    }
}

fn parse_set(p: &mut XParser<'_>) -> Result<BTreeSet<Vec<u8>>> {
    let mut set = BTreeSet::new();
    loop {
        match p.next_token() {
            Some(Token::EndArray) | None => break,
            Some(t) if t.is_value() => {
                set.insert(p.text()?.into_bytes());
            }
            Some(_) => return Err(AggError::new(400, "parse_exception", "Array elements in include/exclude clauses should be string values")),
        }
    }
    Ok(set)
}

/// Accepts or rejects byte terms.
#[derive(Clone)]
pub enum StringFilter {
    Partition(i32, i32),
    SetAndRegex {
        valids: Option<hashbrown::HashSet<Vec<u8>>>,
        invalids: Option<hashbrown::HashSet<Vec<u8>>>,
        automaton: Option<Arc<Dfa>>,
    },
}

impl StringFilter {
    #[inline]
    pub fn accept(&self, v: &[u8]) -> bool {
        match self {
            StringFilter::Partition(p, n) => floor_mod(murmur3_x86_32(v, HASH_PARTITIONING_SEED) as i64, *n as i64) == *p as i64,
            StringFilter::SetAndRegex { valids, invalids, automaton } => {
                if let Some(s) = valids
                    && !s.contains(v)
                {
                    return false;
                }
                if let Some(a) = automaton
                    && !a.matches(v)
                {
                    return false;
                }
                invalids.as_ref().is_none_or(|s| !s.contains(v))
            }
        }
    }

    /// The accepted ordinals of a segment dictionary, as a bit set (`acceptedGlobalOrdinals`).
    pub fn accepted_ords(&self, dict: &search_codec::TermDict) -> Vec<u64> {
        let n = dict.len() as usize;
        let mut bits = vec![0u64; n.div_ceil(64)];
        match self {
            StringFilter::SetAndRegex { valids, invalids, automaton } => {
                let set = |bits: &mut Vec<u64>, o: u64| bits[(o / 64) as usize] |= 1 << (o % 64);
                match (valids, automaton) {
                    (Some(vs), None) => {
                        for v in vs {
                            if let Ok(o) = dict.seek(v) {
                                set(&mut bits, o);
                            }
                        }
                    }
                    (None, Some(a)) => {
                        let mut it = dict.intersect(a.as_ref());
                        while let Some(o) = it.next() {
                            set(&mut bits, o);
                        }
                    }
                    (Some(vs), Some(a)) => {
                        for v in vs {
                            if a.matches(v)
                                && let Ok(o) = dict.seek(v)
                            {
                                set(&mut bits, o);
                            }
                        }
                    }
                    (None, None) => {
                        for (i, w) in bits.iter_mut().enumerate() {
                            let rem = n - i * 64;
                            *w = if rem >= 64 { u64::MAX } else { (1u64 << rem) - 1 };
                        }
                    }
                }
                if let Some(inv) = invalids {
                    for v in inv {
                        if let Ok(o) = dict.seek(v) {
                            bits[(o / 64) as usize] &= !(1 << (o % 64));
                        }
                    }
                }
            }
            StringFilter::Partition(..) => {
                let mut it = dict.iter_from(0);
                while let Some(t) = it.next() {
                    if self.accept(t) {
                        let o = it.ord();
                        bits[(o / 64) as usize] |= 1 << (o % 64);
                    }
                }
            }
        }
        bits
    }
}

/// Accepts or rejects numeric terms (longs, or sortable longs of doubles).
#[derive(Clone)]
pub enum LongFilter {
    Partition(i32, i32),
    Set { valids: Option<hashbrown::HashSet<i64>>, invalids: Option<hashbrown::HashSet<i64>> },
}

impl LongFilter {
    #[inline]
    pub fn accept(&self, v: i64) -> bool {
        match self {
            LongFilter::Partition(p, n) => floor_mod(mix64(v as u64) as i64, *n as i64) == *p as i64,
            LongFilter::Set { valids, invalids } => valids.as_ref().is_none_or(|s| s.contains(&v)) && invalids.as_ref().is_none_or(|s| !s.contains(&v)),
        }
    }
}

/// Tests one bit of an accepted-ordinals set.
#[inline]
pub fn bit(bits: &[u64], o: u64) -> bool {
    bits.get((o / 64) as usize).is_some_and(|w| w & (1 << (o % 64)) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_inc(src: &str) -> Result<IncludeExclude> {
        let j = Json::parse(src).unwrap();
        let mut p = XParser::new(&j, None, Vec::new());
        p.next_token();
        IncludeExclude::parse_include(&mut p)
    }

    #[test]
    fn regex_and_values() {
        let i = parse_inc(r#""ba.*""#).unwrap();
        let f = i.string_filter(&DocValueFormat::Raw).unwrap();
        assert!(f.accept(b"bar"));
        assert!(!f.accept(b"foo"));
        let e = {
            let j = Json::parse(r#""bar""#).unwrap();
            let mut p = XParser::new(&j, None, Vec::new());
            p.next_token();
            IncludeExclude::parse_exclude(&mut p).unwrap()
        };
        let m = IncludeExclude::merge(Some(i), Some(e)).unwrap().unwrap();
        let f = m.string_filter(&DocValueFormat::Raw).unwrap();
        assert!(!f.accept(b"bar"));
        assert!(f.accept(b"baz"));
        let v = parse_inc(r#"["a","c"]"#).unwrap();
        let f = v.string_filter(&DocValueFormat::Raw).unwrap();
        assert!(f.accept(b"a") && !f.accept(b"b"));
        let p = parse_inc(r#"{"partition":1,"num_partitions":3}"#).unwrap();
        assert!(p.is_partition_based());
        assert_eq!(parse_inc(r#"{"partition":3,"num_partitions":3}"#).unwrap_err().reason, "Partition must be >=0 and < numPartition which is 3");
        assert!(parse_inc(r#""[""#).is_err());
    }
}
