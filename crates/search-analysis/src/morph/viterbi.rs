//! Lucene's `org.apache.lucene.analysis.morph.Viterbi`: the forward pass over the word lattice
//! (dictionary, user-dictionary and unknown-word arcs at every position, each arc keeping its
//! least-cost predecessor), with back traces at frontiers where only one path survives.
//! [`Lang`] is what `JapaneseTokenizer`'s and `KoreanTokenizer`'s subclasses add.

use std::collections::VecDeque;

use super::{Fst, System, char_type};

/// For safety (`MAX_UNKNOWN_WORD_LENGTH`).
pub(crate) const MAX_UNKNOWN_WORD_LENGTH: usize = 1024;
const MAX_BACKTRACE_GAP: i32 = 1024;

/// `TokenType`: which dictionary an arc's word id belongs to (declaration order matters to
/// `fixupPendingList`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Ty {
    Known,
    Unknown,
    User,
}

/// One arc arriving at a position.
#[derive(Clone, Copy, Debug)]
pub(crate) struct In {
    pub cost: i32,
    pub last_right: i32,
    pub back_pos: i32,
    pub back_word_pos: i32,
    pub back_index: i32,
    pub back_id: i32,
    pub back_type: Ty,
}

/// A forward arc (used only while re-scoring under a compound token).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fwd {
    pub pos: i32,
    pub id: i32,
    pub ty: Ty,
}

/// `Position`: every arc arriving at one character position.
#[derive(Default, Debug)]
pub(crate) struct Position {
    pub pos: i32,
    pub arcs: Vec<In>,
    pub fwd: Vec<Fwd>,
}

impl Position {
    pub fn add(&mut self, a: In) {
        self.arcs.push(a);
    }
}

/// `WrappedPositionArray`: positions from the last back trace onwards.
#[derive(Default)]
pub(crate) struct Positions {
    ring: VecDeque<Position>,
    first: i32,
    spare: Vec<Position>,
}

impl Positions {
    pub fn reset(&mut self) {
        while let Some(mut p) = self.ring.pop_front() {
            p.arcs.clear();
            p.fwd.clear();
            self.spare.push(p);
        }
        self.first = 0;
    }

    pub fn next_pos(&self) -> i32 {
        self.first + self.ring.len() as i32
    }

    /// The position (created on demand, however far in the future).
    pub fn get(&mut self, pos: i32) -> &mut Position {
        while pos >= self.next_pos() {
            let mut p = self.spare.pop().unwrap_or_default();
            p.pos = self.next_pos();
            self.ring.push_back(p);
        }
        debug_assert!(pos >= self.first, "position {pos} was freed");
        &mut self.ring[(pos - self.first) as usize]
    }

    pub fn peek(&self, pos: i32) -> &Position {
        &self.ring[(pos - self.first) as usize]
    }

    pub fn free_before(&mut self, pos: i32) {
        while self.first < pos {
            let Some(mut p) = self.ring.pop_front() else {
                self.first = pos;
                break;
            };
            p.arcs.clear();
            p.fwd.clear();
            self.spare.push(p);
            self.first += 1;
        }
    }
}

/// The search state shared by both languages.
#[derive(Default)]
pub(crate) struct Core {
    /// The whole input in UTF-16 units.
    pub text: Vec<u16>,
    pub positions: Positions,
    pub end: bool,
    pub last_back_trace_pos: i32,
    pub pos: i32,
}

impl Core {
    /// `buffer.get(pos)`: the unit at `pos`, or -1 past the end.
    #[inline]
    pub fn ch(&self, pos: i32) -> i32 {
        self.text.get(pos as usize).map_or(-1, |&u| u as i32)
    }

    /// `resetState`.
    pub fn reset(&mut self) {
        self.positions.reset();
        self.pos = 0;
        self.end = false;
        self.last_back_trace_pos = 0;
        // BOS
        self.positions.get(0).add(In {
            cost: 0,
            last_right: 0,
            back_pos: -1,
            back_word_pos: -1,
            back_index: -1,
            back_id: -1,
            back_type: Ty::Known,
        });
    }
}

/// What a language adds to the search.
pub(crate) trait Lang {
    fn system(&self) -> &System;
    fn user_fst(&self) -> Option<&Fst>;
    fn left_id(&self, ty: Ty, id: i32) -> i32;
    fn right_id(&self, ty: Ty, id: i32) -> i32;
    fn word_cost(&self, ty: Ty, id: i32) -> i32;
    /// `enableSpacePenaltyFactor`
    fn space_prefix(&self) -> bool {
        false
    }
    /// `outputLongestUserEntryOnly`
    fn longest_user_only(&self) -> bool {
        false
    }
    fn space_penalty(&self, _ty: Ty, _id: i32, _spaces: i32) -> i32 {
        0
    }
    fn penalty(&self, _core: &Core, _pos: i32, _len: i32) -> i32 {
        0
    }
    fn skip_unknown(&self, unknown_end: i32, pos_data_pos: i32) -> bool {
        unknown_end > pos_data_pos
    }
    fn process_unknown(&mut self, core: &mut Core, any_matches: bool, pos_data_pos: i32) -> i32;
    fn backtrace(&mut self, core: &mut Core, end_pos: i32, from_idx: usize);
    fn output_nbest(&self) -> bool {
        false
    }
    fn backtrace_nbest(&mut self, _core: &mut Core, _end_pos: i32, _use_eos: bool) {}
    fn fixup_pending(&mut self) {}
    fn pending_len(&self) -> usize;
}

/// `Viterbi.add`: the arc for a word from `from_pos`'s arcs to `end_pos`, keeping the cheapest
/// predecessor.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add<L: Lang + ?Sized>(
    core: &mut Core,
    lang: &L,
    from_pos: i32,
    word_pos: i32,
    end_pos: i32,
    word_id: i32,
    ty: Ty,
    add_penalty: bool,
) {
    let word_cost = lang.word_cost(ty, word_id);
    let left_id = lang.left_id(ty, word_id);
    let costs = &lang.system().costs;
    let from = core.positions.get(from_pos);
    debug_assert!(!from.arcs.is_empty());
    let num_spaces = word_pos - from.pos;
    let space_penalty = lang.space_penalty(ty, word_id, num_spaces);
    let mut least_cost = i32::MAX;
    let mut least_idx = -1i32;
    for (idx, a) in from.arcs.iter().enumerate() {
        let cost = a
            .cost
            .wrapping_add(costs.get(a.last_right, left_id))
            .wrapping_add(space_penalty);
        if cost < least_cost {
            least_cost = cost;
            least_idx = idx as i32;
        }
    }
    let from_pos_value = from.pos;
    least_cost = least_cost.wrapping_add(word_cost);
    if add_penalty && ty != Ty::User {
        least_cost = least_cost.wrapping_add(lang.penalty(core, from_pos_value, end_pos - from_pos_value));
    }
    let right = lang.right_id(ty, word_id);
    core.positions.get(end_pos).add(In {
        cost: least_cost,
        last_right: right,
        back_pos: from_pos_value,
        back_word_pos: word_pos,
        back_index: least_idx,
        back_id: word_id,
        back_type: ty,
    });
}

/// `Viterbi.forward`: parses until some tokens are pending (or the input ends).
pub(crate) fn forward<L: Lang>(core: &mut Core, lang: &mut L) {
    let mut unknown_word_end: i32 = -1;
    let mut user_word_max_pos_ahead: i32 = -1;

    while core.ch(core.pos) != -1 {
        let pos_data_pos = core.pos;
        let next_pos = core.positions.next_pos();
        let count = core.positions.get(pos_data_pos).arcs.len();
        let is_frontier = next_pos == core.pos + 1;

        if count == 0 {
            core.pos += 1;
            continue;
        }

        if core.pos > core.last_back_trace_pos && count == 1 && is_frontier {
            // Only one node is alive at a frontier: the best path must come through it.
            if lang.output_nbest() {
                lang.backtrace_nbest(core, pos_data_pos, false);
            }
            lang.backtrace(core, pos_data_pos, 0);
            if lang.output_nbest() {
                lang.fixup_pending();
            }
            core.positions.get(pos_data_pos).arcs[0].cost = 0;
            if lang.pending_len() > 0 {
                return;
            }
        }

        if core.pos - core.last_back_trace_pos >= MAX_BACKTRACE_GAP {
            // Safety: back trace from the least-cost partial path and prune all others.
            let mut least_idx = -1i32;
            let mut least_cost = i32::MAX;
            let mut least_pos = -1i32;
            for pos2 in core.pos..core.positions.next_pos() {
                let p2 = core.positions.get(pos2);
                for (idx, a) in p2.arcs.iter().enumerate() {
                    if a.cost < least_cost {
                        least_cost = a.cost;
                        least_idx = idx as i32;
                        least_pos = pos2;
                    }
                }
            }
            debug_assert!(least_idx != -1);
            if lang.output_nbest() {
                lang.backtrace_nbest(core, least_pos, false);
            }
            for pos2 in core.pos..core.positions.next_pos() {
                let p2 = core.positions.get(pos2);
                if pos2 != least_pos {
                    p2.arcs.clear();
                } else {
                    p2.arcs.swap(0, least_idx as usize);
                    p2.arcs.truncate(1);
                }
            }
            lang.backtrace(core, least_pos, 0);
            if lang.output_nbest() {
                lang.fixup_pending();
            }
            for a in core.positions.get(least_pos).arcs.iter_mut() {
                a.cost = 0;
            }
            if core.pos != least_pos {
                core.pos = least_pos;
            }
            if lang.pending_len() > 0 {
                return;
            }
            continue;
        }

        if lang.space_prefix() && char_type(core.text[core.pos as usize]) == crate::unicode::gc::SPACE_SEPARATOR {
            // A single space separator becomes a prefix of the next terms (for the space
            // penalty).
            core.pos += 1;
            if core.ch(core.pos) == -1 {
                core.pos = pos_data_pos;
            }
        }

        let mut any_matches = false;
        let pos = core.pos;

        // First the user dictionary.
        if let Some(user) = lang.user_fst() {
            let mut matches: smallvec::SmallVec<[(i32, i32); 4]> = smallvec::SmallVec::new();
            user.prefixes(&core.text, pos as usize, |end, out| matches.push((end as i32, out as i32)));
            if let Some(&(last_end, last_out)) = matches.last() {
                any_matches = true;
                if !lang.longest_user_only() {
                    for &(end, out) in &matches {
                        add(core, lang, pos_data_pos, pos, end, out, Ty::User, false);
                    }
                }
                let max_pos_ahead = last_end - 1;
                if max_pos_ahead > user_word_max_pos_ahead {
                    if lang.longest_user_only() {
                        add(core, lang, pos_data_pos, pos, last_end, last_out, Ty::User, false);
                    }
                    user_word_max_pos_ahead = user_word_max_pos_ahead.max(max_pos_ahead);
                }
            }
        }

        if !any_matches {
            // Then the system dictionary.
            let mut matches: smallvec::SmallVec<[(i32, u32); 8]> = smallvec::SmallVec::new();
            lang.system()
                .fst
                .prefixes(&core.text, pos as usize, |end, out| matches.push((end as i32, out as u32)));
            for (end, out) in matches {
                let n = lang.system().targets.word_ids(out as usize).len();
                for k in 0..n {
                    let id = lang.system().targets.word_ids(out as usize)[k];
                    add(core, lang, pos_data_pos, pos, end, id as i32, Ty::Known, false);
                    any_matches = true;
                }
            }
        }

        if !lang.skip_unknown(unknown_word_end, pos_data_pos) {
            let len = lang.process_unknown(core, any_matches, pos_data_pos);
            unknown_word_end = pos_data_pos + len;
        }
        core.pos += 1;
    }

    core.end = true;

    if core.pos > 0 {
        let end_pos = core.pos;
        let costs = &lang.system().costs;
        let end = core.positions.get(end_pos);
        let mut least_cost = i32::MAX;
        let mut least_idx = -1i32;
        for (idx, a) in end.arcs.iter().enumerate() {
            // EOS cost
            let cost = a.cost.wrapping_add(costs.get(a.last_right, 0));
            if cost < least_cost {
                least_cost = cost;
                least_idx = idx as i32;
            }
        }
        if lang.output_nbest() {
            lang.backtrace_nbest(core, end_pos, true);
        }
        lang.backtrace(core, end_pos, least_idx.max(0) as usize);
        if lang.output_nbest() {
            lang.fixup_pending();
        }
    }
}
