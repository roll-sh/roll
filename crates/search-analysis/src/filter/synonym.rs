//! `synonym` and `synonym_graph`: synonym maps, their Solr and WordNet rule parsers, and
//! Lucene's `SynonymFilter` (flat, legacy) and `SynonymGraphFilter` (token graphs with position
//! lengths, what query parsers need for multi-word synonyms).
//!
//! As in Elasticsearch, the rules are analyzed by the analysis chain that precedes the filter in
//! each analyzer (the tokenizer, char filters and earlier token filters, each replaced by its
//! "synonym filter": some are skipped, some rejected), so a synonym filter is only built once it
//! knows its chain ([`FilterFactory::chain_aware`]). Rule errors surface then, with
//! Elasticsearch's nested `failed to build synonyms` / `Invalid synonym rule at line N` errors.
//!
//! The map is a trie over the analyzed input phrases (words joined by `\0`, Lucene's
//! `WORD_SEPARATOR`) with per-entry output ordinals, matching Lucene's FST lookups code point by
//! code point. Both filters keep their lookahead and output queues in recycled buffers.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::analyzer::AnalyzerSpec;
use crate::component::{
    AnalysisMode, ChainParts, FilterFactory, FilterResolver, NamedFilter, SynonymRole,
};
use crate::error::{AnalysisError, Result};
use crate::registry::{Ctx, missing_file};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

pub(crate) const TYPE_SYNONYM: &str = "SYNONYM";
const SEP: char = '\0';

// ---------------------------------------------------------------------------------------------
// The map.

#[derive(Default, Debug)]
struct Node {
    next: Vec<(char, u32)>,
    /// Index into `entries` when an input phrase ends here.
    entry: Option<u32>,
}

#[derive(Debug)]
struct Entry {
    keep_orig: bool,
    outputs: Vec<u32>,
}

/// Lucene's `SynonymMap`.
#[derive(Debug, Default)]
pub(crate) struct SynonymMap {
    nodes: Vec<Node>,
    entries: Vec<Entry>,
    /// Output phrases by ordinal (words separated by `\0`).
    words: Vec<Box<str>>,
    pub max_horizontal_context: usize,
}

impl SynonymMap {
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn step(&self, node: u32, c: char) -> Option<u32> {
        let next = &self.nodes[node as usize].next;
        next.binary_search_by_key(&c, |e| e.0)
            .ok()
            .map(|i| next[i].1)
    }

    /// Walks `term` from `node`; None if the trie has no such path.
    fn walk(&self, mut node: u32, term: &str) -> Option<u32> {
        for c in term.chars() {
            node = self.step(node, c)?;
        }
        Some(node)
    }
}

/// `SynonymMap.Builder` (Elasticsearch's `ESSynonymMapBuilder`): rules keyed by input phrase,
/// outputs deduplicated per input.
#[derive(Default)]
struct Builder {
    working: HashMap<String, (bool, Vec<u32>)>,
    order: Vec<String>,
    word_ids: HashMap<String, u32>,
    words: Vec<Box<str>>,
    max_horizontal_context: usize,
}

fn count_words(s: &str) -> usize {
    1 + s.chars().filter(|&c| c == SEP).count()
}

impl Builder {
    fn add(&mut self, input: &str, output: &str, include_orig: bool) -> Result<()> {
        if input.is_empty() {
            return Err(AnalysisError::illegal("input.length must be > 0 (got 0)"));
        }
        if output.is_empty() {
            return Err(AnalysisError::illegal("output.length must be > 0 (got 0)"));
        }
        let ord = match self.word_ids.get(output) {
            Some(&o) => o,
            None => {
                let o = self.words.len() as u32;
                self.words.push(output.into());
                self.word_ids.insert(output.to_string(), o);
                o
            }
        };
        match self.working.get_mut(input) {
            Some(e) => {
                e.1.push(ord);
                e.0 |= include_orig;
            }
            None => {
                self.working
                    .insert(input.to_string(), (include_orig, vec![ord]));
                self.order.push(input.to_string());
            }
        }
        self.max_horizontal_context = self
            .max_horizontal_context
            .max(count_words(input))
            .max(count_words(output));
        Ok(())
    }

    fn build(self) -> SynonymMap {
        let mut map = SynonymMap {
            nodes: vec![Node::default()],
            entries: Vec::new(),
            words: self.words,
            max_horizontal_context: self.max_horizontal_context,
        };
        for input in &self.order {
            let (keep_orig, ords) = &self.working[input];
            let mut outputs: Vec<u32> = Vec::with_capacity(ords.len());
            for &o in ords {
                if !outputs.contains(&o) {
                    outputs.push(o);
                }
            }
            let mut n = 0usize;
            for c in input.chars() {
                n = match map.nodes[n].next.binary_search_by_key(&c, |e| e.0) {
                    Ok(i) => map.nodes[n].next[i].1 as usize,
                    Err(i) => {
                        let id = map.nodes.len() as u32;
                        map.nodes.push(Node::default());
                        map.nodes[n].next.insert(i, (c, id));
                        id as usize
                    }
                };
            }
            map.nodes[n].entry = Some(map.entries.len() as u32);
            map.entries.push(Entry {
                keep_orig: *keep_orig,
                outputs,
            });
        }
        map
    }
}

/// Analyzes rule text with the synonym analyzer (`SynonymMap.Parser.analyze`): its terms joined
/// by `\0`. Lenient parsers turn failures into an empty phrase, which is then skipped.
struct RuleAnalyzer<'a> {
    analyzer: &'a crate::Analyzer,
    lenient: bool,
}

impl RuleAnalyzer<'_> {
    fn analyze(&self, text: &str) -> Result<String> {
        match self.analyze_strict(text) {
            Err(e) if self.lenient && e.kind == "illegal_argument_exception" => Ok(String::new()),
            r => r,
        }
    }

    fn analyze_strict(&self, text: &str) -> Result<String> {
        let mut out = String::new();
        let mut err = None;
        self.analyzer.try_analyze(text, &mut |t| {
            if err.is_some() {
                return;
            }
            if t.term.is_empty() {
                err = Some(AnalysisError::illegal(format!(
                    "term: {text} analyzed to a zero-length token"
                )));
                return;
            }
            if t.position_increment != 1 {
                err = Some(AnalysisError::illegal(format!(
                    "term: {text} analyzed to a token ({}) with position increment != 1 (got: {})",
                    t.term, t.position_increment
                )));
                return;
            }
            if !out.is_empty() {
                out.push(SEP);
            }
            out.push_str(t.term);
        })?;
        if let Some(e) = err {
            return Err(e);
        }
        if out.is_empty() {
            return Err(AnalysisError::illegal(format!(
                "term: {text} was completely eliminated by analyzer"
            )));
        }
        Ok(out)
    }
}

/// `SolrSynonymParser.split`: splits on `sep`, keeping backslash escapes, dropping empty parts.
fn solr_split(s: &str, sep: &str) -> Vec<String> {
    let mut list = Vec::new();
    let mut sb = String::new();
    let mut pos = 0;
    let bytes = s.as_bytes();
    while pos < s.len() {
        if s[pos..].starts_with(sep) {
            if !sb.is_empty() {
                list.push(std::mem::take(&mut sb));
            }
            pos += sep.len();
            continue;
        }
        let c = s[pos..].chars().next().unwrap_or('\0');
        pos += c.len_utf8();
        if c == '\\' {
            sb.push(c);
            if pos >= bytes.len() {
                break;
            }
            let c2 = s[pos..].chars().next().unwrap_or('\0');
            pos += c2.len_utf8();
            sb.push(c2);
            continue;
        }
        sb.push(c);
    }
    if !sb.is_empty() {
        list.push(sb);
    }
    list
}

fn solr_unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i < chars.len() - 1 {
            i += 1;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// `BufferedReader.readLine` over the joined rules: lines end at `\n`, `\r` or `\r\n`.
fn lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        if b[i] == b'\n' || b[i] == b'\r' {
            out.push(&text[start..i]);
            if b[i] == b'\r' && i + 1 < b.len() && b[i + 1] == b'\n' {
                i += 1;
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < b.len() {
        out.push(&text[start..]);
    }
    out
}

fn parse_exception(line: usize, cause: AnalysisError) -> AnalysisError {
    AnalysisError::other(
        "parse_exception",
        400,
        format!("Invalid synonym rule at line {line}"),
    )
    .with_cause(cause)
}

/// `ESSolrSynonymParser`.
fn parse_solr(text: &str, expand: bool, a: &RuleAnalyzer<'_>, b: &mut Builder) -> Result<()> {
    let lenient = a.lenient;
    let add = |b: &mut Builder, i: &str, o: &str, keep: bool| -> Result<()> {
        if !lenient || (!i.is_empty() && !o.is_empty()) {
            b.add(i, o, keep)?;
        }
        Ok(())
    };
    for (n, line) in lines(text).into_iter().enumerate() {
        let run = |b: &mut Builder| -> Result<()> {
            if line.is_empty() || line.starts_with('#') {
                return Ok(());
            }
            let sides = solr_split(line, "=>");
            if sides.len() > 1 {
                if sides.len() != 2 {
                    return Err(AnalysisError::illegal(
                        "more than one explicit mapping specified on the same line",
                    ));
                }
                let inputs = solr_split(&sides[0], ",")
                    .iter()
                    .map(|s| a.analyze(java_trim(&solr_unescape(s))))
                    .collect::<Result<Vec<_>>>()?;
                let outputs = solr_split(&sides[1], ",")
                    .iter()
                    .map(|s| a.analyze(java_trim(&solr_unescape(s))))
                    .collect::<Result<Vec<_>>>()?;
                for i in &inputs {
                    for o in &outputs {
                        add(b, i, o, false)?;
                    }
                }
            } else {
                let inputs = solr_split(line, ",")
                    .iter()
                    .map(|s| a.analyze(java_trim(&solr_unescape(s))))
                    .collect::<Result<Vec<_>>>()?;
                if expand {
                    for (x, i) in inputs.iter().enumerate() {
                        for (y, o) in inputs.iter().enumerate() {
                            if x != y {
                                add(b, i, o, true)?;
                            }
                        }
                    }
                } else {
                    for i in &inputs {
                        add(b, i, &inputs[0], false)?;
                    }
                }
            }
            Ok(())
        };
        run(b).map_err(|e| parse_exception(n + 1, e))?;
    }
    Ok(())
}

/// `ESWordnetSynonymParser`: `s(synset_id,w_num,'word',…).` lines grouped by synset id.
fn parse_wordnet(text: &str, expand: bool, a: &RuleAnalyzer<'_>, b: &mut Builder) -> Result<()> {
    let lenient = a.lenient;
    let add_synset = |b: &mut Builder, synset: &[String]| -> Result<()> {
        if synset.len() <= 1 {
            return Ok(());
        }
        let mut add = |i: &str, o: &str, keep: bool| -> Result<()> {
            if !lenient || (!i.is_empty() && !o.is_empty()) {
                b.add(i, o, keep)?;
            }
            Ok(())
        };
        if expand {
            for (x, i) in synset.iter().enumerate() {
                for (y, o) in synset.iter().enumerate() {
                    if x != y {
                        add(i, o, true)?;
                    }
                }
            }
        } else {
            for i in synset {
                add(i, &synset[0], false)?;
            }
        }
        Ok(())
    };
    let mut last_id = String::new();
    let mut synset: Vec<String> = Vec::new();
    let all = lines(text);
    let mut line_no = 0;
    for line in &all {
        line_no += 1;
        let units: Vec<u16> = line.encode_utf16().collect();
        if units.len() < 11 {
            return Err(AnalysisError::other(
                "string_index_out_of_bounds_exception",
                500,
                format!("Range [2, 11) out of bounds for length {}", units.len()),
            ));
        }
        let id = String::from_utf16_lossy(&units[2..11]);
        if id != last_id {
            add_synset(b, &synset).map_err(|e| parse_exception(line_no, e))?;
            synset.clear();
        }
        let start = line.find('\'').map_or(0, |i| i + 1);
        let end = line.rfind('\'');
        let word = match end {
            Some(e) if e >= start => line[start..e].replace("''", "'"),
            _ => {
                let start16 = line[..start].encode_utf16().count();
                let end16 = end.map_or(-1, |e| line[..e].encode_utf16().count() as i64);
                return Err(AnalysisError::other(
                    "string_index_out_of_bounds_exception",
                    500,
                    format!(
                        "Range [{start16}, {end16}) out of bounds for length {}",
                        units.len()
                    ),
                ));
            }
        };
        synset.push(a.analyze(&word).map_err(|e| parse_exception(line_no, e))?);
        last_id = id;
    }
    add_synset(b, &synset).map_err(|e| parse_exception(line_no, e))
}

// ---------------------------------------------------------------------------------------------
// SynonymGraphFilter.

#[derive(Default)]
struct OutTok {
    /// A buffered input token (when `orig`), or a synonym term with the match offsets.
    tok: Tok,
    orig: bool,
    start_node: i64,
    end_node: i64,
}

/// Fills a synonym output token: a fresh token with the match's offsets and type `SYNONYM`.
fn syn_fill(word: &str, start: u32, end: u32) -> impl FnOnce(&mut Tok) + '_ {
    move |t: &mut Tok| {
        t.clear();
        t.term.push_str(word);
        t.start = start;
        t.end = end;
        t.kind = TYPE_SYNONYM;
    }
}

/// `SynonymGraphFilter`.
pub(crate) struct SynonymGraph {
    map: Arc<SynonymMap>,
    look: VecDeque<Tok>,
    /// Absolute index of `look[0]`.
    look_base: usize,
    next_read: usize,
    next_write: usize,
    out: VecDeque<OutTok>,
    spare_out: Vec<OutTok>,
    spare_toks: Vec<Tok>,
    next_node_out: i64,
    last_node_out: i64,
    live: bool,
    match_start: u32,
    match_end: u32,
    finished: bool,
    paths: Vec<(u32, usize)>,
}

impl SynonymGraph {
    pub fn new(map: Arc<SynonymMap>) -> Self {
        SynonymGraph {
            map,
            look: VecDeque::new(),
            look_base: 0,
            next_read: 0,
            next_write: 0,
            out: VecDeque::new(),
            spare_out: Vec::new(),
            spare_toks: Vec::new(),
            next_node_out: 0,
            last_node_out: -1,
            live: false,
            match_start: 0,
            match_end: 0,
            finished: false,
            paths: Vec::new(),
        }
    }

    fn look_get(&self, pos: usize) -> &Tok {
        &self.look[pos - self.look_base]
    }

    fn max_pos(&self) -> Option<usize> {
        (!self.look.is_empty()).then(|| self.look_base + self.look.len() - 1)
    }

    fn free_before(&mut self, pos: usize) {
        while self.look_base < pos && !self.look.is_empty() {
            if let Some(t) = self.look.pop_front() {
                self.spare_toks.push(t);
            }
            self.look_base += 1;
        }
        if self.look.is_empty() {
            self.look_base = pos.max(self.look_base);
        }
    }

    fn capture(&mut self, t: &Tok) {
        self.live = false;
        let mut s = self.spare_toks.pop().unwrap_or_default();
        s.clone_from(t);
        if self.look.is_empty() {
            self.look_base = self.next_write;
        }
        self.look.push_back(s);
        self.next_write += 1;
    }

    fn release(&mut self, t: &mut Tok) {
        let Some(o) = self.out.pop_front() else {
            return;
        };
        t.clone_from(&o.tok);
        t.pos_inc = (o.start_node - self.last_node_out).max(0) as u32;
        self.last_node_out = o.start_node;
        t.pos_len = (o.end_node - o.start_node).max(1) as u32;
        self.spare_out.push(o);
    }

    /// Queues the buffered input token at `pos` (absolute) as an output.
    fn push_orig(&mut self, pos: usize, start_node: i64, end_node: i64) {
        let mut o = self.spare_out.pop().unwrap_or_default();
        o.tok.clone_from(&self.look[pos - self.look_base]);
        o.orig = true;
        o.start_node = start_node;
        o.end_node = end_node;
        self.out.push_back(o);
    }

    fn push_out(
        &mut self,
        fill: impl FnOnce(&mut Tok),
        orig: bool,
        start_node: i64,
        end_node: i64,
    ) {
        let mut o = self.spare_out.pop().unwrap_or_default();
        fill(&mut o.tok);
        o.orig = orig;
        o.start_node = start_node;
        o.end_node = end_node;
        self.out.push_back(o);
    }

    /// `parse()`: true when a match was found and outputs were buffered.
    fn parse(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        let map = self.map.clone();
        let mut match_entry: Option<u32> = None;
        let mut match_input_len = 0usize;
        let mut node = 0u32;
        let mut match_len = 0usize;
        let mut do_final_capture = false;
        let mut upto = self.next_read;
        let mut start_set = false;
        loop {
            let end_offset;
            let found;
            if self.max_pos().is_some_and(|m| upto <= m) {
                let tok = &self.look[upto - self.look_base];
                upto += 1;
                end_offset = tok.end;
                let tok_start = tok.start;
                found = map.walk(node, &tok.term);
                if !start_set {
                    self.match_start = tok_start;
                    start_set = true;
                }
            } else {
                if self.finished {
                    break;
                }
                if input.next(t) {
                    self.live = true;
                    if !start_set {
                        self.match_start = t.start;
                        start_set = true;
                    }
                    end_offset = t.end;
                    upto += 1;
                    found = map.walk(node, &t.term);
                } else {
                    self.finished = true;
                    break;
                }
            }
            match_len += 1;
            let Some(n) = found else { break };
            node = n;
            if let Some(e) = map.nodes[node as usize].entry {
                match_entry = Some(e);
                match_input_len = match_len;
                self.match_end = end_offset;
            }
            match map.step(node, SEP) {
                None => break,
                Some(n) => {
                    node = n;
                    do_final_capture = true;
                    if self.live {
                        self.capture(t);
                    }
                }
            }
        }
        if do_final_capture && self.live && !self.finished {
            self.capture(t);
        }
        let Some(entry) = match_entry else {
            return false;
        };
        if self.live {
            self.capture(t);
        }
        self.buffer_outputs(&map, entry, match_input_len);
        self.next_read += match_input_len;
        self.free_before(self.next_read);
        true
    }

    fn buffer_outputs(&mut self, map: &SynonymMap, entry: u32, match_input_len: usize) {
        let e = &map.entries[entry as usize];
        let keep_orig = e.keep_orig;
        let mut total_path_nodes: i64 = if keep_orig {
            match_input_len as i64 - 1
        } else {
            0
        };
        self.paths.clear();
        for &ord in &e.outputs {
            let words = map.words[ord as usize].split(SEP).count();
            self.paths.push((ord, words));
            total_path_nodes += words as i64 - 1;
        }
        let start_node = self.next_node_out;
        let end_node = start_node + total_path_nodes + 1;
        let mut new_node_count: i64 = 0;
        let first_out = self.out.len();
        for i in 0..self.paths.len() {
            let (ord, words) = self.paths[i];
            let path_end = if words == 1 {
                end_node
            } else {
                let p = self.next_node_out + new_node_count + 1;
                new_node_count += words as i64 - 1;
                p
            };
            let first = map.words[ord as usize].split(SEP).next().unwrap_or("");
            let fill = syn_fill(first, self.match_start, self.match_end);
            let (s, e2) = (start_node, path_end);
            let mut o = self.spare_out.pop().unwrap_or_default();
            fill(&mut o.tok);
            o.orig = false;
            o.start_node = s;
            o.end_node = e2;
            self.out.push_back(o);
        }
        if keep_orig {
            let input_end = if match_input_len == 1 {
                end_node
            } else {
                self.next_node_out + new_node_count + 1
            };
            self.push_orig(self.next_read, start_node, input_end);
        }
        self.next_node_out = end_node;
        for i in 0..self.paths.len() {
            let (ord, words) = self.paths[i];
            if words > 1 {
                let mut last_node = self.out[first_out + i].end_node;
                for (k, w) in map.words[ord as usize].split(SEP).enumerate().skip(1) {
                    let fill = syn_fill(w, self.match_start, self.match_end);
                    if k + 1 < words {
                        self.push_out(fill, false, last_node, last_node + 1);
                        last_node += 1;
                    } else {
                        self.push_out(fill, false, last_node, end_node);
                    }
                }
            }
        }
        if keep_orig && match_input_len > 1 {
            let mut last_node = self.out[first_out + self.paths.len()].end_node;
            for i in 1..match_input_len - 1 {
                self.push_orig(self.next_read + i, last_node, last_node + 1);
                last_node += 1;
            }
            self.push_orig(self.next_read + match_input_len - 1, last_node, end_node);
        }
    }
}

impl Filter for SynonymGraph {
    fn reset(&mut self) {
        while let Some(t) = self.look.pop_front() {
            self.spare_toks.push(t);
        }
        while let Some(o) = self.out.pop_front() {
            self.spare_out.push(o);
        }
        self.look_base = 0;
        self.next_read = 0;
        self.next_write = 0;
        self.last_node_out = -1;
        self.next_node_out = 0;
        self.finished = false;
        self.live = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !self.out.is_empty() {
            self.release(t);
            return true;
        }
        if self.parse(input, t) {
            self.release(t);
            return true;
        }
        if self.next_read == self.next_write {
            if self.finished {
                return false;
            }
            // The live token passes through.
            self.live = false;
        } else {
            t.clone_from(self.look_get(self.next_read));
            self.next_read += 1;
            self.free_before(self.next_read);
        }
        self.last_node_out += t.pos_inc as i64;
        self.next_node_out = self.last_node_out + t.pos_len as i64;
        true
    }
}

// ---------------------------------------------------------------------------------------------
// SynonymFilter (legacy).

#[derive(Default)]
struct PendingInput {
    tok: Tok,
    has_state: bool,
    keep_orig: bool,
    matched: bool,
    consumed: bool,
    start: u32,
    end: u32,
}

impl PendingInput {
    fn reset(&mut self) {
        self.has_state = false;
        self.consumed = true;
        self.keep_orig = false;
        self.matched = false;
    }
}

#[derive(Default)]
struct PendingOutputs {
    outputs: Vec<String>,
    end_offsets: Vec<i64>,
    pos_lens: Vec<u32>,
    upto: usize,
    count: usize,
    pos_incr: u32,
    last_end: i64,
    last_pos_len: u32,
}

impl PendingOutputs {
    fn reset(&mut self) {
        self.upto = 0;
        self.count = 0;
        self.pos_incr = 1;
    }

    fn add(&mut self, s: &str, end: i64, pos_len: u32) {
        if self.count == self.outputs.len() {
            self.outputs.push(String::new());
            self.end_offsets.push(0);
            self.pos_lens.push(0);
        }
        self.outputs[self.count].clear();
        self.outputs[self.count].push_str(s);
        self.end_offsets[self.count] = end;
        self.pos_lens[self.count] = pos_len;
        self.count += 1;
    }
}

/// `SynonymFilter`.
pub(crate) struct LegacySynonym {
    map: Arc<SynonymMap>,
    size: usize,
    inputs: Vec<PendingInput>,
    outputs: Vec<PendingOutputs>,
    next_read: usize,
    next_write: usize,
    input_skip: usize,
    finished: bool,
    last_start: u32,
    last_end: u32,
}

impl LegacySynonym {
    pub fn new(map: Arc<SynonymMap>) -> Self {
        let size = 1 + map.max_horizontal_context;
        let mut s = LegacySynonym {
            map,
            size,
            inputs: (0..size).map(|_| PendingInput::default()).collect(),
            outputs: (0..size).map(|_| PendingOutputs::default()).collect(),
            next_read: 0,
            next_write: 0,
            input_skip: 0,
            finished: false,
            last_start: 0,
            last_end: 0,
        };
        s.reset_state();
        s
    }

    fn incr(&self, i: usize) -> usize {
        if i + 1 == self.size { 0 } else { i + 1 }
    }

    fn reset_state(&mut self) {
        self.finished = false;
        self.input_skip = 0;
        self.next_read = 0;
        self.next_write = 0;
        for i in &mut self.inputs {
            i.reset();
        }
        for o in &mut self.outputs {
            o.reset();
        }
    }

    fn capture(&mut self, t: &Tok) {
        let w = self.next_write;
        let p = &mut self.inputs[w];
        p.tok.clone_from(t);
        p.has_state = true;
        p.consumed = false;
        self.next_write = self.incr(w);
    }

    fn parse(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        let map = self.map.clone();
        let mut cur = self.next_read;
        let mut match_entry = None;
        let mut match_input_len = 0;
        let mut match_end: i64 = -1;
        let mut node = 0u32;
        let mut count = 0;
        loop {
            let end_offset;
            let found;
            if cur == self.next_write {
                if self.finished {
                    break;
                }
                if input.next(t) {
                    let w = self.next_write;
                    self.inputs[w].start = t.start;
                    self.inputs[w].end = t.end;
                    self.last_start = t.start;
                    self.last_end = t.end;
                    end_offset = t.end;
                    if self.next_read != self.next_write {
                        self.capture(t);
                    } else {
                        self.inputs[w].consumed = false;
                    }
                    found = map.walk(node, &t.term);
                } else {
                    self.finished = true;
                    break;
                }
            } else {
                end_offset = self.inputs[cur].end;
                found = map.walk(node, &self.inputs[cur].tok.term);
            }
            count += 1;
            let Some(n) = found else { break };
            node = n;
            if let Some(e) = map.nodes[node as usize].entry {
                match_entry = Some(e);
                match_input_len = count;
                match_end = end_offset as i64;
            }
            match map.step(node, SEP) {
                None => break,
                Some(n) => {
                    node = n;
                    if self.next_read == self.next_write {
                        self.capture(t);
                    }
                }
            }
            cur = self.incr(cur);
        }
        if self.next_read == self.next_write && !self.finished {
            self.next_write = self.incr(self.next_write);
        }
        if let Some(e) = match_entry {
            self.input_skip = match_input_len;
            self.add_output(&map, e, match_input_len, match_end);
        } else if self.next_read != self.next_write {
            self.input_skip = 1;
        }
    }

    fn add_output(&mut self, map: &SynonymMap, entry: u32, match_input_len: usize, match_end: i64) {
        let e = &map.entries[entry as usize];
        let keep_orig = e.keep_orig;
        for &ord in &e.outputs {
            let phrase = &map.words[ord as usize];
            let mut upto = self.next_read;
            let nwords = phrase.split(SEP).count();
            for (i, w) in phrase.split(SEP).enumerate() {
                let (end, pos_len) = if i == nwords - 1 && i == 0 {
                    (
                        match_end,
                        if keep_orig { match_input_len as u32 } else { 1 },
                    )
                } else {
                    (-1, 1)
                };
                self.outputs[upto].add(w, end, pos_len);
                upto = self.incr(upto);
            }
        }
        let mut upto = self.next_read;
        for _ in 0..match_input_len {
            self.inputs[upto].keep_orig |= keep_orig;
            self.inputs[upto].matched = true;
            upto = self.incr(upto);
        }
    }
}

impl Filter for LegacySynonym {
    fn reset(&mut self) {
        self.reset_state();
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        loop {
            while self.input_skip != 0 {
                let r = self.next_read;
                let (consumed, keep_orig, matched) = (
                    self.inputs[r].consumed,
                    self.inputs[r].keep_orig,
                    self.inputs[r].matched,
                );
                if !consumed && (keep_orig || !matched) {
                    if self.inputs[r].has_state {
                        t.clone_from(&self.inputs[r].tok);
                    }
                    self.inputs[r].reset();
                    if self.outputs[r].count > 0 {
                        self.outputs[r].pos_incr = 0;
                    } else {
                        self.next_read = self.incr(r);
                        self.input_skip -= 1;
                    }
                    return true;
                } else if self.outputs[r].upto < self.outputs[r].count {
                    self.inputs[r].reset();
                    let o = &mut self.outputs[r];
                    let pos_incr = o.pos_incr;
                    let idx = o.upto;
                    o.last_end = o.end_offsets[idx];
                    o.last_pos_len = o.pos_lens[idx];
                    o.upto += 1;
                    o.pos_incr = 0;
                    t.clear();
                    t.term.push_str(&o.outputs[idx]);
                    if o.upto == o.count {
                        o.reset();
                    }
                    t.kind = TYPE_SYNONYM;
                    let end = if o.last_end == -1 {
                        self.inputs[r].end as i64
                    } else {
                        o.last_end
                    };
                    t.start = self.inputs[r].start;
                    t.end = end as u32;
                    t.pos_inc = pos_incr;
                    t.pos_len = o.last_pos_len;
                    if o.count == 0 {
                        self.next_read = self.incr(r);
                        self.input_skip -= 1;
                    }
                    return true;
                } else {
                    self.inputs[r].reset();
                    self.next_read = self.incr(r);
                    self.input_skip -= 1;
                }
            }
            if self.finished && self.next_read == self.next_write {
                let r = self.next_read;
                let o = &mut self.outputs[r];
                if o.upto < o.count {
                    let pos_incr = o.pos_incr;
                    let idx = o.upto;
                    o.upto += 1;
                    o.pos_incr = 0;
                    t.clear();
                    t.term.push_str(&o.outputs[idx]);
                    if o.upto == o.count {
                        o.reset();
                    }
                    let empty = o.count == 0;
                    self.inputs[r].reset();
                    if empty {
                        self.next_read = self.incr(r);
                        self.next_write = self.next_read;
                    }
                    t.start = self.last_start;
                    t.end = self.last_end;
                    t.kind = TYPE_SYNONYM;
                    t.pos_inc = pos_incr;
                    return true;
                }
                return false;
            }
            self.parse(input, t);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Factories.

#[derive(Clone)]
enum Source {
    Inline(Vec<String>),
    Set(Vec<String>),
    Path(String),
    /// Rules that fail to load: Elasticsearch only reads them when an analyzer uses the
    /// filter, so the error is reported then.
    Invalid(AnalysisError),
}

struct SynonymFactory {
    name: String,
    graph: bool,
    source: Source,
    expand: bool,
    wordnet: bool,
    lenient: bool,
    updateable: bool,
    sets: Option<Arc<dyn crate::SynonymSets>>,
}

impl SynonymFactory {
    fn rules(&self) -> Result<(String, String)> {
        match &self.source {
            Source::Inline(lines) => {
                let mut s = String::new();
                for l in lines {
                    s.push_str(l);
                    s.push('\n');
                }
                Ok((s, format!("'{}' analyzer settings", self.name)))
            }
            Source::Invalid(e) => Err(e.clone()),
            Source::Path(p) => Err(
                match crate::registry::file_failure(p, crate::registry::SERVER_READABLE) {
                    // The file is opened lazily, so a directory fails while parsing.
                    crate::registry::FileFailure::Directory(_) => {
                        AnalysisError::illegal(format!("failed to build synonyms from [{p}]"))
                            .with_cause(AnalysisError::other(
                                "i_o_exception",
                                500,
                                "Is a directory",
                            ))
                    }
                    _ => missing_file("synonyms_path", p),
                },
            ),
            Source::Set(sets) => {
                if !self.updateable {
                    return Err(AnalysisError::illegal(
                        "Can't apply [synonyms_set]! Loading synonyms from index is supported only for search time synonyms!",
                    ));
                }
                let mut uniq: Vec<&String> = Vec::new();
                for s in sets {
                    if !uniq.contains(&s) {
                        uniq.push(s);
                    }
                }
                if uniq.len() > 100 {
                    return Err(AnalysisError::illegal(
                        "At most 100 synonym sets may be specified in [synonyms_set]",
                    ));
                }
                let mut text = String::new();
                for set in &uniq {
                    match self.sets.as_ref().and_then(|s| s.rules(set)) {
                        Some(rules) => {
                            for r in rules {
                                text.push_str(&r);
                                text.push('\n');
                            }
                        }
                        None if self.lenient || self.sets.is_none() => {}
                        None => {
                            return Err(AnalysisError::other(
                                "resource_not_found_exception",
                                404,
                                format!("Synonym set [{set}] not found"),
                            ));
                        }
                    }
                }
                let names: Vec<&str> = uniq.iter().map(|s| s.as_str()).collect();
                Ok((
                    text,
                    format!("[{}] synonyms_sets in .synonyms index", names.join(", ")),
                ))
            }
        }
    }
}

/// A synonym filter specialized for its chain.
struct BuiltSynonym {
    map: Arc<SynonymMap>,
    graph: bool,
    mode: AnalysisMode,
}

impl FilterFactory for BuiltSynonym {
    fn create(&self) -> Box<dyn Filter> {
        if self.map.is_empty() {
            return Box::new(Passthrough);
        }
        if self.graph {
            Box::new(SynonymGraph::new(self.map.clone()))
        } else {
            Box::new(LegacySynonym::new(self.map.clone()))
        }
    }
    fn mode(&self) -> AnalysisMode {
        self.mode
    }
    fn synonym_role(&self) -> SynonymRole {
        SynonymRole::Identity
    }
}

struct Passthrough;

impl Filter for Passthrough {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        input.next(t)
    }
}

impl FilterFactory for SynonymFactory {
    fn create(&self) -> Box<dyn Filter> {
        // Only reachable outside an analyzer; Elasticsearch fails there.
        Box::new(Passthrough)
    }
    fn mode(&self) -> AnalysisMode {
        if self.updateable {
            AnalysisMode::SearchTime
        } else {
            AnalysisMode::All
        }
    }
    fn chain_aware(
        &self,
        prev: &ChainParts<'_>,
        _resolve: FilterResolver<'_>,
    ) -> Result<Option<Arc<dyn FilterFactory>>> {
        // The synonym analyzer: the preceding chain with each filter's synonym role applied.
        let mut filters: Vec<NamedFilter> = Vec::new();
        for f in prev.filters {
            match f.factory.synonym_role() {
                SynonymRole::Same => filters.push(f.clone()),
                SynonymRole::Identity => {}
                SynonymRole::Forbidden => {
                    return Err(AnalysisError::illegal(format!(
                        "Token filter [{}] cannot be used to parse synonyms",
                        f.name
                    )));
                }
                SynonymRole::Replace(r) => filters.push(NamedFilter {
                    name: f.name.clone(),
                    factory: r,
                }),
            }
        }
        let analyzer = AnalyzerSpec {
            name: "__synonyms__".into(),
            char_filters: prev.char_filters.to_vec(),
            tokenizer: prev.tokenizer.clone(),
            filters,
            custom: true,
            pos_gap: 0,
            offset_gap: 1,
            norm_filters: None,
        }
        .build()?;
        let (text, origin) = self.rules()?;
        let ra = RuleAnalyzer {
            analyzer: &analyzer,
            lenient: self.lenient,
        };
        let mut b = Builder::default();
        let parsed = if self.wordnet {
            parse_wordnet(&text, self.expand, &ra, &mut b)
        } else {
            parse_solr(&text, self.expand, &ra, &mut b)
        };
        if let Err(e) = parsed {
            return Err(AnalysisError::illegal(format!(
                "failed to build synonyms from [{origin}]"
            ))
            .with_cause(e));
        }
        Ok(Some(Arc::new(BuiltSynonym {
            map: Arc::new(b.build()),
            graph: self.graph,
            mode: self.mode(),
        })))
    }
}

/// `synonym` / `synonym_graph` (`SynonymTokenFilterFactory`).
pub(crate) fn factory(
    ctx: &Ctx,
    name: &str,
    s: &Settings,
    graph: bool,
) -> Result<Arc<dyn FilterFactory>> {
    let source = if s.get("synonyms").is_some() {
        // `Analysis.getWordList` reads `synonyms_path` first when both are set.
        match crate::registry::word_list(s, "synonyms") {
            Ok(Some(list)) => Source::Inline(list),
            // `getWordList` gives null for a value with no items, which the factory iterates.
            Ok(None) => Source::Invalid(AnalysisError::other(
                "null_pointer_exception",
                500,
                "Cannot invoke \"java.util.List.iterator()\" because \"rulesList\" is null",
            )),
            Err(e) => Source::Invalid(e),
        }
    } else if let Some(list) = s.get_list("synonyms_set") {
        Source::Set(list)
    } else if let Some(p) = s.get("synonyms_path") {
        Source::Path(p)
    } else {
        return Err(AnalysisError::illegal(
            "synonym requires either `synonyms`, `synonyms_set` or `synonyms_path` to be configured",
        ));
    };
    let expand = s.get_bool("expand", true)?;
    let wordnet = s
        .get("format")
        .is_some_and(|f| f.eq_ignore_ascii_case("wordnet"));
    let updateable = s.get_bool("updateable", false)?;
    let lenient = s.get_bool("lenient", updateable)?;
    Ok(Arc::new(SynonymFactory {
        name: name.to_string(),
        graph,
        source,
        expand,
        wordnet,
        lenient,
        updateable,
        sets: ctx.opts.synonym_sets.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::run_filters;

    fn map(rules: &[(&str, &str, bool)]) -> Arc<SynonymMap> {
        let mut b = Builder::default();
        for (i, o, k) in rules {
            b.add(&i.replace(' ', "\0"), &o.replace(' ', "\0"), *k)
                .unwrap();
        }
        Arc::new(b.build())
    }

    #[test]
    fn graph_multi_word() {
        // "ny" <=> "new york".
        let m = map(&[("ny", "new york", true), ("new york", "ny", true)]);
        let out = run_filters(vec![Box::new(SynonymGraph::new(m.clone()))], "in ny now");
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(
            terms,
            vec![("in", 1), ("new", 1), ("ny", 0), ("york", 1), ("now", 1)]
        );
        let out = run_filters(vec![Box::new(SynonymGraph::new(m))], "new york city");
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(terms, vec![("ny", 1), ("new", 0), ("york", 1), ("city", 1)]);
    }

    #[test]
    fn legacy_single_word() {
        let m = map(&[("quick", "fast", true), ("fast", "quick", true)]);
        let out = run_filters(vec![Box::new(LegacySynonym::new(m))], "the quick fox");
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(
            terms,
            vec![("the", 1), ("quick", 1), ("fast", 0), ("fox", 1)]
        );
    }

    #[test]
    fn solr_split_and_lines() {
        assert_eq!(solr_split("a, b\\,c ,d", ","), vec!["a", " b\\,c ", "d"]);
        assert_eq!(solr_unescape("b\\,c"), "b,c");
        assert_eq!(lines("a\nb\r\nc\rd\n"), vec!["a", "b", "c", "d"]);
    }
}
