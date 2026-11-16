//! The `analysis-stempel` plugin: the `polish_stem` and `polish_stop` token filters and the
//! `polish` analyzer (Elasticsearch's `PolishStemTokenFilterFactory`,
//! `PolishStopTokenFilterFactory` and `PolishAnalyzerProvider` over Lucene's `StempelFilter`
//! and `PolishAnalyzer`).
//!
//! The stemmer is Egothor's (`org.egothor.stemmer`, Apache-2.0, as Lucene ships it): a
//! `MultiTrie2` of patch commands looked up from the end of the word, applied by `Diff.apply`.
//! Its table is Lucene's `stemmer_20000.tbl` (Apache-2.0) converted by `tools/gen_stempel.py`
//! into `stemmer_20000.bin` (763 KB), which is read where it lies in the binary: an index that
//! never stems Polish never touches it, and nothing is decoded onto the heap. The stop words
//! are Lucene's `pl/stopwords.txt` (from Carrot2, BSD).

use std::sync::{Arc, OnceLock};

use crate::analyzer::Analyzer;
use crate::analyzers::{AnalyzerBuilder, builtin, lowercase, standard_tokenizer, stop_filter};
use crate::component::{FilterFactory, filter_with, keyword_aware_fn};
use crate::error::Result;
use crate::filter::WordSet;
use crate::filter::basic;
use crate::registry::{Ctx, FilterBuilder, Provider, word_list};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::{Tok, attr};

mod stopwords;

use stopwords::POLISH_STOP;

static TABLE: &[u8] = include_bytes!("stemmer_20000.bin");

/// One trie of the table, as slices of [`TABLE`].
struct Trie {
    root: u16,
    cmd_ends: &'static [u8],
    cmds: &'static [u8],
    row_ends: &'static [u8],
    cells: &'static [u8],
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[2 * i], b[2 * i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
}

impl Trie {
    fn cmd(&self, i: usize) -> &'static str {
        let start = if i == 0 { 0 } else { u16_at(self.cmd_ends, i - 1) as usize };
        let end = u16_at(self.cmd_ends, i) as usize;
        std::str::from_utf8(&self.cmds[start..end]).unwrap_or("")
    }

    /// The (command, row) of `ch` in a row: indexes plus one, 0 for none.
    fn cell(&self, row: usize, ch: u16) -> (u16, u16) {
        let start = if row == 0 { 0 } else { u32_at(self.row_ends, row - 1) as usize };
        let end = u32_at(self.row_ends, row) as usize;
        let (mut lo, mut hi) = (start, end);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let c = u16_at(self.cells, 3 * mid);
            match c.cmp(&ch) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return (u16_at(self.cells, 3 * mid + 1), u16_at(self.cells, 3 * mid + 2)),
            }
        }
        (0, 0)
    }

    /// `Trie.getLastOnPath` (the table's tries read keys backwards).
    fn last_on_path(&self, key: &[u16]) -> Option<&'static str> {
        let mut now = self.root as usize;
        let mut last = None;
        let n = key.len();
        if n == 0 {
            return None;
        }
        for i in 0..n - 1 {
            let (cmd, r) = self.cell(now, key[n - 1 - i]);
            if cmd > 0 {
                last = Some(self.cmd(cmd as usize - 1));
            }
            if r == 0 {
                return last;
            }
            now = r as usize - 1;
        }
        match self.cell(now, key[0]).0 {
            0 => last,
            c => Some(self.cmd(c as usize - 1)),
        }
    }
}

fn tries() -> &'static [Trie] {
    static T: OnceLock<Vec<Trie>> = OnceLock::new();
    T.get_or_init(|| {
        let b = TABLE;
        let mut p = 1;
        let mut out = Vec::new();
        for _ in 0..b[0] {
            let root = u16_at(&b[p..], 0);
            let ncmds = u16_at(&b[p..], 1) as usize;
            p += 4;
            let cmd_ends = &b[p..p + 2 * ncmds];
            p += 2 * ncmds;
            let len = if ncmds == 0 { 0 } else { u16_at(cmd_ends, ncmds - 1) as usize };
            let cmds = &b[p..p + len];
            p += len;
            let nrows = u16_at(&b[p..], 0) as usize;
            p += 2;
            let row_ends = &b[p..p + 4 * nrows];
            p += 4 * nrows;
            let ncells = if nrows == 0 { 0 } else { u32_at(row_ends, nrows - 1) as usize };
            let cells = &b[p..p + 6 * ncells];
            p += 6 * ncells;
            out.push(Trie { root, cmd_ends, cmds, row_ends, cells });
        }
        out
    })
}

/// `MultiTrie2.lengthPP`; None where Java runs off the command (caught by the caller).
fn length_pp(cmd: &[u16]) -> Option<usize> {
    let mut len: i64 = 0;
    let mut i = 0;
    while i < cmd.len() {
        let c = cmd[i];
        i += 1;
        match c {
            0x2D | 0x44 => len += *cmd.get(i)? as i64 - 'a' as i64 + 1,
            0x52 => len += 1,
            _ => {}
        }
        i += 1;
    }
    usize::try_from(len).ok()
}

/// `MultiTrie2.getLastOnPath`: the commands of each trie joined, stopping at `*`, a command
/// that cannot follow the previous one, or an index error (which Java catches).
fn command(key: &[u16]) -> Vec<u16> {
    let tries = tries();
    let mut result: Vec<u16> = Vec::new();
    let mut key: &[u16] = key;
    let mut lastkey: &[u16] = key;
    let mut prev: Vec<u16> = Vec::new();
    let mut lastch = ' ' as u16;
    for trie in tries {
        let Some(r) = trie.last_on_path(lastkey) else { return result };
        if r == "*" {
            return result;
        }
        let r: Vec<u16> = r.encode_utf16().collect();
        // `cannotFollow`.
        if matches!(lastch, 0x2D | 0x44) && lastch == r[0] {
            return result;
        }
        if r.len() < 2 {
            return result;
        }
        lastch = r[r.len() - 2];
        if r[0] == '-' as u16 {
            for c in [(!prev.is_empty()).then_some(&prev), Some(&r)].into_iter().flatten() {
                let Some(n) = length_pp(c).filter(|&n| n <= key.len()) else { return result };
                key = &key[..key.len() - n];
            }
        }
        result.extend_from_slice(&r);
        if !key.is_empty() {
            lastkey = key;
        }
        prev = r;
    }
    result
}

/// `Diff.apply`: the commands run backwards from the end of the word until one fails.
fn apply(dest: &mut Vec<u16>, diff: &[u16]) {
    let Some(mut pos) = (dest.len() as i64).checked_sub(1).filter(|&p| p >= 0) else { return };
    for i in 0..diff.len() / 2 {
        let cmd = diff[2 * i];
        let param = diff[2 * i + 1];
        let par_num = param as i64 - 'a' as i64 + 1;
        match cmd {
            0x2D => pos = pos - par_num + 1,
            0x52 => {
                if pos < 0 || pos >= dest.len() as i64 {
                    return;
                }
                dest[pos as usize] = param;
            }
            0x44 => {
                let end = (pos + 1).min(dest.len() as i64);
                pos -= par_num - 1;
                if pos < 0 || pos > end {
                    return;
                }
                dest.drain(pos as usize..end as usize);
            }
            0x49 => {
                pos += 1;
                if pos < 0 || pos > dest.len() as i64 {
                    return;
                }
                dest.insert(pos as usize, param);
            }
            _ => {}
        }
        pos -= 1;
    }
}

/// `StempelStemmer.stem`: None when the word is left alone.
pub(crate) fn stem(word: &str) -> Option<String> {
    let w: Vec<u16> = word.encode_utf16().collect();
    let cmd = command(&w);
    let mut buf = w;
    apply(&mut buf, &cmd);
    (!buf.is_empty()).then(|| String::from_utf16_lossy(&buf))
}

/// `StempelFilter` (words of at least 3 UTF-16 units that aren't keywords).
struct Stempel;

impl Filter for Stempel {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        if !t.keyword
            && t.term.encode_utf16().count() >= 3
            && let Some(s) = stem(&t.term)
        {
            t.term = s;
        }
        true
    }
}

fn stem_filter() -> Arc<dyn FilterFactory> {
    keyword_aware_fn(|| Box::new(Stempel))
}


fn default_stop() -> &'static WordSet {
    static S: OnceLock<WordSet> = OnceLock::new();
    S.get_or_init(|| WordSet::new(POLISH_STOP.iter().copied(), false))
}

/// `polish_stem` and `polish_stop`.
pub(crate) fn filter_provider(ty: &str) -> Option<Provider<FilterBuilder>> {
    let build: FilterBuilder = match ty {
        "polish_stem" => |_, _, _| Ok(stem_filter()),
        "polish_stop" => |_, _, s| {
            let ic = s.get_bool("ignore_case", false)?;
            let remove_trailing = s.get_bool("remove_trailing", true)?;
            let words = Arc::new(stop_words(s, ic)?);
            Ok(if remove_trailing {
                crate::component::filter_fn(move || Box::new(basic::stop(words.clone())))
            } else {
                filter_with(false, attr::KEYWORD, crate::component::SynonymRole::Same, move || {
                    Box::new(basic::SuggestStop::new(words.clone()))
                })
            })
        },
        _ => return None,
    };
    Some(Provider { build, requires_settings: false })
}

/// `Analysis.parseWords` with `_polish_` as the only named list.
fn stop_words(s: &Settings, ignore_case: bool) -> Result<WordSet> {
    let list = match s.get("stopwords") {
        Some(v) if v == "_none_" => return Ok(WordSet::new(std::iter::empty::<&str>(), ignore_case)),
        Some(_) => s.list_or_empty("stopwords"),
        None => match word_list(s, "stopwords")? {
            Some(l) => l,
            None => return Ok(default_stop().clone()),
        },
    };
    let mut set = WordSet::new(std::iter::empty::<&str>(), ignore_case);
    for w in &list {
        if w == "_polish_" {
            for x in POLISH_STOP {
                set.add(x);
            }
        } else {
            set.add(w);
        }
    }
    Ok(set)
}

/// The `polish` analyzer, which ignores its settings.
pub(crate) fn analyzer_provider(ty: &str) -> Option<AnalyzerBuilder> {
    (ty == "polish").then_some(|_: &Ctx, name: &str, _: &Settings| polish(name))
}

fn polish(name: &str) -> Result<Analyzer> {
    builtin(
        name,
        standard_tokenizer(255),
        vec![lowercase(), stop_filter(default_stop().clone()), stem_filter()],
        vec![lowercase()],
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn stems_like_lucene() {
        // Captured from Lucene's `StempelStemmer` over its default table.
        for (w, s) in [("studenci", "student"), ("studenta", "student")] {
            assert_eq!(super::stem(w).as_deref(), Some(s), "{w}");
        }
    }
}
