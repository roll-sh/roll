//! The plain highlighter: Lucene's `Highlighter` (`getBestTextFragments`) with `QueryScorer`
//! token scores, `SimpleSpanFragmenter` / `SimpleFragmenter` / `NullFragmenter`, token groups
//! for overlapping tokens, and Elasticsearch's per-value fragment collection.

use std::collections::HashSet;

use search_analysis::Analyzer;

use crate::error::Result;
use crate::highlight::breaks::Content;
use crate::highlight::{Fragmenter, HighlightOptions, HighlightTerms, HlToken, analyze};

struct Frag {
    start: usize,
    end: usize,
    score: f32,
    num: usize,
}

/// One phrase of the query: its terms and the position spans (`[start, end]`, inclusive) of
/// its matches, as Lucene's `WeightedSpanTermExtractor` records them.
struct PhraseSpans {
    terms: HashSet<Vec<u8>>,
    spans: Vec<(u32, u32)>,
}

/// Lucene's `WeightedSpanTermExtractor` on phrases: a phrase is a `SpanNearQuery` of its
/// terms (a multi-phrase: of a `SpanOrQuery` per position) with the phrase's slop plus its
/// position gaps, in order only when the slop is 0; every match span is recorded. That is
/// not phrase matching: an unordered span near accepts any order and lets one position serve
/// two clauses, and Elasticsearch's plain highlighter shows exactly that.
fn phrase_spans(tokens: &[HlToken], q: &HighlightTerms) -> Vec<PhraseSpans> {
    let mut out = Vec::new();
    for (slots, slop) in &q.phrases {
        if slots.is_empty() {
            continue;
        }
        let terms: HashSet<Vec<u8>> = slots.iter().flat_map(|(_, ts)| ts.iter().cloned()).collect();
        let positions_of = |ts: &[Vec<u8>]| -> Vec<u32> {
            let mut p: Vec<u32> = tokens.iter().filter(|t| ts.contains(&t.term)).map(|t| t.position).collect();
            p.sort_unstable();
            p
        };
        let mut rels: Vec<u32> = slots.iter().map(|(r, _)| *r).collect();
        rels.sort_unstable();
        let repeated = rels.windows(2).any(|w| w[0] == w[1]);
        let (clauses, gaps): (Vec<Vec<u32>>, i64) = if !repeated && slots.iter().all(|(_, ts)| ts.len() == 1) {
            // PhraseQuery: one span term per term; gaps beyond consecutive positions.
            let mut sorted: Vec<&(u32, Vec<Vec<u8>>)> = slots.iter().collect();
            sorted.sort_by_key(|(r, _)| *r);
            let n = sorted.len() as i64;
            let gaps = (sorted[sorted.len() - 1].0 as i64 - sorted[0].0 as i64 - n + 1).max(0);
            (sorted.iter().map(|(_, ts)| positions_of(ts)).collect(), gaps)
        } else {
            // MultiPhraseQuery: a span or per distinct position; empty positions are gaps.
            let max = rels[rels.len() - 1];
            let mut clauses = Vec::new();
            let mut gaps = 0i64;
            for p in 0..=max {
                let ts: Vec<Vec<u8>> = slots.iter().filter(|(r, _)| *r == p).flat_map(|(_, ts)| ts.iter().cloned()).collect();
                if ts.is_empty() {
                    gaps += 1;
                } else {
                    clauses.push(positions_of(&ts));
                }
            }
            (clauses, gaps)
        };
        let spans = if clauses.len() == 1 {
            // A single span term (or span or): every position of it.
            clauses[0].iter().map(|&p| (p, p)).collect()
        } else {
            span_near(&clauses, *slop as i64 + gaps, *slop == 0)
        };
        out.push(PhraseSpans { terms, spans });
    }
    out
}

/// Lucene's `NearSpansOrdered` / `NearSpansUnordered` over clauses whose spans are single
/// positions (`[p, p + 1)`, sorted): `(start, end)` (inclusive) of every match, in order.
fn span_near(clauses: &[Vec<u32>], slop: i64, ordered: bool) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    if clauses.iter().any(|c| c.is_empty()) {
        return out;
    }
    let n = clauses.len();
    // Per clause: its current position and the index of the next one.
    let mut cur = vec![-1i64; n];
    let mut next = vec![0usize; n];
    let mut step = |i: usize, cur: &mut [i64]| -> bool {
        match clauses[i].get(next[i]) {
            Some(&p) => {
                cur[i] = p as i64;
                next[i] += 1;
                true
            }
            None => false,
        }
    };
    if ordered {
        // Each later clause starts at or after the previous one's end; the gaps between them
        // add up to the width.
        'lead: while step(0, &mut cur) {
            let start = cur[0];
            let mut prev_end = start + 1;
            let mut width = 0;
            for i in 1..n {
                while cur[i] < prev_end {
                    if !step(i, &mut cur) {
                        break 'lead;
                    }
                }
                width += cur[i] - prev_end;
                prev_end = cur[i] + 1;
            }
            if width <= slop {
                out.push((start as u32, (prev_end - 1) as u32));
            }
        }
        return out;
    }
    // A window of one span per clause, in (start, end) order: it matches when the window's
    // length minus the clauses' lengths is at most the slop; then its first span moves on.
    let total = n as i64;
    let mut max_end = -1i64;
    for i in 0..n {
        step(i, &mut cur);
        max_end = max_end.max(cur[i] + 1);
    }
    // Ties keep the earlier clause first (a heap's top among equal spans).
    let top = |cur: &[i64]| (0..n).min_by_key(|&i| (cur[i], i)).unwrap();
    loop {
        let t = top(&cur);
        if max_end - cur[t] - total <= slop {
            out.push((cur[t] as u32, (max_end - 1) as u32));
        }
        if !step(t, &mut cur) {
            break;
        }
        max_end = max_end.max(cur[t] + 1);
    }
    out
}

fn fragments_of(text: &str, analyzer: Option<&Analyzer>, q: &HighlightTerms, opts: &HighlightOptions, num_base: usize) -> Vec<(String, f32, usize)> {
    let content = Content::new(text);
    let len = content.len16() as usize;
    let tokens = analyze(&[text], analyzer, opts.max_analyzed_offset, opts.tokenized);
    let phrases = phrase_spans(&tokens, q);
    // Lucene's QueryScorer: phrase terms only score inside a match span.
    let token_score = |t: &HlToken| -> f32 {
        let in_phrase =
            || phrases.iter().any(|p| p.terms.contains(&t.term) && p.spans.iter().any(|&(s, e)| s <= t.position && t.position <= e));
        if q.terms.contains(&t.term) || q.multi.iter().any(|m| m(&t.term)) || in_phrase() {
            1.0
        } else {
            0.0
        }
    };
    let size = opts.fragment_size;
    let mut out = String::new();
    let mut frags: Vec<Frag> = vec![Frag { start: 0, end: 0, score: 0.0, num: 0 }];
    let mut found: HashSet<Vec<u8>> = HashSet::new();
    let mut total = 0f32;
    // Token group: (start, end, total score).
    let mut group: Option<(u32, u32, f32)> = None;
    let mut last_end = 0u32;
    let mut cur_frags = 1usize;
    let mut wait_for: i64 = -1;
    // SimpleSpanFragmenter's own position counter, which (as in Lucene) only advances on the
    // tokens it is asked about: the first token of every group but the first.
    let mut frag_pos: i64 = -1;
    let flush = |g: (u32, u32, f32), out: &mut String, last_end: &mut u32| {
        let (s, e, sc) = g;
        if s > *last_end {
            opts.encoder.encode(content.slice(*last_end, s), out);
        }
        let mut t = String::new();
        opts.encoder.encode(content.slice(s, e), &mut t);
        if sc > 0.0 {
            out.push_str(opts.pre_tag());
            out.push_str(&t);
            out.push_str(opts.post_tag());
        } else {
            out.push_str(&t);
        }
        *last_end = (*last_end).max(e);
    };
    for t in &tokens {
        if let Some(g) = group
            && t.start >= g.1
        {
            flush(g, &mut out, &mut last_end);
            group = None;
            // Fragmenter decision on the token that starts the next group.
            let new_frag = match opts.fragmenter {
                _ if opts.number_of_fragments == 0 => false,
                Fragmenter::Simple => {
                    let n = t.end as usize >= size * cur_frags;
                    if n {
                        cur_frags += 1;
                    }
                    n
                }
                Fragmenter::Span => {
                    frag_pos += t.position_increment as i64;
                    let position = frag_pos;
                    let mut blocked = false;
                    if wait_for <= position {
                        wait_for = -1;
                    } else if wait_for != -1 {
                        blocked = true;
                    }
                    if blocked {
                        false
                    } else {
                        // Keep a phrase in one fragment: wait until past its span.
                        if let Some(&(_, e)) = phrases
                            .iter()
                            .filter(|p| p.terms.contains(&t.term))
                            .flat_map(|p| p.spans.iter())
                            .find(|&&(s, _)| s as i64 == position)
                        {
                            wait_for = e as i64 + 1;
                        }
                        let n = t.end as usize >= size * cur_frags && len.saturating_sub(t.end as usize) >= (size >> 1);
                        if n {
                            cur_frags += 1;
                        }
                        n
                    }
                }
            };
            if new_frag {
                let f = frags.last_mut().unwrap();
                f.score = total;
                f.end = out.len();
                frags.push(Frag { start: out.len(), end: 0, score: 0.0, num: frags.len() });
                found.clear();
                total = 0.0;
            }
        }
        let sc = token_score(t);
        if sc > 0.0 && found.insert(t.term.clone()) {
            total += sc;
        }
        group = Some(match group {
            None => (t.start, t.end, sc),
            Some((s, e, tot)) => (s.min(t.start), e.max(t.end), tot + sc),
        });
    }
    frags.last_mut().unwrap().score = total;
    if let Some(g) = group {
        flush(g, &mut out, &mut last_end);
    }
    if (last_end as usize) < len {
        opts.encoder.encode(content.slice(last_end, len as u32), &mut out);
    }
    frags.last_mut().unwrap().end = out.len();
    let n = if opts.number_of_fragments == 0 { 1 } else { opts.number_of_fragments };
    // Best fragments: score desc, then fragment order.
    let mut order: Vec<&Frag> = frags.iter().collect();
    order.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.num.cmp(&b.num)));
    order.truncate(n);
    order.retain(|f| f.score > 0.0);
    order.sort_by_key(|f| f.num);
    order.into_iter().map(|f| (out[f.start..f.end].to_string(), f.score, f.num + num_base)).collect()
}

pub fn highlight(values: &[&str], analyzer: Option<&Analyzer>, q: &HighlightTerms, opts: &HighlightOptions) -> Result<Vec<String>> {
    let mut all: Vec<(String, f32, usize)> = Vec::new();
    let mut base = 0;
    for v in values {
        let fr = fragments_of(v, analyzer, q, opts, base);
        base += fr.len().max(1);
        all.extend(fr);
    }
    // Fragments come out in field order (value, then position) unless `order: score`.
    if opts.order_by_score {
        all.sort_by(|a, b| b.1.total_cmp(&a.1));
    }
    let out: Vec<String> = if opts.number_of_fragments == 0 && values.len() > 1 {
        all.into_iter().map(|f| f.0).collect()
    } else {
        let n = if opts.number_of_fragments == 0 { 1 } else { opts.number_of_fragments };
        all.into_iter().take(n).map(|f| f.0).collect()
    };
    if !out.is_empty() || opts.no_match_size == 0 {
        return Ok(out);
    }
    // no_match_size: the start of the first value, ending at a token boundary.
    let first = values[0];
    let content = Content::new(first);
    let tokens = analyze(&[first], analyzer, None, opts.tokenized);
    let mut end = 0u32;
    for t in &tokens {
        if t.end as usize > opts.no_match_size {
            break;
        }
        end = t.end;
    }
    if tokens.is_empty() || end == 0 {
        end = (opts.no_match_size as u32).min(content.len16());
    }
    if end > 0 {
        let mut s = String::new();
        opts.encoder.encode(content.slice(0, end), &mut s);
        return Ok(vec![s]);
    }
    Ok(Vec::new())
}
