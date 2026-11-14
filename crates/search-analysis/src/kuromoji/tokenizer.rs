//! `JapaneseTokenizer` and its `ViterbiNBest` (Lucene 10.5.1): segmentation by the lowest-cost
//! path through IPADIC, with search mode's second-best segmentation of long compounds,
//! extended mode's unigrams of unknown words, user dictionaries and n-best output.

use std::sync::Arc;

use super::dict::{self, JaDict, NGRAM, UserDict};
use super::JaMorph;
use crate::morph::viterbi::{self, Core, Fwd, In, Lang, MAX_UNKNOWN_WORD_LENGTH, Ty};
use crate::morph::{Fst, Morph, System, char_type, is_punct_type};
use crate::stream::Tokenizer;
use crate::token::Tok;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Normal,
    Search,
    Extended,
}

/// The tokenizer's settings.
#[derive(Clone)]
pub(crate) struct Options {
    pub mode: Mode,
    pub discard_punctuation: bool,
    pub discard_compound_token: bool,
    pub user: Option<Arc<UserDict>>,
    pub nbest_cost: i32,
}

/// A pending token: a span of the input with its dictionary entry.
#[derive(Clone, Copy, Debug)]
struct JaTok {
    start: i32,
    len: i32,
    word: i32,
    ty: Ty,
    pos_len: i32,
}

const SEARCH_MODE_KANJI_LENGTH: i32 = 2;
const SEARCH_MODE_OTHER_LENGTH: i32 = 7;
const SEARCH_MODE_KANJI_PENALTY: i32 = 3000;
const SEARCH_MODE_OTHER_PENALTY: i32 = 1700;

pub(crate) fn is_punctuation(c: u16) -> bool {
    is_punct_type(char_type(c))
}

fn is_low_surrogate(c: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&c)
}

struct JaLang {
    dict: &'static JaDict,
    user: Option<Arc<UserDict>>,
    discard_punctuation: bool,
    search: bool,
    extended: bool,
    compounds: bool,
    nbest_cost: i32,
    pending: Vec<JaTok>,
    lattice: Lattice,
}

impl Lang for JaLang {
    fn system(&self) -> &System {
        &self.dict.sys
    }
    fn user_fst(&self) -> Option<&Fst> {
        self.user.as_deref().map(|u| &u.fst)
    }
    fn left_id(&self, ty: Ty, id: i32) -> i32 {
        match ty {
            Ty::Known => self.dict.known.left_id(id),
            Ty::Unknown => self.dict.unknown.left_id(id),
            Ty::User => dict::USER_LEFT_ID,
        }
    }
    fn right_id(&self, ty: Ty, id: i32) -> i32 {
        match ty {
            Ty::Known => self.dict.known.left_id(id),
            Ty::Unknown => self.dict.unknown.left_id(id),
            Ty::User => dict::USER_RIGHT_ID,
        }
    }
    fn word_cost(&self, ty: Ty, id: i32) -> i32 {
        match ty {
            Ty::Known => self.dict.known.word_cost(id),
            Ty::Unknown => self.dict.unknown.word_cost(id),
            Ty::User => dict::USER_WORD_COST,
        }
    }
    fn penalty(&self, core: &Core, pos: i32, len: i32) -> i32 {
        if len > SEARCH_MODE_KANJI_LENGTH {
            let all_kanji = (pos..pos + len).all(|p| self.dict.is_kanji(core.text[p as usize]));
            if all_kanji {
                return (len - SEARCH_MODE_KANJI_LENGTH) * SEARCH_MODE_KANJI_PENALTY;
            } else if len > SEARCH_MODE_OTHER_LENGTH {
                return (len - SEARCH_MODE_OTHER_LENGTH) * SEARCH_MODE_OTHER_PENALTY;
            }
        }
        0
    }
    fn skip_unknown(&self, unknown_end: i32, pos_data_pos: i32) -> bool {
        !self.search && unknown_end > pos_data_pos
    }
    fn process_unknown(&mut self, core: &mut Core, any_matches: bool, pos_data_pos: i32) -> i32 {
        let cd = &self.dict.chardef;
        let first = core.text[core.pos as usize];
        if any_matches && !cd.is_invoke(first) {
            return 0;
        }
        let class = cd.class(first);
        let punct = is_punctuation(first);
        let mut len = 1usize;
        if cd.is_group(first) {
            let mut ahead = core.pos as usize + 1;
            while len < MAX_UNKNOWN_WORD_LENGTH {
                let Some(&c) = core.text.get(ahead) else { break };
                if class == cd.class(c) && is_punctuation(c) == punct {
                    len += 1;
                } else {
                    break;
                }
                ahead += 1;
            }
        }
        let pos = core.pos;
        for &id in self.dict.unk_targets.word_ids(class as usize) {
            viterbi::add(core, self, pos_data_pos, pos, pos_data_pos + len as i32, id as i32, Ty::Unknown, false);
        }
        len as i32
    }
    fn backtrace(&mut self, core: &mut Core, end_pos: i32, from_idx: usize) {
        self.backtrace_impl(core, end_pos, from_idx);
    }
    fn output_nbest(&self) -> bool {
        self.nbest_cost > 0
    }
    fn backtrace_nbest(&mut self, core: &mut Core, end_pos: i32, use_eos: bool) {
        self.backtrace_nbest_impl(core, end_pos, use_eos);
    }
    fn fixup_pending(&mut self) {
        self.fixup_pending_impl();
    }
    fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

impl JaLang {
    fn backtrace_impl(&mut self, core: &mut Core, end_pos: i32, from_idx: usize) {
        let last = core.last_back_trace_pos;
        let mut pos = end_pos;
        let mut best_idx = from_idx as i32;
        let mut alt: Option<JaTok> = None;
        let mut last_left_word_id = -1i32;
        let mut back_count = 0i32;

        while pos > last {
            let a = core.positions.peek(pos).arcs[best_idx as usize];
            let mut back_pos = a.back_pos;
            let mut length = pos - back_pos;
            let mut back_type = a.back_type;
            let mut back_id = a.back_id;
            let mut next_best_idx = a.back_index;

            if self.search && alt.is_none() && back_type != Ty::User {
                // A too-long token on the best path: if a second-best segmentation costs less
                // than the penalty more, follow it (and also output the long token).
                let penalty = self.penalty(core, back_pos, pos - back_pos);
                if penalty > 0 {
                    let costs = &self.dict.sys.costs;
                    let mut max_cost = a.cost.wrapping_add(penalty);
                    if last_left_word_id != -1 {
                        max_cost = max_cost.wrapping_add(costs.get(self.right_id(back_type, back_id), last_left_word_id));
                    }
                    self.prune_and_rescore(core, back_pos, pos, a.back_index);
                    let costs = &self.dict.sys.costs;
                    let p = core.positions.peek(pos);
                    let mut least_cost = i32::MAX;
                    let mut least_idx = -1i32;
                    for (idx, arc) in p.arcs.iter().enumerate() {
                        let mut cost = arc.cost;
                        if last_left_word_id != -1 {
                            cost = cost.wrapping_add(costs.get(self.right_id(arc.back_type, arc.back_id), last_left_word_id));
                        }
                        if cost < least_cost {
                            least_cost = cost;
                            least_idx = idx as i32;
                        }
                    }
                    if least_idx != -1 && least_cost <= max_cost && p.arcs[least_idx as usize].back_pos != back_pos {
                        alt = Some(JaTok {
                            start: back_pos,
                            len: length,
                            word: back_id,
                            ty: back_type,
                            pos_len: 1,
                        });
                        best_idx = least_idx;
                        let b = p.arcs[best_idx as usize];
                        next_best_idx = b.back_index;
                        back_pos = b.back_pos;
                        length = pos - back_pos;
                        back_type = b.back_type;
                        back_id = b.back_id;
                        back_count = 0;
                    }
                }
            }

            if let Some(mut t) = alt
                && t.start >= back_pos
            {
                if self.compounds && back_count > 0 {
                    back_count += 1;
                    t.pos_len = back_count;
                    self.pending.push(t);
                }
                alt = None;
            }

            if back_type == Ty::User {
                // Expand the phrase into its segmentation.
                let user = self.user.clone().expect("user dictionary");
                let seg = user.segmentation(back_id);
                let word_id = seg[0];
                let mut current = 0;
                let first = self.pending.len();
                for (j, &len) in seg.iter().enumerate().skip(1) {
                    self.pending.push(JaTok {
                        start: current + back_pos,
                        len,
                        word: word_id + j as i32 - 1,
                        ty: Ty::User,
                        pos_len: 1,
                    });
                    current += len;
                }
                self.pending[first..].reverse();
                back_count += seg.len() as i32 - 1;
            } else if self.extended && back_type == Ty::Unknown {
                // Extended mode turns unknown words into unigrams.
                let mut unigrams = 0;
                let mut i = length - 1;
                while i >= 0 {
                    let mut char_len = 1;
                    if i > 0 && is_low_surrogate(core.text[(back_pos + i) as usize]) {
                        i -= 1;
                        char_len = 2;
                    }
                    if !self.discard_punctuation || !is_punctuation(core.text[(back_pos + i) as usize]) {
                        self.pending.push(JaTok {
                            start: back_pos + i,
                            len: char_len,
                            word: NGRAM,
                            ty: Ty::Unknown,
                            pos_len: 1,
                        });
                        unigrams += 1;
                    }
                    i -= 1;
                }
                back_count += unigrams;
            } else if !self.discard_punctuation || length == 0 || !is_punctuation(core.text[back_pos as usize]) {
                self.pending.push(JaTok {
                    start: back_pos,
                    len: length,
                    word: back_id,
                    ty: back_type,
                    pos_len: 1,
                });
                back_count += 1;
            }

            last_left_word_id = self.left_id(back_type, back_id);
            pos = back_pos;
            best_idx = next_best_idx;
        }

        core.last_back_trace_pos = end_pos;
        core.positions.free_before(end_pos);
    }

    /// Removes compound (penalized) arcs and arcs crossing `start_pos` between the two
    /// positions, then re-scores what is left.
    fn prune_and_rescore(&mut self, core: &mut Core, start_pos: i32, end_pos: i32, best_start_idx: i32) {
        let mut pos = end_pos;
        while pos > start_pos {
            let arcs = std::mem::take(&mut core.positions.get(pos).arcs);
            for a in &arcs {
                if a.back_pos >= start_pos {
                    core.positions.get(a.back_pos).fwd.push(Fwd {
                        pos,
                        id: a.back_id,
                        ty: a.back_type,
                    });
                }
            }
            let mut arcs = arcs;
            arcs.clear();
            core.positions.get(pos).arcs = arcs;
            pos -= 1;
        }
        for pos in start_pos..end_pos {
            let p = core.positions.get(pos);
            if p.arcs.is_empty() {
                p.fwd.clear();
                continue;
            }
            let fwd = std::mem::take(&mut p.fwd);
            if pos == start_pos {
                // Only the best path's arc at the start: the sub-segmentation stays in the
                // context the compound matched.
                let b = p.arcs[best_start_idx as usize];
                let right_id = if start_pos == 0 { 0 } else { self.right_id(b.back_type, b.back_id) };
                let path_cost = b.cost;
                for f in &fwd {
                    let new_cost = path_cost
                        .wrapping_add(self.word_cost(f.ty, f.id))
                        .wrapping_add(self.dict.sys.costs.get(right_id, self.left_id(f.ty, f.id)))
                        .wrapping_add(self.penalty(core, pos, f.pos - pos));
                    let right = self.right_id(f.ty, f.id);
                    core.positions.get(f.pos).add(In {
                        cost: new_cost,
                        last_right: right,
                        back_pos: pos,
                        back_word_pos: -1,
                        back_index: best_start_idx,
                        back_id: f.id,
                        back_type: f.ty,
                    });
                }
            } else {
                for f in &fwd {
                    viterbi::add(core, self, pos, pos, f.pos, f.id, f.ty, true);
                }
            }
            let mut fwd = fwd;
            fwd.clear();
            core.positions.get(pos).fwd = fwd;
        }
    }

    fn backtrace_nbest_impl(&mut self, core: &mut Core, end_pos: i32, use_eos: bool) {
        let mut lattice = std::mem::take(&mut self.lattice);
        lattice.setup(core, self, core.last_back_trace_pos, end_pos, use_eos);
        lattice.mark_unreachable();
        lattice.calc_left_cost(self);
        lattice.calc_right_cost(self);
        let best_cost = lattice.best_cost();
        for node in lattice.best_path_node_list() {
            self.register_node(core, &lattice, node);
        }
        let mut n = 2;
        loop {
            let nbest = lattice.n_best_node_list(n);
            if nbest.is_empty() {
                break;
            }
            let cost = lattice.cost(nbest[0]);
            if best_cost.wrapping_add(self.nbest_cost) < cost {
                break;
            }
            for node in nbest {
                self.register_node(core, &lattice, node);
            }
            n += 1;
        }
        self.lattice = lattice;
    }

    fn register_node(&mut self, core: &Core, l: &Lattice, node: usize) {
        let left = l.left[node];
        let right = l.right[node];
        let ty = l.dic_type[node];
        let base = l.root_base;
        if self.discard_punctuation && is_punctuation(core.text[(base + left) as usize]) {
            return;
        }
        if ty == Ty::User {
            let user = self.user.clone().expect("user dictionary");
            let seg = user.segmentation(l.word_id[node]);
            let word_id = seg[0];
            self.pending.push(JaTok {
                start: base + left,
                len: right - left,
                word: word_id,
                ty: Ty::User,
                pos_len: 1,
            });
            let mut current = 0;
            for (j, &len) in seg.iter().enumerate().skip(1) {
                if len < right - left {
                    self.pending.push(JaTok {
                        start: base + current + left,
                        len,
                        word: word_id + j as i32 - 1,
                        ty: Ty::User,
                        pos_len: 1,
                    });
                }
                current += len;
            }
        } else {
            self.pending.push(JaTok {
                start: base + left,
                len: right - left,
                word: l.word_id[node],
                ty,
                pos_len: 1,
            });
        }
    }

    /// Sorts, removes duplicate spans and computes position lengths of n-best output.
    fn fixup_pending_impl(&mut self) {
        let p = &mut self.pending;
        // Java's `Collections.sort` is stable.
        p.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then(a.len.cmp(&b.len))
                .then((b.ty as i32).cmp(&(a.ty as i32)))
        });
        p.dedup_by(|b, a| a.start == b.start && a.len == b.len);
        let mut edges: Vec<i32> = p.iter().flat_map(|t| [t.start, t.start + t.len]).collect();
        edges.sort_unstable();
        edges.dedup();
        let index = |off: i32| edges.binary_search(&off).unwrap() as i32;
        for t in p.iter_mut() {
            t.pos_len = index(t.start + t.len) - index(t.start);
        }
        p.reverse();
    }
}

/// `ViterbiNBest.Lattice`: the lattice of one back trace, for n-best paths.
#[derive(Default)]
struct Lattice {
    root_size: usize,
    root_base: i32,
    l_root: Vec<i32>,
    r_root: Vec<i32>,
    use_eos: bool,
    dic_type: Vec<Ty>,
    word_id: Vec<i32>,
    mark: Vec<i32>,
    left_id: Vec<i32>,
    right_id: Vec<i32>,
    word_cost: Vec<i32>,
    left_cost: Vec<i32>,
    right_cost: Vec<i32>,
    left_node: Vec<i32>,
    right_node: Vec<i32>,
    left: Vec<i32>,
    right: Vec<i32>,
    left_chain: Vec<i32>,
    right_chain: Vec<i32>,
}

impl Lattice {
    fn add_node(&mut self, lang: &JaLang, ty: Ty, word_id: i32, left: i32, right: i32) -> usize {
        let node = self.dic_type.len();
        self.dic_type.push(ty);
        self.word_id.push(word_id);
        self.mark.push(0);
        if word_id < 0 {
            self.word_cost.push(0);
            self.left_id.push(0);
            self.right_id.push(0);
        } else {
            self.word_cost.push(lang.word_cost(ty, word_id));
            self.left_id.push(lang.left_id(ty, word_id));
            self.right_id.push(lang.right_id(ty, word_id));
        }
        self.left_cost.push(0);
        self.right_cost.push(0);
        self.left_node.push(0);
        self.right_node.push(0);
        self.left.push(left);
        self.right.push(right);
        if left >= 0 {
            self.left_chain.push(self.l_root[left as usize]);
            self.l_root[left as usize] = node as i32;
        } else {
            self.left_chain.push(-1);
        }
        if right >= 0 {
            self.right_chain.push(self.r_root[right as usize]);
            self.r_root[right as usize] = node as i32;
        } else {
            self.right_chain.push(-1);
        }
        node
    }

    fn setup(&mut self, core: &mut Core, lang: &JaLang, prev: i32, end: i32, use_eos: bool) {
        self.use_eos = use_eos;
        let size = (end - prev + 1) as usize;
        self.l_root.clear();
        self.l_root.resize(size, -1);
        self.r_root.clear();
        self.r_root.resize(size, -1);
        self.root_size = size;
        self.root_base = prev;
        for v in [
            &mut self.word_id,
            &mut self.mark,
            &mut self.left_id,
            &mut self.right_id,
            &mut self.word_cost,
            &mut self.left_cost,
            &mut self.right_cost,
            &mut self.left_node,
            &mut self.right_node,
            &mut self.left,
            &mut self.right,
            &mut self.left_chain,
            &mut self.right_chain,
        ] {
            v.clear();
        }
        self.dic_type.clear();
        let first = core.positions.get(prev).arcs[0];
        // BOS = 0
        self.add_node(lang, first.back_type, first.back_id, -1, 0);
        // EOS = 1
        self.add_node(lang, Ty::Known, -1, end - self.root_base, -1);
        let mut offset = end;
        while prev < offset {
            let right = offset - self.root_base;
            if self.l_root[right as usize] >= 0 {
                let arcs = core.positions.get(offset).arcs.clone();
                for a in arcs {
                    self.add_node(lang, a.back_type, a.back_id, a.back_pos - self.root_base, right);
                }
            }
            offset -= 1;
        }
    }

    fn mark_unreachable(&mut self) {
        for index in 1..self.root_size.saturating_sub(1) {
            if self.r_root[index] < 0 {
                let mut node = self.l_root[index];
                while node >= 0 {
                    self.mark[node as usize] = -1;
                    node = self.left_chain[node as usize];
                }
            }
        }
    }

    fn connection_cost(&self, lang: &JaLang, left: usize, right: usize) -> i32 {
        let left_id = self.left_id[right];
        if left_id == 0 && !self.use_eos {
            0
        } else {
            lang.dict.sys.costs.get(self.right_id[left], left_id)
        }
    }

    fn calc_left_cost(&mut self, lang: &JaLang) {
        for index in 0..self.root_size {
            let mut node = self.l_root[index];
            while node >= 0 {
                let n = node as usize;
                if self.mark[n] >= 0 {
                    let mut least_node = -1;
                    let mut least_cost = i32::MAX;
                    let mut ln = self.r_root[index];
                    while ln >= 0 {
                        let l = ln as usize;
                        if self.mark[l] >= 0 {
                            let cost = self.left_cost[l]
                                .wrapping_add(self.word_cost[l])
                                .wrapping_add(self.connection_cost(lang, l, n));
                            if cost < least_cost {
                                least_cost = cost;
                                least_node = ln;
                            }
                        }
                        ln = self.right_chain[l];
                    }
                    self.left_node[n] = least_node;
                    self.left_cost[n] = least_cost;
                }
                node = self.left_chain[n];
            }
        }
    }

    fn calc_right_cost(&mut self, lang: &JaLang) {
        for index in (0..self.root_size).rev() {
            let mut node = self.r_root[index];
            while node >= 0 {
                let n = node as usize;
                if self.mark[n] >= 0 {
                    let mut least_node = -1;
                    let mut least_cost = i32::MAX;
                    let mut rn = self.l_root[index];
                    while rn >= 0 {
                        let r = rn as usize;
                        if self.mark[r] >= 0 {
                            let cost = self.right_cost[r]
                                .wrapping_add(self.word_cost[r])
                                .wrapping_add(self.connection_cost(lang, n, r));
                            if cost < least_cost {
                                least_cost = cost;
                                least_node = rn;
                            }
                        }
                        rn = self.left_chain[r];
                    }
                    self.right_node[n] = least_node;
                    self.right_cost[n] = least_cost;
                }
                node = self.right_chain[n];
            }
        }
    }

    fn mark_same_span_node(&mut self, ref_node: usize, value: i32) {
        let left = self.left[ref_node];
        let right = self.right[ref_node];
        let mut node = self.l_root[left as usize];
        while node >= 0 {
            if self.right[node as usize] == right {
                self.mark[node as usize] = value;
            }
            node = self.left_chain[node as usize];
        }
    }

    fn best_path_node_list(&mut self) -> Vec<usize> {
        let mut list = Vec::new();
        let mut node = self.right_node[0];
        while node != 1 && node >= 0 {
            list.push(node as usize);
            self.mark_same_span_node(node as usize, 1);
            node = self.right_node[node as usize];
        }
        list
    }

    fn cost(&self, node: usize) -> i32 {
        self.left_cost[node]
            .wrapping_add(self.word_cost[node])
            .wrapping_add(self.right_cost[node])
    }

    fn n_best_node_list(&mut self, n: i32) -> Vec<usize> {
        let mut list = Vec::new();
        let mut least_cost = i32::MAX;
        let (mut least_left, mut least_right) = (-1, -1);
        for node in 2..self.dic_type.len() {
            if self.mark[node] == 0 {
                let cost = self.cost(node);
                if cost < least_cost {
                    least_cost = cost;
                    least_left = self.left[node];
                    least_right = self.right[node];
                    list.clear();
                    list.push(node);
                } else if cost == least_cost && (self.left[node] != least_left || self.right[node] != least_right) {
                    list.push(node);
                }
            }
        }
        for &node in &list {
            self.mark_same_span_node(node, n);
        }
        list
    }

    fn best_cost(&self) -> i32 {
        self.left_cost[1]
    }

    fn probe_delta(&self, start: i32, end: i32) -> i32 {
        let left = start - self.root_base;
        let right = end - self.root_base;
        if left < 0 || self.root_size < right as usize {
            return i32::MAX;
        }
        let mut probed = i32::MAX;
        if (left as usize) < self.l_root.len() {
            let mut node = self.l_root[left as usize];
            while node >= 0 {
                if self.right[node as usize] == right {
                    probed = probed.min(self.cost(node as usize));
                }
                node = self.left_chain[node as usize];
            }
        }
        probed.wrapping_sub(self.best_cost())
    }
}

/// The `kuromoji_tokenizer`.
pub(crate) struct JapaneseTokenizer {
    lang: JaLang,
    core: Core,
    shared: Option<crate::morph::Text>,
    last_token_pos: i32,
    last: Option<(JaTok, u32, u32)>,
}

impl JapaneseTokenizer {
    pub fn new(o: &Options) -> JapaneseTokenizer {
        let (search, extended, compounds) = match o.mode {
            Mode::Search => (true, false, !o.discard_compound_token),
            Mode::Extended => (true, true, !o.discard_compound_token),
            Mode::Normal => (false, false, false),
        };
        JapaneseTokenizer {
            lang: JaLang {
                dict: dict::dict(),
                user: o.user.clone(),
                discard_punctuation: o.discard_punctuation,
                search,
                extended,
                compounds,
                nbest_cost: o.nbest_cost,
                pending: Vec::new(),
                lattice: Lattice::default(),
            },
            core: Core::default(),
            shared: None,
            last_token_pos: -1,
            last: None,
        }
    }

    fn fill(&mut self, tok: &JaTok, pos_inc: u32, pos_len: u32, t: &mut Tok) {
        let units = &self.core.text[tok.start as usize..(tok.start + tok.len) as usize];
        crate::morph::push_utf16(&mut t.term, units);
        t.start = tok.start as u32;
        t.end = (tok.start + tok.len) as u32;
        t.pos_inc = pos_inc;
        t.pos_len = pos_len;
        let text = self
            .shared
            .get_or_insert_with(|| Arc::from(self.core.text.as_slice()))
            .clone();
        t.morph = Morph::Ja(JaMorph {
            text,
            start: tok.start as u32,
            len: tok.len as u32,
            word: tok.word,
            ty: tok.ty,
            user: if tok.ty == Ty::User { self.lang.user.clone() } else { None },
        });
    }

    fn start(&mut self, text: &str) {
        crate::morph::to_utf16(text, &mut self.core.text);
        self.core.reset();
        self.lang.pending.clear();
        self.shared = None;
        self.last_token_pos = -1;
        self.last = None;
    }

    fn next_token(&mut self) -> Option<JaTok> {
        while self.lang.pending.is_empty() {
            if self.core.end {
                return None;
            }
            viterbi::forward(&mut self.core, &mut self.lang);
        }
        self.lang.pending.pop()
    }

    /// `JapaneseTokenizer.calcNBestCost`: the largest extra cost that makes each
    /// `text-token` example's token appear (examples separated by `/`).
    pub fn calc_nbest_cost(&mut self, examples: &str) -> std::result::Result<i32, String> {
        let mut max_delta = 0;
        for example in examples.split('/') {
            if example.is_empty() {
                continue;
            }
            let pair: Vec<&str> = example.split('-').collect();
            if pair.len() != 2 {
                return Err(format!("Unexpected example form: {example} (expected two '-')"));
            }
            max_delta = max_delta.max(self.probe_delta(pair[0], pair[1]));
        }
        Ok(max_delta)
    }

    fn probe_delta(&mut self, text: &str, required: &str) -> i32 {
        let Some(byte_start) = text.find(required) else { return -1 };
        let start = text[..byte_start].encode_utf16().count() as i32;
        let end = start + required.encode_utf16().count() as i32;
        let saved = self.lang.nbest_cost;
        self.lang.nbest_cost = 1;
        self.start(text);
        let mut delta = i32::MAX;
        let mut prev_root_base = -1;
        while self.next_token().is_some() {
            if self.lang.lattice.root_base != prev_root_base {
                prev_root_base = self.lang.lattice.root_base;
                delta = delta.min(self.lang.lattice.probe_delta(start, end));
            }
        }
        self.lang.nbest_cost = saved;
        if delta == i32::MAX { -1 } else { delta }
    }
}

impl Tokenizer for JapaneseTokenizer {
    fn reset(&mut self, text: &str) {
        self.start(text);
    }

    fn next(&mut self, _text: &str, t: &mut Tok) -> bool {
        let Some(tok) = self.next_token() else {
            // Lucene's tokenizer leaves the last token's attributes in place when the input
            // ends, and filters (`kuromoji_number`) read them.
            if let Some((last, inc, len)) = self.last {
                self.fill(&last, inc, len, t);
            }
            return false;
        };
        let (pos_inc, pos_len) = if tok.start == self.last_token_pos {
            (0, tok.pos_len as u32)
        } else {
            (1, if self.lang.nbest_cost > 0 { tok.pos_len as u32 } else { 1 })
        };
        self.last_token_pos = tok.start;
        self.fill(&tok, pos_inc, pos_len, t);
        self.last = Some((tok, pos_inc, pos_len));
        true
    }

    fn end(&mut self, _text: &str, t: &mut Tok) {
        t.start = self.core.pos as u32;
        t.end = self.core.pos as u32;
    }
}
