//! The unified highlighter: Lucene's `FieldHighlighter` passage selection, `PassageScorer`
//! and Elasticsearch's `CustomPassageFormatter` / `CustomFieldHighlighter` behaviour.

use std::collections::HashMap;

use search_analysis::Analyzer;

use crate::error::Result;
use crate::highlight::breaks::{Boundaries, Bounded, BreakIter, Content, DONE};
use crate::highlight::{BoundaryScanner, HighlightOptions, HighlightTerms, HlMatch, analyze, find_matches};

/// A passage being built.
#[derive(Clone, Debug, Default)]
struct Passage {
    start: i64,
    end: i64,
    score: f32,
    matches: Vec<(u32, u32, usize)>,
}

impl Passage {
    fn new() -> Passage {
        Passage { start: -1, end: -1, score: 0.0, matches: Vec::new() }
    }
}

/// Lucene's `PassageScorer` (k1 1.2, b 0.75, pivot 87).
struct PassageScorer;

impl PassageScorer {
    const K1: f32 = 1.2;
    const B: f32 = 0.75;
    const PIVOT: f32 = 87.0;

    fn weight(content_len: i64, ttf: u32) -> f32 {
        let num_docs = 1.0 + content_len as f32 / Self::PIVOT;
        (Self::K1 + 1.0) * (1.0 + (num_docs as f64 + 0.5) / (ttf as f64 + 0.5)).ln() as f32
    }

    fn tf(freq: u32, passage_len: i64) -> f32 {
        let norm = Self::K1 * ((1.0 - Self::B) + Self::B * (passage_len as f32 / Self::PIVOT));
        freq as f32 / (freq as f32 + norm)
    }

    fn norm(start: i64) -> f32 {
        1.0 + 1.0 / ((Self::PIVOT as f64 + start as f64).ln() as f32)
    }

    /// Every match counts as one occurrence in the doc: Lucene's matches-based offsets
    /// (weight matches, which Elasticsearch uses) report `freq() == 1`.
    fn score(p: &Passage, content_len: i64) -> f32 {
        let mut per: HashMap<usize, u32> = HashMap::new();
        let mut order = Vec::new();
        for &(_, _, k) in &p.matches {
            let e = per.entry(k).or_insert_with(|| {
                order.push(k);
                0
            });
            *e += 1;
        }
        let mut s = 0f64;
        for k in order {
            s += (Self::tf(per[&k], p.end - p.start) * Self::weight(content_len, 1)) as f64;
        }
        s *= Self::norm(p.start) as f64;
        s as f32
    }
}

pub fn highlight(values: &[&str], analyzer: Option<&Analyzer>, q: &HighlightTerms, opts: &HighlightOptions) -> Result<Vec<String>> {
    let joined = values.join("\0");
    let content = Content::new(&joined);
    let len = content.len16() as i64;
    if len == 0 {
        return Ok(Vec::new());
    }
    let tokens = analyze(values, analyzer, opts.max_analyzed_offset, opts.tokenized);
    let matches: Vec<HlMatch> = find_matches(&tokens, q);
    // Keys and their frequencies in the doc.
    let mut key_of: HashMap<&[u8], usize> = HashMap::new();
    let mut ms: Vec<(u32, u32, usize)> = Vec::with_capacity(matches.len());
    for m in &matches {
        let n = key_of.len();
        let k = *key_of.entry(&m.key).or_insert(n);
        ms.push((m.start, m.end, k));
    }
    let n_frag = opts.number_of_fragments;
    let (mut bi, max_passages) = if n_frag == 0 || !opts.tokenized {
        (BreakIter::Separator(Boundaries::separators(&content)), if n_frag == 0 { usize::MAX - 1 } else { n_frag })
    } else {
        let b = match opts.boundary_scanner.unwrap_or(BoundaryScanner::Sentence) {
            BoundaryScanner::Sentence => {
                if opts.fragment_size > 0 {
                    BreakIter::Bounded(Bounded::new(
                        Boundaries::sentences(&content),
                        Boundaries::words(&content),
                        opts.fragment_size,
                        &content,
                    ))
                } else {
                    BreakIter::Plain(Boundaries::sentences(&content))
                }
            }
            BoundaryScanner::Word | BoundaryScanner::Chars => BreakIter::Plain(Boundaries::words(&content)),
        };
        (b, n_frag)
    };
    let mut passages: Vec<Passage> = Vec::new();
    if !ms.is_empty() {
        // Lucene FieldHighlighter.highlightOffsetsEnums.
        let mut queue: Vec<Passage> = Vec::new();
        let mut passage = Passage::new();
        let mut last_end = 0i64;
        let add = |queue: &mut Vec<Passage>, mut p: Passage| -> Passage {
            if p.start == -1 {
                return p;
            }
            p.score = PassageScorer::score(&p, len);
            let worst = |q: &Vec<Passage>| {
                q.iter().enumerate().min_by(|a, b| a.1.score.total_cmp(&b.1.score).then(b.1.start.cmp(&a.1.start))).map(|x| x.0)
            };
            if queue.len() == max_passages
                && let Some(w) = worst(queue)
                && p.score < queue[w].score
            {
                return Passage::new();
            }
            queue.push(p);
            if queue.len() > max_passages
                && let Some(w) = worst(queue)
            {
                queue.swap_remove(w);
            }
            Passage::new()
        };
        for &(start, end, k) in &ms {
            let (start, end) = (start as i64, end as i64);
            if start < len && end > len {
                continue;
            }
            if start >= passage.end {
                passage = add(&mut queue, std::mem::take(&mut passage));
                if start >= len {
                    break;
                }
                let center = start + (end - start) / 2;
                let pre = bi.preceding((start + 1).max(center));
                passage.start = start.min(pre.max(last_end));
                let fol = bi.following((end - 1).min(center));
                last_end = end.max(fol.min(len));
                passage.end = last_end;
            }
            passage.matches.push((start as u32, end as u32, k));
        }
        let _ = add(&mut queue, passage);
        passages = queue;
        if opts.order_by_score {
            passages.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.start.cmp(&b.start)));
        } else {
            passages.sort_by_key(|p| p.start);
        }
    }
    if passages.is_empty() {
        // Elasticsearch's no_match_size summary: the first value, cut at a word boundary.
        if opts.no_match_size == 0 {
            return Ok(Vec::new());
        }
        let mut pos = 0u32;
        while pos < content.len16() && content.char_at(pos) == Some('\0') {
            pos += 1;
        }
        if pos >= content.len16() {
            return Ok(Vec::new());
        }
        let mut end = content.slice(pos, content.len16()).find('\0').map_or(content.len16(), |b| content.u16(content.byte(pos) + b));
        if (opts.no_match_size as u32 + pos) < end {
            let mut w = Boundaries::words(&content);
            let f = w.following((opts.no_match_size as u32 + pos) as i64);
            end = if f == DONE { content.len16() } else { f as u32 };
        }
        passages.push(Passage { start: pos as i64, end: end as i64, score: f32::NAN, matches: Vec::new() });
    }
    // Elasticsearch's CustomPassageFormatter.
    let mut out = Vec::with_capacity(passages.len());
    for p in &passages {
        let mut sb = String::new();
        let mut pos = p.start as u32;
        let mut i = 0;
        while i < p.matches.len() {
            let (start, mut end, _) = p.matches[i];
            opts.encoder.encode(content.slice(pos, start.max(pos)), &mut sb);
            while i + 1 < p.matches.len() && p.matches[i + 1].0 < end {
                i += 1;
                end = end.max(p.matches[i].1);
            }
            end = end.min(p.end as u32);
            sb.push_str(opts.pre_tag());
            opts.encoder.encode(content.slice(start, end), &mut sb);
            sb.push_str(opts.post_tag());
            pos = end;
            i += 1;
        }
        opts.encoder.encode(content.slice(pos, (p.end as u32).max(pos)), &mut sb);
        let mut text = sb.replace('\0', " ");
        if text.ends_with('\u{2029}') {
            text.pop();
        }
        if n_frag > 0 {
            text = text.trim_matches(|c: char| c <= ' ').to_string();
        }
        if text.chars().any(|c| !c.is_whitespace()) {
            out.push(text);
        }
    }
    Ok(out)
}
