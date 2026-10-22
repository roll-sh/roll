//! `flatten_graph` (Lucene's `FlattenGraphFilter`) and the rolling buffer it is built on.
//!
//! The filter turns a token graph (stacked alternatives with position lengths, as produced by
//! `synonym_graph` and `word_delimiter_graph`) into a "sausage": every token keeps its position
//! but spans to a node all alternatives share, so the stream can be indexed. It is a port of
//! Lucene's algorithm with input and output node buffers that roll forward as tokens are
//! released.

use std::collections::VecDeque;

use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

/// Lucene's `RollingBuffer`: positions from a moving base, created on demand, recycled on free.
pub(crate) struct RollingBuffer<T> {
    items: VecDeque<T>,
    base: usize,
    spare: Vec<T>,
    reset: fn(&mut T),
    make: fn() -> T,
}

impl<T> RollingBuffer<T> {
    pub fn new(make: fn() -> T, reset: fn(&mut T)) -> Self {
        RollingBuffer {
            items: VecDeque::new(),
            base: 0,
            spare: Vec::new(),
            reset,
            make,
        }
    }

    pub fn clear(&mut self) {
        while let Some(mut x) = self.items.pop_front() {
            (self.reset)(&mut x);
            self.spare.push(x);
        }
        self.base = 0;
    }

    /// The next position not yet created.
    fn next_pos(&self) -> usize {
        self.base + self.items.len()
    }

    /// `getMaxPos()` (may be -1).
    pub fn max_pos(&self) -> i64 {
        self.next_pos() as i64 - 1
    }

    pub fn get(&mut self, pos: usize) -> &mut T {
        while pos >= self.next_pos() {
            let x = self.spare.pop().unwrap_or_else(self.make);
            self.items.push_back(x);
        }
        debug_assert!(
            pos >= self.base,
            "position {pos} was freed (base {})",
            self.base
        );
        let i = pos.saturating_sub(self.base);
        &mut self.items[i]
    }

    pub fn free_before(&mut self, pos: usize) {
        while self.base < pos && !self.items.is_empty() {
            if let Some(mut x) = self.items.pop_front() {
                (self.reset)(&mut x);
                self.spare.push(x);
            }
            self.base += 1;
        }
    }
}

struct InputNode {
    tokens: Vec<Tok>,
    ntokens: usize,
    node: i64,
    max_to: i64,
    min_to: i64,
    output_node: i64,
    next_out: usize,
}

impl InputNode {
    fn new() -> Self {
        InputNode {
            tokens: Vec::new(),
            ntokens: 0,
            node: -1,
            max_to: -1,
            min_to: i64::MAX,
            output_node: -1,
            next_out: 0,
        }
    }
    fn reset(n: &mut InputNode) {
        n.ntokens = 0;
        n.node = -1;
        n.output_node = -1;
        n.max_to = -1;
        n.min_to = i64::MAX;
        n.next_out = 0;
    }
}

struct OutputNode {
    input_nodes: Vec<i64>,
    node: i64,
    next_out: usize,
    start: i64,
    end: i64,
}

impl OutputNode {
    fn new() -> Self {
        OutputNode {
            input_nodes: Vec::new(),
            node: -1,
            next_out: 0,
            start: -1,
            end: -1,
        }
    }
    fn reset(n: &mut OutputNode) {
        n.input_nodes.clear();
        n.node = -1;
        n.next_out = 0;
        n.start = -1;
        n.end = -1;
    }
}

/// `FlattenGraphFilter`.
pub(crate) struct FlattenGraph {
    inputs: RollingBuffer<InputNode>,
    outputs: RollingBuffer<OutputNode>,
    input_from: i64,
    output_from: i64,
    done: bool,
    last_output_from: i64,
    final_offset: u32,
    final_pos_inc: u32,
    last_start_offset: i64,
}

impl FlattenGraph {
    pub fn new() -> Self {
        let mut f = FlattenGraph {
            inputs: RollingBuffer::new(InputNode::new, InputNode::reset),
            outputs: RollingBuffer::new(OutputNode::new, OutputNode::reset),
            input_from: -1,
            output_from: 0,
            done: false,
            last_output_from: -1,
            final_offset: 0,
            final_pos_inc: 0,
            last_start_offset: 0,
        };
        f.reset_state();
        f
    }

    fn reset_state(&mut self) {
        self.input_from = -1;
        self.inputs.clear();
        {
            let n = self.inputs.get(0);
            n.node = 0;
            n.output_node = 0;
        }
        self.outputs.clear();
        {
            let o = self.outputs.get(0);
            o.node = 0;
            o.input_nodes.push(0);
            o.start = 0;
        }
        self.output_from = 0;
        self.last_output_from = -1;
        self.done = false;
        self.last_start_offset = 0;
    }

    fn free_before(&mut self, output_from: usize) {
        self.output_from += 1;
        let min = self
            .outputs
            .get(output_from)
            .input_nodes
            .iter()
            .copied()
            .min()
            .unwrap_or(0);
        self.inputs.free_before(min.max(0) as usize);
        self.outputs.free_before(self.output_from as usize);
    }

    fn release(&mut self, t: &mut Tok) -> bool {
        while self.output_from < self.outputs.max_pos() {
            let of = self.output_from as usize;
            if self.outputs.get(of).input_nodes.is_empty() {
                self.output_from += 1;
                continue;
            }
            let ids: smallvec::SmallVec<[i64; 8]> =
                self.outputs.get(of).input_nodes.iter().copied().collect();
            let mut max_to = -1;
            for id in &ids {
                max_to = max_to.max(self.inputs.get(*id as usize).max_to);
            }
            if max_to <= self.input_from || self.done {
                let next_out = self.outputs.get(of).next_out;
                let in_id = ids[next_out] as usize;
                let (empty, in_next_out) = {
                    let n = self.inputs.get(in_id);
                    (n.ntokens == 0, n.next_out)
                };
                if self.done && empty && self.output_from >= self.outputs.max_pos() {
                    return false;
                }
                if empty {
                    if ids.len() > 1 {
                        let o = self.outputs.get(of);
                        o.next_out += 1;
                        if o.next_out < ids.len() {
                            continue;
                        }
                    }
                    self.free_before(of);
                    continue;
                }
                let node = {
                    let n = self.inputs.get(in_id);
                    t.clone_from(&n.tokens[in_next_out]);
                    n.node
                };
                t.pos_inc = (self.output_from - self.last_output_from).max(0) as u32;
                let to_id = (node + t.pos_len as i64) as usize;
                let to_output = self.inputs.get(to_id).output_node;
                t.pos_len = (to_output - self.output_from).max(1) as u32;
                self.last_output_from = self.output_from;
                let n = self.inputs.get(in_id);
                n.next_out += 1;
                let finished_input = n.next_out == n.ntokens;
                let out_start = self.outputs.get(of).start;
                let end_node_end = self.outputs.get(to_output.max(0) as usize).end;
                let start = self.last_start_offset.max(out_start);
                let end = start.max(end_node_end);
                t.start = start.max(0) as u32;
                t.end = end.max(0) as u32;
                self.last_start_offset = start;
                if finished_input {
                    let o = self.outputs.get(of);
                    o.next_out += 1;
                    if o.next_out == o.input_nodes.len() {
                        self.free_before(of);
                    }
                }
                return true;
            }
            return false;
        }
        false
    }

    fn recover_from_hole(&mut self, src_id: usize, start: u32, pos_inc: u32) -> usize {
        let input_from = self.input_from;
        self.inputs.get(src_id).node = input_from;
        let prev = input_from - pos_inc as i64;
        let out_index = if prev >= 0 {
            let min_to = self.inputs.get(prev as usize).min_to;
            if min_to < input_from {
                (self.inputs.get(min_to as usize).output_node + 1) as usize
            } else {
                self.outputs.max_pos().max(0) as usize
            }
        } else {
            (self.outputs.max_pos() + 1) as usize
        };
        self.inputs.get(src_id).output_node = out_index as i64;
        let o = self.outputs.get(out_index);
        if o.node == -1 {
            o.node = out_index as i64;
            o.start = start as i64;
        } else {
            o.start = o.start.max(start as i64);
        }
        o.input_nodes.push(input_from);
        out_index
    }
}

impl Filter for FlattenGraph {
    fn reset(&mut self) {
        self.reset_state();
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        loop {
            if self.release(t) {
                return true;
            } else if self.done {
                return false;
            }
            if input.next(t) {
                let pos_inc = t.pos_inc;
                self.input_from += pos_inc as i64;
                let (start, end) = (t.start, t.end);
                let input_to = self.input_from + t.pos_len as i64;
                let src_id = self.input_from as usize;
                let out_src_id;
                if self.inputs.get(src_id).node == -1 {
                    out_src_id = self.recover_from_hole(src_id, start, pos_inc);
                } else {
                    let mut osid = self.inputs.get(src_id).output_node as usize;
                    if pos_inc > 1 {
                        let prev_id = (self.input_from - pos_inc as i64) as usize;
                        let (prev_out, prev_min_to) = {
                            let p = self.inputs.get(prev_id);
                            (p.output_node, p.min_to)
                        };
                        let src_out = self.inputs.get(src_id).output_node;
                        if src_out - prev_out <= 1 && prev_min_to != self.input_from {
                            let input_from = self.input_from;
                            let o = self.outputs.get(osid);
                            if let Some(i) = o.input_nodes.iter().position(|&x| x == input_from) {
                                o.input_nodes.remove(i);
                            }
                            let prev_end = o.end;
                            self.inputs.get(src_id).output_node = -1;
                            osid = self.recover_from_hole(src_id, start, pos_inc);
                            self.outputs.get(osid).end = prev_end;
                        }
                    }
                    out_src_id = osid;
                }
                {
                    let o = self.outputs.get(out_src_id);
                    if o.start == -1 || start as i64 > o.start {
                        o.start = o.start.max(start as i64);
                    }
                }
                {
                    let src = self.inputs.get(src_id);
                    if src.ntokens == src.tokens.len() {
                        src.tokens.push(Tok::default());
                    }
                    src.tokens[src.ntokens].clone_from(t);
                    src.ntokens += 1;
                    src.max_to = src.max_to.max(input_to);
                    src.min_to = src.min_to.min(input_to);
                }
                let src_out = self.inputs.get(src_id).output_node;
                let dest = self.inputs.get(input_to as usize);
                if dest.node == -1 {
                    dest.node = input_to;
                }
                let dest_out = dest.output_node;
                let output_end = src_out + 1;
                if output_end > dest_out {
                    if dest_out != -1 {
                        let o = self.outputs.get(dest_out as usize);
                        if let Some(i) = o.input_nodes.iter().position(|&x| x == input_to) {
                            o.input_nodes.remove(i);
                        }
                    }
                    self.outputs
                        .get(output_end as usize)
                        .input_nodes
                        .push(input_to);
                    self.inputs.get(input_to as usize).output_node = output_end;
                }
                let dest_out = self.inputs.get(input_to as usize).output_node as usize;
                let o = self.outputs.get(dest_out);
                if o.end == -1 || (end as i64) < o.end {
                    o.end = end as i64;
                }
            } else {
                t.clear_for_end();
                input.end(t);
                self.final_pos_inc = t.pos_inc;
                self.final_offset = t.end;
                self.done = true;
            }
        }
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        if self.done {
            t.clear_for_end();
            t.pos_inc = self.final_pos_inc;
            t.start = self.final_offset;
            t.end = self.final_offset;
        } else {
            input.end(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream of prepared tokens: (term, pos_inc, pos_len, start, end).
    struct Fixed(Vec<(&'static str, u32, u32, u32, u32)>, usize);

    impl TokenStream for Fixed {
        fn next(&mut self, t: &mut Tok) -> bool {
            let Some(&(term, inc, len, s, e)) = self.0.get(self.1) else {
                return false;
            };
            self.1 += 1;
            t.clear();
            t.term.push_str(term);
            t.pos_inc = inc;
            t.pos_len = len;
            t.start = s;
            t.end = e;
            true
        }
        fn end(&mut self, t: &mut Tok) {
            t.start = 13;
            t.end = 13;
        }
    }

    #[test]
    fn flattens_a_synonym_graph() {
        // "wtf happened" with "wtf" -> "what the fudge" (a side path of length 3).
        let mut input = Fixed(
            vec![
                ("what", 1, 1, 0, 3),
                ("wtf", 0, 3, 0, 3),
                ("the", 1, 1, 0, 3),
                ("fudge", 1, 1, 0, 3),
                ("happened", 1, 1, 4, 12),
            ],
            0,
        );
        let mut f = FlattenGraph::new();
        f.reset();
        let mut t = Tok::default();
        let mut out = Vec::new();
        while f.next(&mut input, &mut t) {
            out.push((t.term.clone(), t.pos_inc, t.pos_len));
        }
        assert_eq!(
            out,
            vec![
                ("what".to_string(), 1, 1),
                ("wtf".to_string(), 0, 3),
                ("the".to_string(), 1, 1),
                ("fudge".to_string(), 1, 1),
                ("happened".to_string(), 1, 1)
            ]
        );
    }
}
