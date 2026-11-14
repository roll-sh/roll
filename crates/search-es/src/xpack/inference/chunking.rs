//! Chunking settings (`chunking_settings` of an endpoint or a `semantic_text` field):
//! Elasticsearch's `ChunkingSettingsBuilder` and the four strategies' `fromMap` / `toXContent`.

use super::settings::{Reader, java_string, validation};
use crate::error::{EsError, EsResult};
use crate::json::write::java_double;
use crate::json::{Map, Value};
use crate::obj;

/// How text is cut into chunks before it is embedded.
#[derive(Clone, Debug, PartialEq)]
pub enum Chunking {
    /// One chunk: the whole text.
    None,
    /// Up to `max_chunk_size` words, consecutive chunks sharing `overlap` words.
    Word { max_chunk_size: i64, overlap: i64 },
    /// Whole sentences up to `max_chunk_size` words, overlapping by 0 or 1 sentence.
    Sentence { max_chunk_size: i64, sentence_overlap: i64 },
    /// Split on the first separator (a regex) that brings pieces under `max_chunk_size` words.
    Recursive { max_chunk_size: i64, separators: Vec<String> },
}

/// Sentences of up to 250 words overlapping by one (`ChunkingSettingsBuilder.DEFAULT_SETTINGS`).
pub const DEFAULT: Chunking = Chunking::Sentence { max_chunk_size: 250, sentence_overlap: 1 };

/// Words, 250 per chunk with 100 shared: what endpoints created before chunking settings existed
/// use (`OLD_DEFAULT_SETTINGS`).
pub const OLD_DEFAULT: Chunking = Chunking::Word { max_chunk_size: 250, overlap: 100 };

const PLAINTEXT: &[&str] = &["(?<!\\n)\\n\\n(?!\\n)", "(?<!\\n)\\n(?!\\n)"];
const MARKDOWN: &[&str] =
    &["\n# ", "\n## ", "\n### ", "\n#### ", "\n##### ", "\n###### ", "\n^(?!\\s*$).*\\n-{1,}\\n", "\n^(?!\\s*$).*\\n={1,}\\n"];

/// Most separators a recursive strategy may list in a request.
const MAX_SEPARATORS: usize = 64;

/// `Arrays.toString` of the keys not in `valid`, in Java `HashMap` order; `None` when all are.
fn invalid_keys(m: &Map, valid: &[&str]) -> Option<String> {
    let bad: Vec<String> = crate::json::java_hash_map(m.clone(), 16).keys().filter(|k| !valid.contains(&k.as_str())).cloned().collect();
    (!bad.is_empty()).then(|| format!("[{}]", bad.join(", ")))
}

fn at_least(r: &mut Reader, key: &str, min: i64) -> Option<i64> {
    let v = r.required_positive_int(key)?;
    if v < min {
        r.error(format!("[chunking_settings] Invalid value [{}]. [{key}] must be greater than or equal to [{}]", java_double(v as f64), java_double(min as f64)));
        return None;
    }
    Some(v)
}

impl Chunking {
    /// Reads a request's `chunking_settings` (`fromMap(settings, true, true)`): `None` (the key
    /// absent) is the old default, `{}` the default.
    pub fn from_request(v: Option<&Value>) -> EsResult<Chunking> {
        match v {
            None => Ok(OLD_DEFAULT),
            Some(Value::Obj(m)) if m.is_empty() => Ok(DEFAULT),
            Some(Value::Obj(m)) => Chunking::parse(m, true),
            Some(other) => Err(EsError::java(
                400,
                "illegal_argument_exception",
                format!("field [chunking_settings] is not of the expected type. The value [{}] cannot be converted to a [Map]", java_string(other)),
            )),
        }
    }

    /// Reads stored settings (no request limits).
    pub fn from_stored(v: &Value) -> Option<Chunking> {
        match v {
            Value::Obj(m) if !m.is_empty() => Chunking::parse(m, false).ok(),
            _ => None,
        }
    }

    fn parse(m: &Map, enforce_limits: bool) -> EsResult<Chunking> {
        let Some(strategy) = m.get("strategy") else {
            return Err(EsError::java(400, "illegal_argument_exception", "Can't generate Chunker without ChunkingStrategy provided"));
        };
        let strategy = strategy.to_es_string();
        let mut r = Reader::new(m.clone(), "chunking_settings");
        r.map.shift_remove("strategy");
        match strategy.as_str() {
            "none" => match invalid_keys(m, &["strategy"]) {
                Some(bad) => Err(validation(&[format!("When chunking is disabled (none), settings can not have the following: {bad}")])),
                None => Ok(Chunking::None),
            },
            "word" => {
                if let Some(bad) = invalid_keys(m, &["strategy", "max_chunk_size", "overlap"]) {
                    r.error(format!("Word based chunking settings can not have the following settings: {bad}"));
                }
                let max = at_least(&mut r, "max_chunk_size", 10);
                let overlap = max.and_then(|max| {
                    let o = r.required_positive_int("overlap")?;
                    if o > max / 2 {
                        r.error(format!(
                            "[chunking_settings] Invalid value [{}]. [overlap] must be less than or equal to [{}]",
                            java_double(o as f64),
                            java_double((max / 2) as f64)
                        ));
                    }
                    Some(o)
                });
                r.finish()?;
                Ok(Chunking::Word { max_chunk_size: max.unwrap_or(0), overlap: overlap.unwrap_or(0) })
            }
            "sentence" => {
                if let Some(bad) = invalid_keys(m, &["strategy", "max_chunk_size", "sentence_overlap"]) {
                    r.error(format!("Sentence based chunking settings can not have the following settings: {bad}"));
                }
                let max = at_least(&mut r, "max_chunk_size", 20);
                let overlap = match r.optional_int("sentence_overlap") {
                    None => 1,
                    Some(o) if !(0..=1).contains(&o) => {
                        r.error(format!("sentence_overlap[{o}] must be either 0 or 1"));
                        1
                    }
                    Some(o) => o,
                };
                r.finish()?;
                Ok(Chunking::Sentence { max_chunk_size: max.unwrap_or(0), sentence_overlap: overlap })
            }
            "recursive" => {
                if let Some(bad) = invalid_keys(m, &["strategy", "max_chunk_size", "separators", "separator_group"]) {
                    r.error(format!("Recursive chunking settings can not have the following settings: {bad}"));
                }
                let max = at_least(&mut r, "max_chunk_size", 10);
                let group = r.optional_enum("separator_group", &["plaintext", "markdown"]);
                let listed: Option<Vec<String>> = match r.map.shift_remove("separators") {
                    None => None,
                    Some(Value::Arr(a)) if a.iter().all(|x| matches!(x, Value::Str(_))) => Some(a.iter().map(Value::to_es_string).collect()),
                    Some(other) => {
                        r.error(format!("field [separators] is not of the expected type. The value [{}] cannot be converted to a [List]", java_string(&other)));
                        None
                    }
                };
                if listed.is_some() && group.is_some() {
                    r.error("Recursive chunking settings can not have both separators and separator_set");
                }
                let separators = match (group.as_deref(), listed) {
                    (Some("markdown"), _) => MARKDOWN.iter().map(|s| s.to_string()).collect(),
                    (Some(_), _) | (None, None) => PLAINTEXT.iter().map(|s| s.to_string()).collect(),
                    (None, Some(s)) => {
                        if s.is_empty() {
                            r.error("Recursive chunking settings can not have an empty list of separators");
                        }
                        if enforce_limits && s.len() > MAX_SEPARATORS {
                            r.error(format!("separators list size [{}] must not exceed [{MAX_SEPARATORS}]", s.len()));
                        }
                        s
                    }
                };
                r.finish()?;
                Ok(Chunking::Recursive { max_chunk_size: max.unwrap_or(0), separators })
            }
            other => Err(EsError::java(400, "illegal_argument_exception", format!("Invalid chunkingStrategy {other}"))),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Chunking::None => obj! { "strategy" => "none" },
            Chunking::Word { max_chunk_size, overlap } => obj! { "strategy" => "word", "max_chunk_size" => *max_chunk_size, "overlap" => *overlap },
            Chunking::Sentence { max_chunk_size, sentence_overlap } => {
                obj! { "strategy" => "sentence", "max_chunk_size" => *max_chunk_size, "sentence_overlap" => *sentence_overlap }
            }
            Chunking::Recursive { max_chunk_size, separators } => obj! {
                "strategy" => "recursive",
                "max_chunk_size" => *max_chunk_size,
                "separators" => Value::Arr(separators.iter().map(|s| Value::from(s.as_str())).collect()),
            },
        }
    }
}

/// A chunk of a text: offsets in UTF-16 code units, as Java's `String` counts them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A text's word boundaries (ICU's word `BreakIterator` as Unicode word segmentation): each
/// boundary (a byte offset) and whether the segment ending there is a word (a letter or digit
/// in it, ICU's rule status other than `WORD_NONE`).
fn word_bounds(text: &str, from: usize) -> Vec<(usize, bool)> {
    use unicode_segmentation::UnicodeSegmentation;
    text[from..].split_word_bound_indices().map(|(i, w)| (from + i + w.len(), w.chars().any(char::is_alphanumeric))).collect()
}

fn count_words(text: &str, start: usize, end: usize) -> usize {
    word_bounds(&text[..end], start).iter().filter(|(_, w)| *w).count()
}

/// `WordBoundaryChunker.chunkPositions` (byte offsets and word counts).
fn word_chunks(text: &str, size: usize, overlap: usize) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let step = size.saturating_sub(overlap);
    let (mut in_chunk, mut next_start, mut start, mut since_mark) = (0usize, 0usize, 0usize, 0usize);
    for (boundary, word) in word_bounds(text, 0) {
        if !word {
            continue;
        }
        in_chunk += 1;
        since_mark += 1;
        if in_chunk >= size {
            out.push((start, boundary, in_chunk));
            in_chunk = overlap;
            if overlap == 0 {
                next_start = boundary;
            }
            start = next_start;
        }
        if since_mark == step {
            next_start = boundary;
            since_mark = 0;
        }
    }
    if in_chunk > overlap || out.is_empty() {
        out.push((start, text.len(), in_chunk));
    }
    out
}

/// `SentenceBoundaryChunker.skipWords`: the offset after `n` words from the start of the word
/// at `start`.
fn skip_words(text: &str, start: usize, n: usize) -> usize {
    if n == 0 {
        return start;
    }
    let mut count = 0;
    for (b, word) in word_bounds(text, start) {
        if word {
            count += 1;
            if count == n {
                return b;
            }
        }
    }
    text.len()
}

/// `SentenceBoundaryChunker.chunk` (byte offsets).
fn sentence_chunks(text: &str, max: usize, preceding: bool) -> Vec<(usize, usize)> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut chunks = Vec::new();
    let (mut chunk_start, mut chunk_end, mut sentence_start, mut chunk_words) = (0usize, 0usize, 0usize, 0usize);
    let (mut prev_words, mut prev_start) = (0usize, 0usize);
    let max_overlap = (max / 2).min(20);
    let long_overlap = 20.min(max.saturating_sub(1) / 2);
    for (i, s) in text.split_sentence_bound_indices() {
        let sentence_end = i + s.len();
        let words = count_words(text, sentence_start, sentence_end);
        if chunk_words + words > max {
            let mut next = words;
            if chunk_words > 0 {
                chunks.push((chunk_start, chunk_end));
                if preceding {
                    if prev_words + words > max {
                        let skip = prev_words.saturating_sub(max_overlap);
                        chunk_start = skip_words(text, prev_start, skip);
                        chunk_words = (prev_words - skip) + words;
                    } else {
                        chunk_words = prev_words + words;
                        chunk_start = prev_start;
                    }
                    next = chunk_words;
                } else {
                    chunk_start = chunk_end;
                    chunk_words = words;
                }
            }
            if next > max {
                let splits = word_chunks(&text[chunk_start..sentence_end], max, long_overlap);
                for s in &splits[..splits.len() - 1] {
                    chunks.push((chunk_start + s.0, chunk_start + s.1));
                }
                let last = splits[splits.len() - 1];
                chunk_start += last.0;
                chunk_words = last.2;
            }
        } else {
            chunk_words += words;
        }
        if preceding {
            prev_start = sentence_start;
            prev_words = words;
        }
        sentence_start = sentence_end;
        chunk_end = sentence_end;
    }
    if chunk_words > 0 {
        chunks.push((chunk_start, text.len()));
    }
    if chunks.is_empty() {
        chunks.push((0, text.len()));
    }
    chunks
}

/// `RecursiveChunker`: split on the first separator that brings every piece within the word
/// limit (pieces merged back up to it), recursing with the next separators into pieces that are
/// still too long, and on sentences once none is left. Separators are regular expressions;
/// lookarounds, which the regex engine lacks, are dropped (`(?<!\n)\n(?!\n)` splits on every
/// `\n`).
fn recursive_chunks(text: &str, start: usize, end: usize, max: usize, separators: &[String], out: &mut Vec<(usize, usize)>) {
    if count_words(text, start, end) <= max {
        out.push((start, end));
        return;
    }
    let Some((sep, rest)) = separators.split_first() else {
        for (s, e) in sentence_chunks(&text[start..end], max, false) {
            out.push((start + s, start + e));
        }
        return;
    };
    let pattern = regex::Regex::new(r"\(\?<?[!=][^)]*\)").map(|re| re.replace_all(sep, "").into_owned()).unwrap_or_else(|_| sep.clone());
    let Ok(re) = regex::RegexBuilder::new(&pattern).multi_line(true).size_limit(1 << 20).build() else {
        return recursive_chunks(text, start, end, max, rest, out);
    };
    // Pieces start at each separator match.
    let mut cuts = vec![start];
    for m in re.find_iter(&text[start..end]) {
        if m.start() > 0 {
            cuts.push(start + m.start());
        }
    }
    cuts.push(end);
    cuts.dedup();
    if cuts.len() <= 2 {
        return recursive_chunks(text, start, end, max, rest, out);
    }
    let (mut cur_start, mut cur_words) = (cuts[0], 0usize);
    for w in cuts.windows(2) {
        let (s, e) = (w[0], w[1]);
        let words = count_words(text, s, e);
        if words > max {
            if cur_words > 0 {
                out.push((cur_start, s));
            }
            recursive_chunks(text, s, e, max, rest, out);
            cur_start = e;
            cur_words = 0;
        } else if cur_words + words > max {
            out.push((cur_start, s));
            cur_start = s;
            cur_words = words;
        } else {
            cur_words += words;
        }
    }
    if cur_start < end && (cur_words > 0 || out.is_empty()) {
        out.push((cur_start, end));
    }
}

/// Byte spans as UTF-16 spans, in one pass over the text (a text has many chunks: counting
/// from its start for each would be quadratic).
fn utf16_spans(text: &str, bytes: &[(usize, usize)]) -> Vec<Span> {
    let mut points: Vec<usize> = bytes.iter().flat_map(|&(s, e)| [s, e]).collect();
    points.sort_unstable();
    points.dedup();
    let mut units = Vec::with_capacity(points.len());
    let (mut byte, mut count) = (0usize, 0usize);
    for &p in &points {
        count += text.get(byte..p).map_or(0, |t| t.encode_utf16().count());
        byte = p;
        units.push(count);
    }
    let at = |b: usize| points.binary_search(&b).map_or(0, |i| units[i]);
    bytes.iter().map(|&(s, e)| Span { start: at(s), end: at(e) }).collect()
}

impl Chunking {
    /// The chunks of `text` (`Chunker.chunk`), as UTF-16 offsets.
    pub fn chunk(&self, text: &str) -> Vec<Span> {
        self.pieces(text).into_iter().map(|(s, _)| s).collect()
    }

    /// The chunks of `text`: each one's UTF-16 span and its text.
    pub fn pieces<'t>(&self, text: &'t str) -> Vec<(Span, &'t str)> {
        let bytes = self.byte_spans(text);
        let spans = utf16_spans(text, &bytes);
        spans.into_iter().zip(&bytes).map(|(span, &(s, e))| (span, text.get(s..e).unwrap_or_default())).collect()
    }

    fn byte_spans(&self, text: &str) -> Vec<(usize, usize)> {
        match self {
            Chunking::None => vec![(0, text.len())],
            Chunking::Word { max_chunk_size, overlap } => word_chunks(text, (*max_chunk_size).max(1) as usize, *overlap as usize).into_iter().map(|(s, e, _)| (s, e)).collect(),
            Chunking::Sentence { max_chunk_size, sentence_overlap } => sentence_chunks(text, (*max_chunk_size).max(1) as usize, *sentence_overlap > 0),
            Chunking::Recursive { max_chunk_size, separators } => {
                let mut out = Vec::new();
                recursive_chunks(text, 0, text.len(), (*max_chunk_size).max(1) as usize, separators, &mut out);
                out
            }
        }
    }
}

/// The text of a chunk (`String.substring` over UTF-16 offsets).
pub fn substring(text: &str, span: Span) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = span.end.min(units.len());
    String::from_utf16_lossy(&units[span.start.min(end)..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> EsResult<Chunking> {
        Chunking::from_request(Some(&v))
    }

    #[test]
    fn defaults_and_strategies() {
        assert_eq!(Chunking::from_request(None).unwrap(), OLD_DEFAULT);
        assert_eq!(parse(obj! {}).unwrap(), DEFAULT);
        assert_eq!(parse(obj! { "strategy" => "word", "max_chunk_size" => 20, "overlap" => 2 }).unwrap(), Chunking::Word { max_chunk_size: 20, overlap: 2 });
        assert_eq!(parse(obj! { "strategy" => "none" }).unwrap().to_value(), obj! { "strategy" => "none" });
        let e = parse(obj! { "strategy" => "sentence", "max_chunk_size" => 5 }).unwrap_err();
        assert_eq!(e.reason(), "Validation Failed: 1: [chunking_settings] Invalid value [5.0]. [max_chunk_size] must be greater than or equal to [20.0];");
        let e = parse(obj! { "strategy" => "word", "max_chunk_size" => 20, "overlap" => 11 }).unwrap_err();
        assert_eq!(e.reason(), "Validation Failed: 1: [chunking_settings] Invalid value [11.0]. [overlap] must be less than or equal to [10.0];");
        assert_eq!(parse(obj! { "strategy" => "bogus" }).unwrap_err().reason(), "Invalid chunkingStrategy bogus");
        assert_eq!(parse(obj! { "max_chunk_size" => 5 }).unwrap_err().reason(), "Can't generate Chunker without ChunkingStrategy provided");
    }
}

#[cfg(test)]
mod chunker_tests {
    use super::*;

    #[test]
    fn word_and_sentence_chunks() {
        let text = "one two three four five six seven";
        let w = Chunking::Word { max_chunk_size: 3, overlap: 1 }.chunk(text);
        let parts: Vec<String> = w.iter().map(|s| substring(text, *s)).collect();
        // A window starts at the boundary after the word before it, as Elasticsearch's does.
        assert_eq!(parts, vec!["one two three", " three four five", " five six seven"]);
        let text = "These are not the droids. He's free to go. Move along.";
        let s = Chunking::Sentence { max_chunk_size: 20, sentence_overlap: 0 }.chunk(text);
        assert_eq!(s, vec![Span { start: 0, end: text.len() }]);
        let s = Chunking::Sentence { max_chunk_size: 5, sentence_overlap: 0 }.chunk(text);
        let parts: Vec<String> = s.iter().map(|x| substring(text, *x)).collect();
        assert_eq!(parts, vec!["These are not the droids. ", "He's free to go. ", "Move along."]);
        assert_eq!(Chunking::None.chunk("é😀"), vec![Span { start: 0, end: 3 }]);
    }

    /// A long text (here 512 KiB, tens of thousands of chunks) chunks in time linear in its
    /// length: `semantic_text` chunks writers' documents on a shared node.
    #[test]
    fn long_texts_chunk_in_linear_time() {
        let text = "Ab cd é😀 gh. ".repeat(32_000);
        let started = std::time::Instant::now();
        let pieces = Chunking::Sentence { max_chunk_size: 20, sentence_overlap: 1 }.pieces(&text);
        assert!(pieces.len() > 5_000, "{}", pieces.len());
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
        let (span, piece) = pieces[1];
        assert_eq!(substring(&text, span), piece);
    }
}
