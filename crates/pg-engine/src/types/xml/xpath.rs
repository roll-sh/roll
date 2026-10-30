//! XPath 1.0 as libxml2 2.13 implements it (xpath.c and timsort.h), over the engine's
//! document tree, and the Postgres glue around it (xml.c: `xpath_internal`,
//! `xml_xpathobjtoxmlarray`, `xml_xmlnodetoxmltype`).
//!
//! The evaluator keeps libxml's shape — a value stack, first-error-wins error codes, the
//! "first"/"last" shortcuts, the namespace-node copies — because Postgres users see its quirks:
//! which nodes come back, in what order, and which error detail.

use super::chvalid;
use super::parser::{Options, Parser, XML_NS};
use super::save::Saver;
use super::tree::{AttrType, DOC, Decl, Doc, Kind, NodeId, Ns};
use super::{Strictness, escape_xml, parse_xml_decl, report};
use crate::error::{Error, Result, code};
use crate::types::Value;
use std::cell::{Cell, OnceCell};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// Errors (xmlXPathError)

const NUMBER_ERROR: i32 = 1;
const UNFINISHED_LITERAL_ERROR: i32 = 2;
const START_LITERAL_ERROR: i32 = 3;
const VARIABLE_REF_ERROR: i32 = 4;
const UNDEF_VARIABLE_ERROR: i32 = 5;
const INVALID_PREDICATE_ERROR: i32 = 6;
const EXPR_ERROR: i32 = 7;
const UNCLOSED_ERROR: i32 = 8;
const UNKNOWN_FUNC_ERROR: i32 = 9;
const INVALID_OPERAND: i32 = 10;
const INVALID_TYPE: i32 = 11;
const INVALID_ARITY: i32 = 12;
const INVALID_CTXT_SIZE: i32 = 13;
const INVALID_CTXT_POSITION: i32 = 14;
const MEMORY_ERROR: i32 = 15;
const UNDEF_PREFIX_ERROR: i32 = 19;
const ENCODING_ERROR: i32 = 20;
const INVALID_CHAR_ERROR: i32 = 21;
const STACK_ERROR: i32 = 23;
const FORBID_VARIABLE_ERROR: i32 = 24;
const RECURSION_LIMIT_EXCEEDED: i32 = 26;

/// `xmlXPathErrorMessages`, without their trailing newlines (Postgres chops them).
const MESSAGES: [&str; 28] = [
    "Ok",
    "Number encoding",
    "Unfinished literal",
    "Start of literal",
    "Expected $ for variable reference",
    "Undefined variable",
    "Invalid predicate",
    "Invalid expression",
    "Missing closing curly brace",
    "Unregistered function",
    "Invalid operand",
    "Invalid type",
    "Invalid number of arguments",
    "Invalid context size",
    "Invalid context position",
    "Memory allocation error",
    "Syntax error",
    "Resource error",
    "Sub resource error",
    "Undefined namespace prefix",
    "Encoding error",
    "Char out of XML range",
    "Invalid or incomplete context",
    "Stack usage error",
    "Forbidden variable",
    "Operation limit exceeded",
    "Recursion limit exceeded",
    "?? Unknown error ??",
];

fn message(code: i32) -> &'static str {
    MESSAGES[code.clamp(0, MESSAGES.len() as i32 - 1) as usize]
}

/// `XPATH_MAX_RECURSION_DEPTH`.
const MAX_RECURSION_DEPTH: i32 = 5000;

/// `IS_BLANK_CH`.
fn is_blank(c: u8) -> bool {
    matches!(c, 0x20 | 0x09 | 0x0a | 0x0d)
}

/// `IS_CHAR`.
fn is_xml_char(c: u32) -> bool {
    matches!(c, 0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)
}

fn is_letter(c: u32) -> bool {
    chvalid::is_base_char(c) || chvalid::is_ideographic(c)
}

// ---------------------------------------------------------------------------------------------
// Nodes

/// A namespace node: a copy of an `xmlNs` whose `next` points at the element it belongs to
/// (`xmlXPathNodeSetDupNs`). Every copy is a distinct node, as in libxml.
#[derive(Debug)]
pub struct NsNode {
    pub parent: Option<NodeId>,
    pub prefix: Option<String>,
    pub href: Option<String>,
}

/// What an XPath node set holds: a tree node, or a namespace node.
#[derive(Clone, Debug)]
pub enum XNode {
    Node(NodeId),
    Ns(Arc<NsNode>),
}

impl XNode {
    /// Pointer identity.
    fn same(&self, o: &XNode) -> bool {
        match (self, o) {
            (XNode::Node(a), XNode::Node(b)) => a == b,
            (XNode::Ns(a), XNode::Ns(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    fn id(&self) -> Option<NodeId> {
        match self {
            XNode::Node(n) => Some(*n),
            XNode::Ns(_) => None,
        }
    }
}

/// A parsed document with libxml's links XPath walks, including the entity-reference →
/// entity-declaration → replacement-text linkage.
pub struct XDoc {
    pub doc: Doc,
    /// Entity index → its declaration node in the DTD.
    decl_of: Vec<Option<NodeId>>,
    /// The parent of an entity's top-level replacement nodes: its declaration (`ent`).
    ent_parent: HashMap<NodeId, NodeId>,
    ids: OnceCell<HashMap<String, NodeId>>,
    /// Document order of the source tree's nodes (1-based; 0: not ordered), as libxslt's
    /// `xmlXPathOrderDocElems` makes comparisons constant-time. Empty unless ordered.
    order: Vec<u32>,
}

impl XDoc {
    /// The document node, the context XPath starts from.
    pub fn root(&self) -> XNode {
        XNode::Node(DOC)
    }

    pub fn new(doc: Doc) -> XDoc {
        let mut decl_of = vec![None; doc.entities.len()];
        let mut ent_parent = HashMap::new();
        if !doc.entities.is_empty() {
            for (id, node) in doc.nodes.iter().enumerate() {
                if let (Kind::EntityDecl, Decl::Entity(e)) = (node.kind, node.decl())
                    && *e < decl_of.len()
                {
                    decl_of[*e] = Some(id);
                }
            }
            for (e, ent) in doc.entities.iter().enumerate() {
                if let Some(d) = decl_of[e] {
                    for &c in &ent.children {
                        ent_parent.insert(c, d);
                    }
                }
            }
        }
        XDoc {
            doc,
            decl_of,
            ent_parent,
            ids: OnceCell::new(),
            order: Vec::new(),
        }
    }

    fn kind(&self, id: NodeId) -> Kind {
        self.doc.nodes[id].kind
    }

    /// The parent XPath sees: an attribute's element, an entity's declaration.
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        match self.doc.parent(id) {
            None if !self.ent_parent.is_empty() => self.ent_parent.get(&id).copied(),
            p => p,
        }
    }

    /// The document node `id` belongs to.
    pub fn doc_of(&self, mut id: NodeId) -> NodeId {
        while let Some(p) = self.parent(id) {
            id = p;
        }
        id
    }

    /// Numbers the document's nodes in document order (an element, its attributes, then its
    /// children), so comparing two of them is a lookup. The order is the one the
    /// comparison computes; nodes grafted later stay unnumbered.
    pub fn order_nodes(&mut self) {
        let mut order = vec![0u32; self.doc.nodes.len()];
        let mut k = 0u32;
        let mut stack = vec![DOC];
        while let Some(id) = stack.pop() {
            k += 1;
            order[id] = k;
            let n = &self.doc.nodes[id];
            if n.kind == Kind::EntityRef || n.kind == Kind::Dtd {
                continue;
            }
            for a in self.doc.attrs(id) {
                k += 1;
                order[a] = k;
            }
            stack.extend(self.doc.children(id).rev());
        }
        self.order = order;
    }

    /// Sorts nodes into document order (`xmlXPathNodeSetSort`).
    pub fn sort_nodes(&self, set: &mut [XNode]) {
        if set.len() > 1 {
            self.sort(set);
        }
    }

    /// Adds another document's tree (a result tree fragment, say) to the arena, so one XPath
    /// evaluation reaches nodes of both; returns its document node, which has no parent.
    /// Entities aren't carried over: the trees grafted have no entity references.
    pub fn graft(&mut self, other: Doc) -> NodeId {
        self.doc.absorb(other)
    }

    fn next(&self, id: NodeId) -> Option<NodeId> {
        self.doc.next_sibling(id)
    }

    fn prev(&self, id: NodeId) -> Option<NodeId> {
        self.doc.prev_sibling(id)
    }

    /// The entity declaration an entity reference points at (`ref->children`).
    fn ref_target(&self, id: NodeId) -> Option<NodeId> {
        let e = self.doc.entity(&self.doc.nodes[id].name)?;
        self.decl_of[e]
    }

    fn entity_children(&self, id: NodeId) -> &[NodeId] {
        match self.doc.nodes[id].decl() {
            Decl::Entity(e) => &self.doc.entities[*e].children,
            _ => &[],
        }
    }

    fn first_child(&self, id: NodeId) -> Option<NodeId> {
        match self.kind(id) {
            Kind::EntityRef => self.ref_target(id),
            Kind::EntityDecl => self.entity_children(id).first().copied(),
            _ => self.doc.first_child(id),
        }
    }

    fn last_child(&self, id: NodeId) -> Option<NodeId> {
        match self.kind(id) {
            Kind::EntityRef => self.ref_target(id),
            Kind::EntityDecl => self.entity_children(id).last().copied(),
            _ => self.doc.last_child(id),
        }
    }

    // --- string values

    /// `xmlXPathCastNodeToString` (`xmlNodeGetContent`).
    pub fn node_string(&self, x: &XNode) -> String {
        match x {
            XNode::Ns(ns) => ns.href.clone().unwrap_or_default(),
            XNode::Node(id) => self.node_content(*id),
        }
    }

    fn node_content(&self, id: NodeId) -> String {
        let n = &self.doc.nodes[id];
        match n.kind {
            Kind::Element | Kind::Attribute | Kind::EntityDecl => {
                let Some(c) = self.first_child(id) else {
                    return String::new();
                };
                if matches!(self.kind(c), Kind::Text | Kind::CData) && self.next(c).is_none() {
                    return self.doc.text_of(c).to_string();
                }
                let mut out = String::new();
                self.child_content(id, &mut out, &mut Vec::new());
                out
            }
            Kind::Text | Kind::CData | Kind::Comment | Kind::Pi => self.doc.text(n).to_string(),
            Kind::Document => {
                let mut out = String::new();
                self.child_content(id, &mut out, &mut Vec::new());
                out
            }
            Kind::EntityRef => {
                let mut out = String::new();
                self.entity_ref_content(id, &mut out, &mut Vec::new());
                out
            }
            Kind::Namespace => {
                n.ns.as_ref()
                    .and_then(|ns| ns.href.clone())
                    .unwrap_or_default()
            }
            _ => String::new(),
        }
    }

    /// `xmlBufGetEntityRefContent`.
    fn entity_ref_content(&self, id: NodeId, out: &mut String, expanding: &mut Vec<NodeId>) {
        let decl = match self.ref_target(id) {
            Some(d) => d,
            None => {
                if let Some(p) = Doc::predefined(&self.doc.nodes[id].name) {
                    out.push_str(p);
                }
                return;
            }
        };
        if expanding.contains(&decl) {
            return;
        }
        expanding.push(decl);
        self.child_content(decl, out, expanding);
        expanding.pop();
    }

    /// `xmlBufGetChildContent`.
    fn child_content(&self, tree: NodeId, out: &mut String, expanding: &mut Vec<NodeId>) {
        let mut cur = self.first_child(tree);
        while let Some(c) = cur {
            match self.kind(c) {
                Kind::Text | Kind::CData => out.push_str(self.doc.text_of(c)),
                Kind::EntityRef => self.entity_ref_content(c, out, expanding),
                _ => {
                    if let Some(f) = self.first_child(c) {
                        cur = Some(f);
                        continue;
                    }
                }
            }
            let mut c = c;
            loop {
                if let Some(n) = self.next(c) {
                    cur = Some(n);
                    break;
                }
                match self.parent(c) {
                    Some(p) if p != tree => c = p,
                    _ => return,
                }
            }
        }
    }

    /// `xmlXPathNodeValHash`.
    fn node_hash(&self, x: &XNode) -> u32 {
        let id = match x {
            XNode::Ns(ns) => return string_hash(ns.href.as_deref().unwrap_or("")),
            XNode::Node(id) => *id,
        };
        let mut node = id;
        if self.kind(node) == Kind::Document {
            let root = self.doc.children(node).find(|&c| self.kind(c) == Kind::Element);
            match root.or_else(|| self.first_child(node)) {
                Some(n) => node = n,
                None => return 0,
            }
        }
        let mut tmp = match self.kind(node) {
            Kind::Comment | Kind::Pi | Kind::CData | Kind::Text => {
                return string_hash(self.doc.text_of(node));
            }
            Kind::Attribute | Kind::Element => self.first_child(node),
            _ => return 0,
        };
        let mut len = 2;
        let mut ret: u32 = 0;
        while let Some(t) = tmp {
            let s = match self.kind(t) {
                Kind::CData | Kind::Text => Some(self.doc.text_of(t).as_bytes()),
                _ => None,
            };
            if let Some(s) = s
                && !s.is_empty()
            {
                if len == 1 {
                    return ret + ((s[0] as u32) << 8);
                }
                if s.len() == 1 {
                    len = 1;
                    ret = s[0] as u32;
                } else {
                    return s[0] as u32 + ((s[1] as u32) << 8);
                }
            }
            if let Some(f) = self.first_child(t)
                && !matches!(self.kind(t), Kind::Dtd | Kind::EntityRef)
                && self.kind(f) != Kind::EntityDecl
            {
                tmp = Some(f);
                continue;
            }
            if t == node {
                break;
            }
            if let Some(n) = self.next(t) {
                tmp = Some(n);
                continue;
            }
            let mut t = t;
            tmp = None;
            while let Some(p) = self.parent(t) {
                if p == node {
                    break;
                }
                if let Some(n) = self.next(p) {
                    tmp = Some(n);
                    break;
                }
                t = p;
            }
        }
        ret
    }

    // --- document order

    /// `xmlXPathCmpNodesExt`: 1 if `n1` comes first, -1 if after, 0 if the same, -2 if
    /// unrelated.
    fn cmp_nodes(&self, n1: &XNode, n2: &XNode) -> i32 {
        if n1.same(n2) {
            return 0;
        }
        if let (XNode::Node(a), XNode::Node(b)) = (n1, n2)
            && let (Some(&x), Some(&y)) = (self.order.get(*a), self.order.get(*b))
            && x != 0
            && y != 0
        {
            return if x < y { 1 } else { -1 };
        }
        let XNode::Node(mut node1) = *n1 else {
            return 1;
        };
        let XNode::Node(mut node2) = *n2 else {
            return 1;
        };
        let (mut misc, mut prec1, mut prec2) = (false, 0, 0);
        let (mut misc1, mut misc2) = (node1, node2);
        if self.kind(node1) == Kind::Attribute {
            prec1 = 1;
            misc1 = node1;
            match self.parent(node1) {
                Some(p) => node1 = p,
                None => return -2,
            }
            misc = true;
        }
        if self.kind(node2) == Kind::Attribute {
            prec2 = 1;
            misc2 = node2;
            match self.parent(node2) {
                Some(p) => node2 = p,
                None => return -2,
            }
            misc = true;
        }
        if misc && node1 == node2 {
            if prec1 == prec2 {
                let mut cur = self.prev(misc2);
                while let Some(c) = cur {
                    if c == misc1 {
                        return 1;
                    }
                    if self.kind(c) == Kind::Element {
                        return -1;
                    }
                    cur = self.prev(c);
                }
                return -1;
            }
            return if prec1 < prec2 { 1 } else { -1 };
        }
        if Some(node1) == self.prev(node2) {
            return 1;
        }
        if Some(node1) == self.next(node2) {
            return -1;
        }
        let mut depth2 = 0;
        let mut cur = node2;
        while let Some(p) = self.parent(cur) {
            if p == node1 {
                return 1;
            }
            depth2 += 1;
            cur = p;
        }
        let root = cur;
        let mut depth1 = 0;
        cur = node1;
        while let Some(p) = self.parent(cur) {
            if p == node2 {
                return -1;
            }
            depth1 += 1;
            cur = p;
        }
        if root != cur {
            return -2;
        }
        while depth1 > depth2 {
            depth1 -= 1;
            node1 = self.parent(node1).unwrap();
        }
        while depth2 > depth1 {
            depth2 -= 1;
            node2 = self.parent(node2).unwrap();
        }
        while self.parent(node1) != self.parent(node2) {
            match (self.parent(node1), self.parent(node2)) {
                (Some(a), Some(b)) => {
                    node1 = a;
                    node2 = b;
                }
                _ => return -2,
            }
        }
        if Some(node1) == self.prev(node2) {
            return 1;
        }
        if Some(node1) == self.next(node2) {
            return -1;
        }
        let mut cur = self.next(node1);
        while let Some(c) = cur {
            if c == node2 {
                return 1;
            }
            cur = self.next(c);
        }
        -1
    }

    /// timsort.h's `SORT_CMP`.
    fn sort_cmp(&self, x: &XNode, y: &XNode) -> i32 {
        let r = self.cmp_nodes(x, y);
        if r == -2 { r } else { -r }
    }

    /// `xmlXPathNodeSetSort` (libxml's bundled timsort).
    fn sort(&self, set: &mut [XNode]) {
        tim_sort(set, &|x, y| self.sort_cmp(x, y));
    }

    // --- ids

    /// The document's ID table (`xmlAddID` as SAX2 calls it while parsing without validation).
    fn ids(&self) -> &HashMap<String, NodeId> {
        self.ids.get_or_init(|| {
            let mut ids = HashMap::new();
            let has_dtd = self.doc.int_subset.is_some() || self.doc.has_ext_subset;
            let mut stack = vec![DOC];
            while let Some(n) = stack.pop() {
                let node = &self.doc.nodes[n];
                if node.kind == Kind::Element {
                    for a in self.doc.attrs(n) {
                        let an = &self.doc.nodes[a];
                        let (Some(t), None) = (self.doc.first_child(a), self.doc.first_child(a).and_then(|t| self.doc.next_sibling(t))) else {
                            continue;
                        };
                        if self.doc.nodes[t].kind != Kind::Text {
                            continue;
                        }
                        let value = self.doc.text_of(t);
                        if value.is_empty() {
                            continue;
                        }
                        let aprefix = an.ns.as_ref().and_then(|ns| ns.prefix.clone());
                        let is_id = if aprefix.as_deref() == Some("xml") && an.name == "id" {
                            true
                        } else if has_dtd {
                            let key = (self.doc.qname(n), an.name.to_string(), aprefix);
                            self.doc.attr_decls.get(&key).is_some_and(|&d| {
                                matches!(
                                    self.doc.nodes[d].decl(),
                                    Decl::Attribute {
                                        atype: AttrType::Id,
                                        ..
                                    }
                                )
                            })
                        } else {
                            false
                        };
                        if is_id && !ids.contains_key(value) {
                            ids.insert(value.to_string(), a);
                        }
                    }
                }
                if matches!(node.kind, Kind::Element | Kind::Document) {
                    for c in self.doc.children(n).rev() {
                        if self.doc.nodes[c].kind == Kind::Element {
                            stack.push(c);
                        }
                    }
                }
            }
            ids
        })
    }
}

/// `xmlXPathStringHash`.
fn string_hash(s: &str) -> u32 {
    let b = s.as_bytes();
    match b {
        [] => 0,
        [a] => *a as u32,
        [a, b, ..] => *a as u32 + ((*b as u32) << 8),
    }
}

// ---------------------------------------------------------------------------------------------
// timsort.h

fn compute_minrun(size: usize) -> usize {
    let top_bit = 64 - (size as u64).leading_zeros() as usize;
    let shift = top_bit.max(6) - 6;
    let minrun = size >> shift;
    let mask = (1usize << shift) - 1;
    if mask & size != 0 { minrun + 1 } else { minrun }
}

fn binary_insertion_find(
    dst: &[XNode],
    x: &XNode,
    size: usize,
    cmp: &dyn Fn(&XNode, &XNode) -> i32,
) -> usize {
    let mut l = 0;
    let mut r = size - 1;
    let mut c = r >> 1;
    if cmp(x, &dst[0]) < 0 {
        return 0;
    } else if cmp(x, &dst[r]) > 0 {
        return r;
    }
    loop {
        let val = cmp(x, &dst[c]);
        if val < 0 {
            if c - l <= 1 {
                return c;
            }
            r = c;
        } else {
            if r - c <= 1 {
                return c + 1;
            }
            l = c;
        }
        c = l + ((r - l) >> 1);
    }
}

fn binary_insertion_sort_start(
    dst: &mut [XNode],
    start: usize,
    size: usize,
    cmp: &dyn Fn(&XNode, &XNode) -> i32,
) {
    for i in start..size {
        if cmp(&dst[i - 1], &dst[i]) <= 0 {
            continue;
        }
        let x = dst[i].clone();
        let location = binary_insertion_find(dst, &x, i, cmp);
        dst[location..=i].rotate_right(1);
    }
}

fn count_run(
    dst: &mut [XNode],
    start: usize,
    size: usize,
    cmp: &dyn Fn(&XNode, &XNode) -> i32,
) -> usize {
    if size - start == 1 {
        return 1;
    }
    if start >= size - 2 {
        if cmp(&dst[size - 2], &dst[size - 1]) > 0 {
            dst.swap(size - 2, size - 1);
        }
        return 2;
    }
    let mut curr = start + 2;
    if cmp(&dst[start], &dst[start + 1]) <= 0 {
        loop {
            if curr == size - 1 || cmp(&dst[curr - 1], &dst[curr]) > 0 {
                break;
            }
            curr += 1;
        }
        curr - start
    } else {
        loop {
            if curr == size - 1 || cmp(&dst[curr - 1], &dst[curr]) <= 0 {
                break;
            }
            curr += 1;
        }
        dst[start..curr].reverse();
        curr - start
    }
}

#[derive(Clone, Copy, Default)]
struct Run {
    start: usize,
    length: usize,
}

fn check_invariant(stack: &[Run], stack_curr: usize) -> bool {
    if stack_curr < 2 {
        return true;
    }
    if stack_curr == 2 {
        return stack[0].length > stack[1].length;
    }
    let a = stack[stack_curr - 3].length;
    let b = stack[stack_curr - 2].length;
    let c = stack[stack_curr - 1].length;
    !(a <= b + c || b <= c)
}

fn tim_sort_merge(
    dst: &mut [XNode],
    stack: &[Run],
    stack_curr: usize,
    cmp: &dyn Fn(&XNode, &XNode) -> i32,
) {
    let a = stack[stack_curr - 2].length;
    let b = stack[stack_curr - 1].length;
    let curr = stack[stack_curr - 2].start;
    if a < b {
        let storage: Vec<XNode> = dst[curr..curr + a].to_vec();
        let (mut i, mut j) = (0, curr + a);
        for k in curr..curr + a + b {
            if i < a && j < curr + a + b {
                if cmp(&storage[i], &dst[j]) <= 0 {
                    dst[k] = storage[i].clone();
                    i += 1;
                } else {
                    dst[k] = dst[j].clone();
                    j += 1;
                }
            } else if i < a {
                dst[k] = storage[i].clone();
                i += 1;
            } else {
                break;
            }
        }
    } else {
        let storage: Vec<XNode> = dst[curr + a..curr + a + b].to_vec();
        let (mut i, mut j, mut k) = (b, curr + a, curr + a + b);
        while k > curr {
            k -= 1;
            if i > 0 && j > curr {
                if cmp(&dst[j - 1], &storage[i - 1]) > 0 {
                    j -= 1;
                    dst[k] = dst[j].clone();
                } else {
                    i -= 1;
                    dst[k] = storage[i].clone();
                }
            } else if i > 0 {
                i -= 1;
                dst[k] = storage[i].clone();
            } else {
                break;
            }
        }
    }
}

fn tim_sort_collapse(
    dst: &mut [XNode],
    stack: &mut [Run],
    mut stack_curr: usize,
    size: usize,
    cmp: &dyn Fn(&XNode, &XNode) -> i32,
) -> usize {
    loop {
        if stack_curr <= 1 {
            break;
        }
        if stack_curr == 2
            && (stack[0].length + stack[1].length == size || stack[0].length <= stack[1].length)
        {
            tim_sort_merge(dst, stack, stack_curr, cmp);
            stack[0].length += stack[1].length;
            stack_curr -= 1;
            break;
        } else if stack_curr == 2 {
            break;
        }
        let b = stack[stack_curr - 3].length;
        let c = stack[stack_curr - 2].length;
        let d = stack[stack_curr - 1].length;
        let abc = if stack_curr >= 4 {
            stack[stack_curr - 4].length <= b + c
        } else {
            false
        };
        let bcd = b <= c + d || abc;
        let cd = c <= d;
        if !bcd && !cd {
            break;
        }
        if bcd && !cd {
            tim_sort_merge(dst, stack, stack_curr - 1, cmp);
            stack[stack_curr - 3].length += stack[stack_curr - 2].length;
            stack[stack_curr - 2] = stack[stack_curr - 1];
            stack_curr -= 1;
        } else {
            tim_sort_merge(dst, stack, stack_curr, cmp);
            stack[stack_curr - 2].length += stack[stack_curr - 1].length;
            stack_curr -= 1;
        }
    }
    stack_curr
}

fn push_next(
    dst: &mut [XNode],
    size: usize,
    minrun: usize,
    stack: &mut Vec<Run>,
    curr: &mut usize,
    cmp: &dyn Fn(&XNode, &XNode) -> i32,
) -> bool {
    let mut len = count_run(dst, *curr, size, cmp);
    let run = minrun.min(size - *curr);
    if run > len {
        binary_insertion_sort_start(&mut dst[*curr..], len, run, cmp);
        len = run;
    }
    stack.push(Run {
        start: *curr,
        length: len,
    });
    *curr += len;
    if *curr == size {
        while stack.len() > 1 {
            let n = stack.len();
            tim_sort_merge(dst, stack, n, cmp);
            stack[n - 2].length += stack[n - 1].length;
            stack.pop();
        }
        return false;
    }
    true
}

fn tim_sort(dst: &mut [XNode], cmp: &dyn Fn(&XNode, &XNode) -> i32) {
    let size = dst.len();
    if size <= 1 {
        return;
    }
    if size < 64 {
        binary_insertion_sort_start(dst, 1, size, cmp);
        return;
    }
    let minrun = compute_minrun(size);
    let mut stack: Vec<Run> = Vec::new();
    let mut curr = 0;
    for _ in 0..3 {
        if !push_next(dst, size, minrun, &mut stack, &mut curr, cmp) {
            return;
        }
    }
    loop {
        if !check_invariant(&stack, stack.len()) {
            let n = stack.len();
            let kept = tim_sort_collapse(dst, &mut stack, n, size, cmp);
            stack.truncate(kept);
            continue;
        }
        if !push_next(dst, size, minrun, &mut stack, &mut curr, cmp) {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Compiled expressions (xmlXPathCompExpr)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    And,
    Or,
    Equal,
    Cmp,
    Plus,
    Mult,
    Union,
    Root,
    Node,
    Collect,
    Value,
    Variable,
    Function,
    Arg,
    Predicate,
    Filter,
    Sort,
}

const AXIS_ANCESTOR: i32 = 1;
const AXIS_ANCESTOR_OR_SELF: i32 = 2;
const AXIS_ATTRIBUTE: i32 = 3;
const AXIS_CHILD: i32 = 4;
const AXIS_DESCENDANT: i32 = 5;
const AXIS_DESCENDANT_OR_SELF: i32 = 6;
const AXIS_FOLLOWING: i32 = 7;
const AXIS_FOLLOWING_SIBLING: i32 = 8;
const AXIS_NAMESPACE: i32 = 9;
const AXIS_PARENT: i32 = 10;
const AXIS_PRECEDING: i32 = 11;
const AXIS_PRECEDING_SIBLING: i32 = 12;
const AXIS_SELF: i32 = 13;

const TEST_NONE: i32 = 0;
const TEST_TYPE: i32 = 1;
const TEST_PI: i32 = 2;
const TEST_ALL: i32 = 3;
const TEST_NAME: i32 = 5;

const TYPE_NODE: i32 = 0;
const TYPE_TEXT: i32 = 3;
const TYPE_PI: i32 = 7;
const TYPE_COMMENT: i32 = 8;

/// A literal operand (`value4` of an `XPATH_OP_VALUE`).
#[derive(Clone, Debug)]
enum Lit {
    Number(f64),
    Str(String),
}

/// `xmlXPathStepOp`. `ch1`, `ch2` and `value` are cells: evaluation swaps union operands and
/// the optimizer rewrites axes, as libxml does in place.
#[derive(Debug)]
struct Step {
    op: Op,
    ch1: Cell<i32>,
    ch2: Cell<i32>,
    value: Cell<i32>,
    value2: i32,
    value3: i32,
    lit: Option<Lit>,
    /// `value4`: the prefix of a COLLECT, the name of a FUNCTION/VARIABLE.
    s4: Option<String>,
    /// `value5`: the name of a COLLECT, the prefix of a FUNCTION/VARIABLE.
    s5: Option<String>,
}

/// A compiled XPath expression: libxml's operation table and its root. (2.13 compiles
/// `XPATH_STREAMING` out, so no expression is evaluated as a streaming pattern.)
#[derive(Debug)]
pub struct Compiled {
    steps: Vec<Step>,
    last: i32,
}

/// The `XPATH_MAX_STEPS` limit as `xmlXPathCompExprAdd` reaches it (the step table doubles
/// from 10 until it would exceed one million).
const MAX_STEPS: usize = 1_310_720;

struct Compiler<'a> {
    s: &'a [u8],
    cur: usize,
    error: i32,
    steps: Vec<Step>,
    last: i32,
    depth: i32,
    /// `XML_XPATH_NOVAR`: variable references are an error.
    novar: bool,
}

impl<'a> Compiler<'a> {
    fn c(&self) -> u8 {
        self.s.get(self.cur).copied().unwrap_or(0)
    }

    fn nxt(&self, n: usize) -> u8 {
        self.s.get(self.cur + n).copied().unwrap_or(0)
    }

    fn next(&mut self) {
        if self.c() != 0 {
            self.cur += 1;
        }
    }

    fn skip(&mut self, n: usize) {
        self.cur += n;
    }

    fn skip_blanks(&mut self) {
        while is_blank(self.c()) {
            self.next();
        }
    }

    /// `xmlXPathErr`: only the first error counts.
    fn err(&mut self, code: i32) {
        if self.error == 0 {
            self.error = code;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        ch1: i32,
        ch2: i32,
        op: Op,
        value: i32,
        value2: i32,
        value3: i32,
        lit: Option<Lit>,
        s4: Option<String>,
        s5: Option<String>,
    ) -> i32 {
        if self.steps.len() >= MAX_STEPS {
            self.error = MEMORY_ERROR;
            return -1;
        }
        self.steps.push(Step {
            op,
            ch1: Cell::new(ch1),
            ch2: Cell::new(ch2),
            value: Cell::new(value),
            value2,
            value3,
            lit,
            s4,
            s5,
        });
        self.last = self.steps.len() as i32 - 1;
        self.last
    }

    fn push_long(
        &mut self,
        op: Op,
        value: i32,
        value2: i32,
        value3: i32,
        lit: Option<Lit>,
        s4: Option<String>,
        s5: Option<String>,
    ) -> i32 {
        let last = self.last;
        self.add(last, -1, op, value, value2, value3, lit, s4, s5)
    }

    fn push_leave(&mut self, op: Op) {
        self.add(-1, -1, op, 0, 0, 0, None, None, None);
    }

    fn push_unary(&mut self, op: Op, ch: i32, value: i32) {
        self.add(ch, -1, op, value, 0, 0, None, None, None);
    }

    fn push_binary(&mut self, op: Op, ch1: i32, ch2: i32, value: i32, value2: i32) {
        self.add(ch1, ch2, op, value, value2, 0, None, None, None);
    }

    /// `xmlXPathCurrentChar`: the character at the cursor and its length; characters outside
    /// the XML range are an error and read as 0.
    fn cur_char(&mut self) -> (u32, usize) {
        let b = self.c();
        if b < 0x80 {
            return (b as u32, 1);
        }
        let s = std::str::from_utf8(&self.s[self.cur..]).ok().or_else(|| {
            let end = (self.cur + 4).min(self.s.len());
            (self.cur + 1..=end)
                .rev()
                .find_map(|e| std::str::from_utf8(&self.s[self.cur..e]).ok())
        });
        match s.and_then(|s| s.chars().next()) {
            Some(ch) => {
                let l = ch.len_utf8();
                if !is_xml_char(ch as u32) {
                    self.err(INVALID_CHAR_ERROR);
                    return (0, l);
                }
                (ch as u32, l)
            }
            None => {
                self.err(ENCODING_ERROR);
                (0, 0)
            }
        }
    }

    fn text(&self, from: usize, to: usize) -> String {
        String::from_utf8_lossy(&self.s[from..to]).into_owned()
    }

    /// `xmlXPathParseNCName`.
    fn parse_ncname(&mut self) -> Option<String> {
        let mut i = self.cur;
        let b = |i: usize| self.s.get(i).copied().unwrap_or(0);
        if b(i).is_ascii_alphabetic() || b(i) == b'_' {
            i += 1;
            while b(i).is_ascii_alphanumeric() || matches!(b(i), b'_' | b'.' | b'-') {
                i += 1;
            }
            if matches!(b(i), b' ' | b'>' | b'/' | b'[' | b']' | b':' | b'@' | b'*') {
                let r = self.text(self.cur, i);
                self.cur = i;
                return Some(r);
            }
        }
        self.parse_name_complex(false)
    }

    /// `xmlXPathParseQName`: (name, prefix).
    fn parse_qname(&mut self) -> (Option<String>, Option<String>) {
        let mut prefix = None;
        let mut ret = self.parse_ncname();
        if ret.is_some() && self.c() == b':' {
            prefix = ret;
            self.next();
            ret = self.parse_ncname();
        }
        (ret, prefix)
    }

    fn name_char(c: u32, qualified: bool) -> bool {
        is_letter(c)
            || chvalid::is_digit(c)
            || c == '.' as u32
            || c == '-' as u32
            || c == '_' as u32
            || (qualified && c == ':' as u32)
            || chvalid::is_combining(c)
            || chvalid::is_extender(c)
    }

    /// `xmlXPathParseNameComplex`.
    fn parse_name_complex(&mut self, qualified: bool) -> Option<String> {
        let start = self.cur;
        let (mut c, mut l) = self.cur_char();
        if matches!(c, 0x20 | 0x3e | 0x2f | 0x5b | 0x5d | 0x40 | 0x2a)
            || (!is_letter(c) && c != '_' as u32 && (!qualified || c != ':' as u32))
        {
            return None;
        }
        let mut len = 0usize;
        while c != ' ' as u32 && c != '>' as u32 && c != '/' as u32 && Self::name_char(c, qualified)
        {
            len += l;
            self.cur += l;
            (c, l) = self.cur_char();
            if len >= 100 {
                // XML_MAX_NAMELEN: switch to the growing buffer.
                let mut max = len * 2;
                while Self::name_char(c, qualified) {
                    if len + 10 > max {
                        if max > 50000 {
                            self.err(EXPR_ERROR);
                            return None;
                        }
                        max *= 2;
                    }
                    len += l;
                    self.cur += l;
                    (c, l) = self.cur_char();
                }
                return Some(self.text(start, self.cur));
            }
        }
        if len == 0 {
            return None;
        }
        Some(self.text(start, self.cur))
    }

    /// `xmlXPathScanName`: the name at the cursor, without consuming it.
    fn scan_name(&mut self) -> Option<String> {
        let start = self.cur;
        let (mut c, mut l) = self.cur_char();
        if matches!(c, 0x20 | 0x3e | 0x2f) || (!is_letter(c) && c != '_' as u32 && c != ':' as u32)
        {
            return None;
        }
        while c != ' ' as u32 && c != '>' as u32 && c != '/' as u32 && Self::name_char(c, true) {
            self.cur += l;
            (c, l) = self.cur_char();
        }
        let r = self.text(start, self.cur);
        self.cur = start;
        Some(r)
    }

    /// `xmlXPathCompNumber`.
    fn comp_number(&mut self) {
        if self.error != 0 {
            return;
        }
        if self.c() != b'.' && !self.c().is_ascii_digit() {
            self.err(NUMBER_ERROR);
            return;
        }
        let mut ret = 0.0f64;
        let mut ok = false;
        while self.c().is_ascii_digit() {
            ret = ret * 10.0 + (self.c() - b'0') as f64;
            ok = true;
            self.next();
        }
        if self.c() == b'.' {
            self.next();
            if !self.c().is_ascii_digit() && !ok {
                self.err(NUMBER_ERROR);
                return;
            }
            let mut frac = 0i32;
            let mut fraction = 0.0f64;
            while self.c() == b'0' {
                frac += 1;
                self.next();
            }
            let max = frac + 20;
            while self.c().is_ascii_digit() && frac < max {
                fraction = fraction * 10.0 + (self.c() - b'0') as f64;
                frac += 1;
                self.next();
            }
            fraction /= 10f64.powf(frac as f64);
            ret += fraction;
            while self.c().is_ascii_digit() {
                self.next();
            }
        }
        if self.c() == b'e' || self.c() == b'E' {
            self.next();
            let mut neg = false;
            if self.c() == b'-' {
                neg = true;
                self.next();
            } else if self.c() == b'+' {
                self.next();
            }
            let mut exponent = 0i32;
            while self.c().is_ascii_digit() {
                if exponent < 1_000_000 {
                    exponent = exponent * 10 + (self.c() - b'0') as i32;
                }
                self.next();
            }
            if neg {
                exponent = -exponent;
            }
            ret *= 10f64.powf(exponent as f64);
        }
        self.push_long(Op::Value, 3, 0, 0, Some(Lit::Number(ret)), None, None);
    }

    /// `xmlXPathParseLiteral`.
    fn parse_literal(&mut self) -> Option<String> {
        let quote = match self.c() {
            b'"' => b'"',
            b'\'' => b'\'',
            _ => {
                self.err(START_LITERAL_ERROR);
                return None;
            }
        };
        self.next();
        let q = self.cur;
        while self.c() != quote {
            if self.c() == 0 {
                self.err(UNFINISHED_LITERAL_ERROR);
                return None;
            }
            let (ch, len) = utf8_char_at(self.s, self.cur);
            if !is_xml_char(ch) {
                self.err(INVALID_CHAR_ERROR);
                return None;
            }
            self.cur += len;
        }
        let ret = self.text(q, self.cur);
        self.next();
        Some(ret)
    }

    fn comp_literal(&mut self) {
        if let Some(s) = self.parse_literal() {
            self.push_long(Op::Value, 4, 0, 0, Some(Lit::Str(s)), None, None);
        }
    }

    /// `xmlXPathCompVariableReference`.
    fn comp_variable_reference(&mut self) {
        self.skip_blanks();
        if self.c() != b'$' {
            self.err(VARIABLE_REF_ERROR);
            return;
        }
        self.next();
        let (name, prefix) = self.parse_qname();
        let Some(name) = name else {
            self.err(VARIABLE_REF_ERROR);
            return;
        };
        self.last = -1;
        self.push_long(Op::Variable, 0, 0, 0, None, Some(name), prefix);
        self.skip_blanks();
        if self.novar {
            self.err(FORBID_VARIABLE_ERROR);
        }
    }

    /// `xmlXPathCompFunctionCall`.
    fn comp_function_call(&mut self) {
        let (name, prefix) = self.parse_qname();
        let Some(name) = name else {
            self.err(EXPR_ERROR);
            return;
        };
        self.skip_blanks();
        if self.c() != b'(' {
            self.err(EXPR_ERROR);
            return;
        }
        self.next();
        self.skip_blanks();
        let sort = !(prefix.is_none() && name == "count");
        self.last = -1;
        let mut nbargs = 0;
        if self.c() != b')' {
            while self.c() != 0 {
                let op1 = self.last;
                self.last = -1;
                self.compile_expr(sort);
                if self.error != 0 {
                    return;
                }
                let last = self.last;
                self.push_binary(Op::Arg, op1, last, 0, 0);
                nbargs += 1;
                if self.c() == b')' {
                    break;
                }
                if self.c() != b',' {
                    self.err(EXPR_ERROR);
                    return;
                }
                self.next();
                self.skip_blanks();
            }
        }
        self.push_long(Op::Function, nbargs, 0, 0, None, Some(name), prefix);
        self.next();
        self.skip_blanks();
    }

    /// `xmlXPathCompPrimaryExpr`.
    fn comp_primary_expr(&mut self) {
        self.skip_blanks();
        if self.c() == b'$' {
            self.comp_variable_reference();
        } else if self.c() == b'(' {
            self.next();
            self.skip_blanks();
            self.compile_expr(true);
            if self.error != 0 {
                return;
            }
            if self.c() != b')' {
                self.err(EXPR_ERROR);
                return;
            }
            self.next();
            self.skip_blanks();
        } else if self.c().is_ascii_digit() || (self.c() == b'.' && self.nxt(1).is_ascii_digit()) {
            self.comp_number();
        } else if self.c() == b'\'' || self.c() == b'"' {
            self.comp_literal();
        } else {
            self.comp_function_call();
        }
        self.skip_blanks();
    }

    /// `xmlXPathCompFilterExpr`.
    fn comp_filter_expr(&mut self) {
        self.comp_primary_expr();
        if self.error != 0 {
            return;
        }
        self.skip_blanks();
        while self.c() == b'[' {
            self.comp_predicate(true);
            self.skip_blanks();
        }
    }

    fn is_node_type(name: &str) -> bool {
        matches!(name, "node" | "text" | "comment" | "processing-instruction")
    }

    /// `xmlXPathCompPathExpr`.
    fn comp_path_expr(&mut self) {
        let mut lc = true;
        self.skip_blanks();
        let c = self.c();
        if c == b'$'
            || c == b'('
            || c.is_ascii_digit()
            || c == b'\''
            || c == b'"'
            || (c == b'.' && self.nxt(1).is_ascii_digit())
        {
            lc = false;
        } else if matches!(c, b'*' | b'/' | b'@' | b'.') {
            lc = true;
        } else {
            self.skip_blanks();
            match self.scan_name() {
                Some(name) if name.contains("::") => lc = true,
                Some(name) => {
                    let mut len = name.len();
                    while self.nxt(len) != 0 {
                        let n = self.nxt(len);
                        if is_blank(n) {
                            len += 1;
                            continue;
                        }
                        lc = if n == b'(' {
                            Self::is_node_type(&name)
                        } else {
                            true
                        };
                        break;
                    }
                    if self.nxt(len) == 0 {
                        lc = true;
                    }
                }
                None => {
                    self.err(EXPR_ERROR);
                    return;
                }
            }
        }
        if lc {
            if self.c() == b'/' {
                self.push_leave(Op::Root);
            } else {
                self.push_leave(Op::Node);
            }
            self.comp_location_path();
        } else {
            self.comp_filter_expr();
            if self.error != 0 {
                return;
            }
            if self.c() == b'/' && self.nxt(1) == b'/' {
                self.skip(2);
                self.skip_blanks();
                self.push_long(
                    Op::Collect,
                    AXIS_DESCENDANT_OR_SELF,
                    TEST_TYPE,
                    TYPE_NODE,
                    None,
                    None,
                    None,
                );
                self.comp_relative_location_path();
            } else if self.c() == b'/' {
                self.comp_relative_location_path();
            }
        }
        self.skip_blanks();
    }

    /// `xmlXPathCompUnionExpr`.
    fn comp_union_expr(&mut self) {
        self.comp_path_expr();
        if self.error != 0 {
            return;
        }
        self.skip_blanks();
        while self.c() == b'|' {
            let op1 = self.last;
            self.push_leave(Op::Node);
            self.next();
            self.skip_blanks();
            self.comp_path_expr();
            let last = self.last;
            self.push_binary(Op::Union, op1, last, 0, 0);
            self.skip_blanks();
        }
    }

    /// `xmlXPathCompUnaryExpr`.
    fn comp_unary_expr(&mut self) {
        let mut minus = false;
        let mut found = false;
        self.skip_blanks();
        while self.c() == b'-' {
            minus = !minus;
            found = true;
            self.next();
            self.skip_blanks();
        }
        self.comp_union_expr();
        if self.error != 0 {
            return;
        }
        if found {
            let last = self.last;
            self.push_unary(Op::Plus, last, if minus { 2 } else { 3 });
        }
    }

    /// `xmlXPathCompMultiplicativeExpr`.
    fn comp_multiplicative_expr(&mut self) {
        self.comp_unary_expr();
        if self.error != 0 {
            return;
        }
        self.skip_blanks();
        loop {
            let op = if self.c() == b'*' {
                self.next();
                0
            } else if self.c() == b'd' && self.nxt(1) == b'i' && self.nxt(2) == b'v' {
                self.skip(3);
                1
            } else if self.c() == b'm' && self.nxt(1) == b'o' && self.nxt(2) == b'd' {
                self.skip(3);
                2
            } else {
                break;
            };
            let op1 = self.last;
            self.skip_blanks();
            self.comp_unary_expr();
            if self.error != 0 {
                return;
            }
            let last = self.last;
            self.push_binary(Op::Mult, op1, last, op, 0);
            self.skip_blanks();
        }
    }

    /// `xmlXPathCompAdditiveExpr`.
    fn comp_additive_expr(&mut self) {
        self.comp_multiplicative_expr();
        if self.error != 0 {
            return;
        }
        self.skip_blanks();
        while self.c() == b'+' || self.c() == b'-' {
            let op1 = self.last;
            let plus = (self.c() == b'+') as i32;
            self.next();
            self.skip_blanks();
            self.comp_multiplicative_expr();
            if self.error != 0 {
                return;
            }
            let last = self.last;
            self.push_binary(Op::Plus, op1, last, plus, 0);
            self.skip_blanks();
        }
    }

    /// `xmlXPathCompRelationalExpr`.
    fn comp_relational_expr(&mut self) {
        self.comp_additive_expr();
        if self.error != 0 {
            return;
        }
        self.skip_blanks();
        while self.c() == b'<' || self.c() == b'>' {
            let op1 = self.last;
            let inf = (self.c() == b'<') as i32;
            let strict = (self.nxt(1) != b'=') as i32;
            self.next();
            if strict == 0 {
                self.next();
            }
            self.skip_blanks();
            self.comp_additive_expr();
            if self.error != 0 {
                return;
            }
            let last = self.last;
            self.push_binary(Op::Cmp, op1, last, inf, strict);
            self.skip_blanks();
        }
    }

    /// `xmlXPathCompEqualityExpr`.
    fn comp_equality_expr(&mut self) {
        self.comp_relational_expr();
        if self.error != 0 {
            return;
        }
        self.skip_blanks();
        while self.c() == b'=' || (self.c() == b'!' && self.nxt(1) == b'=') {
            let op1 = self.last;
            let eq = (self.c() == b'=') as i32;
            self.next();
            if eq == 0 {
                self.next();
            }
            self.skip_blanks();
            self.comp_relational_expr();
            if self.error != 0 {
                return;
            }
            let last = self.last;
            self.push_binary(Op::Equal, op1, last, eq, 0);
            self.skip_blanks();
        }
    }

    /// `xmlXPathCompAndExpr`.
    fn comp_and_expr(&mut self) {
        self.comp_equality_expr();
        if self.error != 0 {
            return;
        }
        self.skip_blanks();
        while self.c() == b'a' && self.nxt(1) == b'n' && self.nxt(2) == b'd' {
            let op1 = self.last;
            self.skip(3);
            self.skip_blanks();
            self.comp_equality_expr();
            if self.error != 0 {
                return;
            }
            let last = self.last;
            self.push_binary(Op::And, op1, last, 0, 0);
            self.skip_blanks();
        }
    }

    /// `xmlXPathCompileExpr`.
    fn compile_expr(&mut self, sort: bool) {
        if self.depth >= MAX_RECURSION_DEPTH || crate::stack::check().is_err() {
            self.err(RECURSION_LIMIT_EXCEEDED);
            return;
        }
        self.depth += 10;
        self.comp_and_expr();
        if self.error != 0 {
            return;
        }
        self.skip_blanks();
        while self.c() == b'o' && self.nxt(1) == b'r' {
            let op1 = self.last;
            self.skip(2);
            self.skip_blanks();
            self.comp_and_expr();
            if self.error != 0 {
                return;
            }
            let last = self.last;
            self.push_binary(Op::Or, op1, last, 0, 0);
            self.skip_blanks();
        }
        if sort && self.last >= 0 && self.steps[self.last as usize].op != Op::Value {
            let last = self.last;
            self.push_unary(Op::Sort, last, 0);
        }
        self.depth -= 10;
    }

    /// `xmlXPathCompPredicate`.
    fn comp_predicate(&mut self, filter: bool) {
        let op1 = self.last;
        self.skip_blanks();
        if self.c() != b'[' {
            self.err(INVALID_PREDICATE_ERROR);
            return;
        }
        self.next();
        self.skip_blanks();
        self.last = -1;
        self.compile_expr(filter);
        if self.error != 0 {
            return;
        }
        if self.c() != b']' {
            self.err(INVALID_PREDICATE_ERROR);
            return;
        }
        let last = self.last;
        self.push_binary(
            if filter { Op::Filter } else { Op::Predicate },
            op1,
            last,
            0,
            0,
        );
        self.next();
        self.skip_blanks();
    }

    /// `xmlXPathCompNodeTest`: (test, type, prefix, name).
    fn comp_node_test(
        &mut self,
        name: Option<String>,
    ) -> (i32, i32, Option<String>, Option<String>) {
        self.skip_blanks();
        if name.is_none() && self.c() == b'*' {
            self.next();
            return (TEST_ALL, 0, None, None);
        }
        let name = match name.or_else(|| self.parse_ncname()) {
            Some(n) => n,
            None => {
                self.err(EXPR_ERROR);
                return (TEST_NONE, 0, None, None);
            }
        };
        let blanks = is_blank(self.c());
        self.skip_blanks();
        if self.c() == b'(' {
            self.next();
            let ty = match name.as_str() {
                "comment" => TYPE_COMMENT,
                "node" => TYPE_NODE,
                "processing-instruction" => TYPE_PI,
                "text" => TYPE_TEXT,
                _ => {
                    self.err(EXPR_ERROR);
                    return (TEST_NONE, 0, None, None);
                }
            };
            let mut test = TEST_TYPE;
            let mut name = Some(name);
            self.skip_blanks();
            if ty == TYPE_PI {
                name = None;
                if self.c() != b')' {
                    name = self.parse_literal();
                    test = TEST_PI;
                    self.skip_blanks();
                }
            }
            if self.c() != b')' {
                self.err(UNCLOSED_ERROR);
                return (test, ty, None, None);
            }
            self.next();
            return (test, ty, None, name);
        }
        if !blanks && self.c() == b':' {
            self.next();
            let prefix = Some(name);
            if self.c() == b'*' {
                self.next();
                return (TEST_ALL, 0, prefix, None);
            }
            return match self.parse_ncname() {
                Some(n) => (TEST_NAME, 0, prefix, Some(n)),
                None => {
                    self.err(EXPR_ERROR);
                    (TEST_NAME, 0, prefix, None)
                }
            };
        }
        (TEST_NAME, 0, None, Some(name))
    }

    fn axis_name(name: &str) -> i32 {
        match name {
            "ancestor" => AXIS_ANCESTOR,
            "ancestor-or-self" => AXIS_ANCESTOR_OR_SELF,
            "attribute" => AXIS_ATTRIBUTE,
            "child" => AXIS_CHILD,
            "descendant" => AXIS_DESCENDANT,
            "descendant-or-self" => AXIS_DESCENDANT_OR_SELF,
            "following" => AXIS_FOLLOWING,
            "following-sibling" => AXIS_FOLLOWING_SIBLING,
            "namespace" => AXIS_NAMESPACE,
            "parent" => AXIS_PARENT,
            "preceding" => AXIS_PRECEDING,
            "preceding-sibling" => AXIS_PRECEDING_SIBLING,
            "self" => AXIS_SELF,
            _ => 0,
        }
    }

    /// `xmlXPathCompStep`.
    fn comp_step(&mut self) {
        self.skip_blanks();
        if self.c() == b'.' && self.nxt(1) == b'.' {
            self.skip(2);
            self.skip_blanks();
            self.push_long(
                Op::Collect,
                AXIS_PARENT,
                TEST_TYPE,
                TYPE_NODE,
                None,
                None,
                None,
            );
            return;
        }
        if self.c() == b'.' {
            self.next();
            self.skip_blanks();
            return;
        }
        let mut name = None;
        let axis;
        if self.c() == b'*' {
            axis = AXIS_CHILD;
        } else {
            name = self.parse_ncname();
            if let Some(n) = &name {
                let a = Self::axis_name(n);
                if a != 0 {
                    self.skip_blanks();
                    if self.c() == b':' && self.nxt(1) == b':' {
                        self.skip(2);
                        name = None;
                        axis = a;
                    } else {
                        axis = AXIS_CHILD;
                    }
                } else {
                    axis = AXIS_CHILD;
                }
            } else if self.c() == b'@' {
                self.next();
                axis = AXIS_ATTRIBUTE;
            } else {
                axis = AXIS_CHILD;
            }
        }
        if self.error != 0 {
            return;
        }
        let (test, ty, prefix, name) = self.comp_node_test(name);
        if test == TEST_NONE {
            return;
        }
        let op1 = self.last;
        self.last = -1;
        self.skip_blanks();
        while self.c() == b'[' {
            self.comp_predicate(false);
        }
        let last = self.last;
        self.add(op1, last, Op::Collect, axis, test, ty, None, prefix, name);
    }

    /// `xmlXPathCompRelativeLocationPath`.
    fn comp_relative_location_path(&mut self) {
        self.skip_blanks();
        if self.c() == b'/' && self.nxt(1) == b'/' {
            self.skip(2);
            self.skip_blanks();
            self.push_long(
                Op::Collect,
                AXIS_DESCENDANT_OR_SELF,
                TEST_TYPE,
                TYPE_NODE,
                None,
                None,
                None,
            );
        } else if self.c() == b'/' {
            self.next();
            self.skip_blanks();
        }
        self.comp_step();
        if self.error != 0 {
            return;
        }
        self.skip_blanks();
        while self.c() == b'/' {
            if self.nxt(1) == b'/' {
                self.skip(2);
                self.skip_blanks();
                self.push_long(
                    Op::Collect,
                    AXIS_DESCENDANT_OR_SELF,
                    TEST_TYPE,
                    TYPE_NODE,
                    None,
                    None,
                    None,
                );
                self.comp_step();
            } else {
                self.next();
                self.skip_blanks();
                self.comp_step();
            }
            self.skip_blanks();
        }
    }

    /// `xmlXPathCompLocationPath`.
    fn comp_location_path(&mut self) {
        self.skip_blanks();
        if self.c() != b'/' {
            self.comp_relative_location_path();
            return;
        }
        while self.c() == b'/' {
            if self.nxt(1) == b'/' {
                self.skip(2);
                self.skip_blanks();
                self.push_long(
                    Op::Collect,
                    AXIS_DESCENDANT_OR_SELF,
                    TEST_TYPE,
                    TYPE_NODE,
                    None,
                    None,
                    None,
                );
                self.comp_relative_location_path();
            } else {
                self.next();
                self.skip_blanks();
                let c = self.c();
                if c != 0
                    && (c.is_ascii_alphabetic()
                        || c >= 0x80
                        || matches!(c, b'_' | b'.' | b'@' | b'*'))
                {
                    self.comp_relative_location_path();
                }
            }
            if self.error != 0 {
                return;
            }
        }
    }

    /// `xmlXPathOptimizeExpression`.
    fn optimize(&mut self, op: usize) {
        let step = &self.steps[op];
        if step.op == Op::Collect && step.ch1.get() != -1 && step.ch2.get() == -1 {
            let prev = &self.steps[step.ch1.get() as usize];
            if prev.op == Op::Collect
                && prev.value.get() == AXIS_DESCENDANT_OR_SELF
                && prev.ch2.get() == -1
                && prev.value2 == TEST_TYPE
                && prev.value3 == TYPE_NODE
            {
                match step.value.get() {
                    AXIS_CHILD | AXIS_DESCENDANT => {
                        step.ch1.set(prev.ch1.get());
                        step.value.set(AXIS_DESCENDANT);
                    }
                    AXIS_SELF | AXIS_DESCENDANT_OR_SELF => {
                        step.ch1.set(prev.ch1.get());
                        step.value.set(AXIS_DESCENDANT_OR_SELF);
                    }
                    _ => {}
                }
            }
        }
        if step.op == Op::Value {
            return;
        }
        if self.depth >= MAX_RECURSION_DEPTH || crate::stack::check().is_err() {
            return;
        }
        self.depth += 1;
        let (ch1, ch2) = (self.steps[op].ch1.get(), self.steps[op].ch2.get());
        if ch1 != -1 {
            self.optimize(ch1 as usize);
        }
        if ch2 != -1 {
            self.optimize(ch2 as usize);
        }
        self.depth -= 1;
    }
}

/// The character at `i` (`xmlGetUTF8Char`; the text is valid UTF-8).
fn utf8_char_at(s: &[u8], i: usize) -> (u32, usize) {
    let b = s[i];
    let len = match b {
        0..=0x7f => return (b as u32, 1),
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    };
    let end = (i + len).min(s.len());
    match std::str::from_utf8(&s[i..end])
        .ok()
        .and_then(|t| t.chars().next())
    {
        Some(c) => (c as u32, c.len_utf8()),
        None => (0xfffe, 1),
    }
}

/// An XPath error as Postgres reports it: `msg`, with libxml's message as the detail.
fn xpath_error(msg: &str, code: i32) -> Error {
    Error::new(code::INVALID_ARGUMENT_FOR_XQUERY, msg).detail(message(code))
}

/// `xmlXPathCtxtCompile`, failing with Postgres's "invalid XPath expression".
pub fn compile(expr: &str) -> Result<Compiled> {
    compile_flags(expr, false)
}

/// `xmlXPathCtxtCompile` with the context's `XML_XPATH_NOVAR` flag as given.
pub fn compile_flags(expr: &str, novar: bool) -> Result<Compiled> {
    let mut c = Compiler {
        s: expr.as_bytes(),
        cur: 0,
        error: 0,
        steps: Vec::new(),
        last: -1,
        depth: 0,
        novar,
    };
    c.compile_expr(true);
    if c.error != 0 {
        return Err(xpath_error("invalid XPath expression", c.error));
    }
    if c.c() != 0 {
        return Err(xpath_error("invalid XPath expression", EXPR_ERROR));
    }
    if c.steps.len() > 1 && c.last >= 0 {
        c.depth = 0;
        let last = c.last as usize;
        c.optimize(last);
    }
    Ok(Compiled {
        steps: c.steps,
        last: c.last,
    })
}

// ---------------------------------------------------------------------------------------------
// Values and casts

/// An XPath result (`xmlXPathObject`).
#[derive(Clone, Debug)]
pub enum XValue {
    /// Nodes; in document order when the expression sorted them.
    NodeSet(Vec<XNode>),
    /// A result tree fragment (`XPATH_XSLT_TREE`): libxslt's variables with content. A node
    /// set to every function and operator, but no location step, filter or union takes it.
    Tree(Vec<XNode>),
    Bool(bool),
    Number(f64),
    String(String),
}

impl XValue {
    /// The nodes of a node set or a result tree fragment.
    pub fn nodes(&self) -> Option<&Vec<XNode>> {
        match self {
            XValue::NodeSet(s) | XValue::Tree(s) => Some(s),
            _ => None,
        }
    }

    fn from_lit(l: &Lit) -> XValue {
        match l {
            Lit::Number(n) => XValue::Number(*n),
            Lit::Str(s) => XValue::String(s.clone()),
        }
    }
}

/// `xmlXPathStringEvalNumber`.
pub fn string_to_number(s: &str) -> f64 {
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut cur = 0;
    while is_blank(at(cur)) {
        cur += 1;
    }
    let mut neg = false;
    if at(cur) == b'-' {
        neg = true;
        cur += 1;
    }
    if at(cur) != b'.' && !at(cur).is_ascii_digit() {
        return f64::NAN;
    }
    let mut ret = 0.0f64;
    let mut ok = false;
    while at(cur).is_ascii_digit() {
        ret = ret * 10.0 + (at(cur) - b'0') as f64;
        ok = true;
        cur += 1;
    }
    if at(cur) == b'.' {
        cur += 1;
        if !at(cur).is_ascii_digit() && !ok {
            return f64::NAN;
        }
        let mut frac = 0i32;
        let mut fraction = 0.0f64;
        while at(cur) == b'0' {
            frac += 1;
            cur += 1;
        }
        let max = frac + 20;
        while at(cur).is_ascii_digit() && frac < max {
            fraction = fraction * 10.0 + (at(cur) - b'0') as f64;
            frac += 1;
            cur += 1;
        }
        fraction /= 10f64.powf(frac as f64);
        ret += fraction;
        while at(cur).is_ascii_digit() {
            cur += 1;
        }
    }
    let mut exponent = 0i32;
    let mut exp_neg = false;
    if at(cur) == b'e' || at(cur) == b'E' {
        cur += 1;
        if at(cur) == b'-' {
            exp_neg = true;
            cur += 1;
        } else if at(cur) == b'+' {
            cur += 1;
        }
        while at(cur).is_ascii_digit() {
            if exponent < 1_000_000 {
                exponent = exponent * 10 + (at(cur) - b'0') as i32;
            }
            cur += 1;
        }
    }
    while is_blank(at(cur)) {
        cur += 1;
    }
    if cur != b.len() {
        return f64::NAN;
    }
    if neg {
        ret = -ret;
    }
    if exp_neg {
        exponent = -exponent;
    }
    ret * 10f64.powf(exponent as f64)
}

/// `xmlXPathCastNumberToString` (`xmlXPathFormatNumber`).
pub fn cast_number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if n == 0.0 {
        return "0".into();
    }
    if n > i32::MIN as f64 && n < i32::MAX as f64 && n == (n as i32) as f64 {
        return (n as i32).to_string();
    }
    let abs = n.abs();
    let mut work = if abs > 1e9 || abs < 1e-5 {
        // "%21.14e", cut at the exponent.
        let s = format!("{n:.14e}");
        let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let e: i32 = exp.parse().unwrap_or(0);
        let mut m = strip_fraction_zeros(mant);
        m.push_str(&format!("e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs()));
        return m;
    } else {
        let ip = abs.log10() as i32;
        let frac = if ip > 0 { 15 - ip - 1 } else { 15 - ip };
        format!("{n:.*}", frac.max(0) as usize)
    };
    work = strip_fraction_zeros(&work);
    work
}

/// Drops trailing zeros of a fraction, and the point if nothing is left after it.
fn strip_fraction_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let t = s.trim_end_matches('0');
    t.strip_suffix('.').unwrap_or(t).to_string()
}

fn number_to_bool(n: f64) -> bool {
    !(n.is_nan() || n == 0.0)
}

fn bool_to_number(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

/// `xmlXPathCastToBoolean`.
fn cast_to_bool(v: &XValue) -> bool {
    match v {
        XValue::NodeSet(s) | XValue::Tree(s) => !s.is_empty(),
        XValue::String(s) => !s.is_empty(),
        XValue::Number(n) => number_to_bool(*n),
        XValue::Bool(b) => *b,
    }
}

/// `xmlXPathCastNodeSetToString`: the string value of the first node in document order (the
/// set is sorted in place, as libxml does).
fn cast_node_set_to_string(x: &XDoc, set: &mut [XNode]) -> String {
    if set.is_empty() {
        return String::new();
    }
    if set.len() > 1 {
        x.sort(set);
    }
    x.node_string(&set[0])
}

/// `xmlXPathCastNodeToString`.
pub fn cast_node_to_string(x: &XDoc, node: &XNode) -> String {
    x.node_string(node)
}

/// `xmlXPathCastToString`.
pub fn cast_to_string(x: &XDoc, v: &mut XValue) -> String {
    match v {
        XValue::NodeSet(s) | XValue::Tree(s) => cast_node_set_to_string(x, s),
        XValue::String(s) => s.clone(),
        XValue::Bool(b) => (if *b { "true" } else { "false" }).into(),
        XValue::Number(n) => cast_number_to_string(*n),
    }
}

/// `xmlXPathCastToNumber`.
pub fn cast_to_number(x: &XDoc, v: &mut XValue) -> f64 {
    match v {
        XValue::NodeSet(s) | XValue::Tree(s) => string_to_number(&cast_node_set_to_string(x, s)),
        XValue::String(s) => string_to_number(s),
        XValue::Number(n) => *n,
        XValue::Bool(b) => bool_to_number(*b),
    }
}

/// The relational operators on two numbers, with libxml's hand-checked infinities.
fn compare_numbers(inf: bool, strict: bool, a: f64, b: f64) -> bool {
    if a.is_nan() || b.is_nan() {
        return false;
    }
    let ai = if a.is_infinite() {
        a.signum() as i32
    } else {
        0
    };
    let bi = if b.is_infinite() {
        b.signum() as i32
    } else {
        0
    };
    match (inf, strict) {
        (true, true) => {
            if (ai == -1 && bi != -1) || (bi == 1 && ai != 1) {
                true
            } else {
                ai == 0 && bi == 0 && a < b
            }
        }
        (true, false) => {
            if ai == -1 || bi == 1 {
                true
            } else {
                ai == 0 && bi == 0 && a <= b
            }
        }
        (false, true) => {
            if (ai == 1 && bi != 1) || (bi == -1 && ai != -1) {
                true
            } else {
                ai == 0 && bi == 0 && a > b
            }
        }
        (false, false) => {
            if ai == 1 || bi == -1 {
                true
            } else {
                ai == 0 && bi == 0 && a >= b
            }
        }
    }
}

/// Number equality with libxml's hand-checked NaN and infinities.
fn equal_numbers(a: f64, b: f64) -> bool {
    if a.is_nan() || b.is_nan() {
        return false;
    }
    a == b
}

// ---------------------------------------------------------------------------------------------
// Namespaces registered for an evaluation

/// The prefixes registered on the XPath context (`xmlXPathRegisterNs`).
#[derive(Clone, Debug, Default)]
pub struct NsMap {
    map: HashMap<String, String>,
}

impl NsMap {
    pub fn new() -> NsMap {
        NsMap::default()
    }

    /// `xmlXPathRegisterNs`: false (failure) for an empty prefix; a prefix registered again
    /// takes the new URI.
    pub fn register(&mut self, prefix: &str, uri: &str) -> bool {
        if prefix.is_empty() {
            return false;
        }
        self.map.insert(prefix.to_string(), uri.to_string());
        true
    }

    /// `xmlXPathNsLookup`.
    pub fn lookup(&self, prefix: &str) -> Option<&str> {
        if prefix == "xml" {
            return Some(XML_NS);
        }
        self.map.get(prefix).map(String::as_str)
    }
}

/// The namespace escape-uri() is registered under.
pub const XQUERY_NS: &str = XQUERY_FUNCTIONS_NS;
const XQUERY_FUNCTIONS_NS: &str = "http://www.w3.org/2002/08/xquery-functions";

// ---------------------------------------------------------------------------------------------
// Node sets

/// `xmlXPathNodeSetDupNs`: namespace nodes attached to an element are copied, so every set
/// owns its own.
fn dup(n: &XNode) -> XNode {
    match n {
        XNode::Ns(ns) if ns.parent.is_some() => XNode::Ns(Arc::new(NsNode {
            parent: ns.parent,
            prefix: ns.prefix.clone(),
            href: ns.href.clone(),
        })),
        n => n.clone(),
    }
}

/// `xmlXPathNodeSetAddUnique`.
fn add_unique(set: &mut Vec<XNode>, n: &XNode) {
    set.push(dup(n));
}

/// `xmlXPathNodeSetAddNs`.
fn add_ns(set: &mut Vec<XNode>, elem: NodeId, ns: &NsNode) {
    if set
        .iter()
        .any(|x| matches!(x, XNode::Ns(o) if o.parent == Some(elem) && o.prefix == ns.prefix))
    {
        return;
    }
    set.push(XNode::Ns(Arc::new(NsNode {
        parent: Some(elem),
        prefix: ns.prefix.clone(),
        href: ns.href.clone(),
    })));
}

/// The duplicate test of the merges: the same node, or namespace nodes for the same prefix
/// on the same element.
fn same_or_equal_ns(a: &XNode, b: &XNode) -> bool {
    match (a, b) {
        (XNode::Ns(x), XNode::Ns(y)) => {
            Arc::ptr_eq(x, y) || (x.parent == y.parent && x.prefix == y.prefix)
        }
        _ => a.same(b),
    }
}

/// `xmlXPathNodeSetMerge`.
fn merge(mut a: Vec<XNode>, b: &[XNode]) -> Vec<XNode> {
    let init = a.len();
    for n2 in b {
        if a[..init].iter().any(|n1| same_or_equal_ns(n1, n2)) {
            continue;
        }
        a.push(dup(n2));
    }
    a
}

/// `xmlXPathNodeSetMergeAndClear`.
fn merge_and_clear(mut a: Vec<XNode>, b: Vec<XNode>) -> Vec<XNode> {
    let init = a.len();
    for n2 in b {
        if a[..init].iter().any(|n1| same_or_equal_ns(n1, &n2)) {
            continue;
        }
        a.push(n2);
    }
    a
}

// ---------------------------------------------------------------------------------------------
// Evaluation (xmlXPathCompOpEval and friends)

/// libxml node type numbers (`xmlElementType`).
fn node_type(x: &XDoc, n: &XNode) -> i32 {
    match n {
        XNode::Ns(_) => 18,
        XNode::Node(id) => match x.kind(*id) {
            Kind::Element => 1,
            Kind::Attribute => 2,
            Kind::Text => 3,
            Kind::CData => 4,
            Kind::EntityRef => 5,
            Kind::Pi => 7,
            Kind::Comment => 8,
            Kind::Document => 9,
            Kind::Dtd => 14,
            Kind::ElementDecl => 15,
            Kind::AttributeDecl => 16,
            Kind::EntityDecl => 17,
            Kind::Namespace => 18,
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    No,
    Node,
    Ns,
}

struct Ctx<'a> {
    x: &'a XDoc,
    comp: &'a Compiled,
    ns: &'a NsMap,
    stack: Vec<XValue>,
    error: i32,
    node: XNode,
    size: i32,
    pos: i32,
    depth: i32,
    /// `tmpNsList` of the namespace axis.
    tmp_ns: Vec<Arc<NsNode>>,
    /// `ctxt->ancestor` of the preceding axis.
    ancestor: Option<NodeId>,
    /// `ctxt->context->doc`: the document node `/` selects.
    root: NodeId,
    /// Variables and the functions beyond XPath's own (libxslt's).
    host: Option<&'a dyn Host>,
    /// Steps taken on the following axis, which cycles through entity content in libxml.
    following_steps: usize,
}

impl<'a> Ctx<'a> {
    fn new(x: &'a XDoc, comp: &'a Compiled, ns: &'a NsMap, node: XNode) -> Ctx<'a> {
        Ctx {
            x,
            comp,
            ns,
            stack: Vec::new(),
            error: 0,
            node,
            size: -1,
            pos: -1,
            depth: 0,
            tmp_ns: Vec::new(),
            ancestor: None,
            root: DOC,
            host: None,
            following_steps: 0,
        }
    }

    fn err(&mut self, code: i32) {
        if self.error == 0 {
            self.error = code;
        }
    }

    fn step(&self, i: i32) -> &'a Step {
        &self.comp.steps[i as usize]
    }

    fn pop(&mut self) -> Option<XValue> {
        self.stack.pop()
    }

    fn push(&mut self, v: XValue) {
        self.stack.push(v);
    }

    /// A node set or a result tree fragment on top (the functions' type check).
    fn top_is_nodeset(&self) -> bool {
        matches!(self.stack.last(), Some(XValue::NodeSet(_) | XValue::Tree(_)))
    }

    /// `CHECK_TYPE0(XPATH_NODESET)`: a node set proper (location steps, filters, unions).
    fn top_is_strict_nodeset(&self) -> bool {
        matches!(self.stack.last(), Some(XValue::NodeSet(_)))
    }

    /// Pops a node set or a result tree fragment's nodes.
    fn pop_nodes(&mut self) -> Option<Vec<XNode>> {
        match self.pop() {
            Some(XValue::NodeSet(s) | XValue::Tree(s)) => Some(s),
            _ => None,
        }
    }

    /// `CHECK_ARITY`.
    fn check_arity(&mut self, nargs: i32, x: i32) -> bool {
        if nargs != x {
            self.err(INVALID_ARITY);
            return false;
        }
        if (self.stack.len() as i32) < x {
            self.err(STACK_ERROR);
            return false;
        }
        true
    }

    fn cast_top_to_string(&mut self) {
        if matches!(self.stack.last(), Some(v) if !matches!(v, XValue::String(_))) {
            self.f_string(1);
        }
    }

    fn cast_top_to_number(&mut self) {
        if matches!(self.stack.last(), Some(v) if !matches!(v, XValue::Number(_))) {
            self.f_number(1);
        }
    }

    fn cast_top_to_boolean(&mut self) {
        if matches!(self.stack.last(), Some(v) if !matches!(v, XValue::Bool(_))) {
            self.f_boolean(1);
        }
    }

    /// `CHECK_TYPE` for strings, numbers and booleans.
    fn check_top(&mut self, f: fn(&XValue) -> bool) -> bool {
        match self.stack.last() {
            Some(v) if f(v) => true,
            _ => {
                self.err(INVALID_TYPE);
                false
            }
        }
    }

    fn top_number(&mut self) -> Option<&mut f64> {
        match self.stack.last_mut() {
            Some(XValue::Number(n)) => Some(n),
            _ => None,
        }
    }

    /// `xmlXPathCacheNewNodeSet`.
    fn node_set_of(&self, n: &XNode) -> XValue {
        XValue::NodeSet(vec![dup(n)])
    }

    // --- axes

    fn ctx_id(&self) -> Option<NodeId> {
        self.node.id()
    }

    fn kind_of(&self, n: &XNode) -> Option<Kind> {
        n.id().map(|id| self.x.kind(id))
    }

    fn next_child(&self, cur: Option<&XNode>) -> Option<XNode> {
        let x = self.x;
        match cur {
            None => {
                let id = self.ctx_id()?;
                match x.kind(id) {
                    Kind::Element
                    | Kind::Text
                    | Kind::CData
                    | Kind::EntityRef
                    | Kind::Pi
                    | Kind::Comment
                    | Kind::Dtd
                    | Kind::Document => x.first_child(id).map(XNode::Node),
                    _ => None,
                }
            }
            Some(c) => {
                let id = c.id()?;
                if x.kind(id) == Kind::Document {
                    return None;
                }
                x.next(id).map(XNode::Node)
            }
        }
    }

    fn next_child_element(&self, cur: Option<&XNode>) -> Option<XNode> {
        let x = self.x;
        let is_elem = |n: NodeId| x.kind(n) == Kind::Element;
        match cur {
            None => {
                let id = self.ctx_id()?;
                match x.kind(id) {
                    Kind::Element | Kind::EntityRef => {
                        let mut c = x.first_child(id)?;
                        while !is_elem(c) {
                            c = x.next(c)?;
                        }
                        Some(XNode::Node(c))
                    }
                    Kind::Document => x.doc.children(id).find(|&c| is_elem(c)).map(XNode::Node),
                    _ => None,
                }
            }
            Some(c) => {
                let id = c.id()?;
                match x.kind(id) {
                    Kind::Element
                    | Kind::Text
                    | Kind::EntityRef
                    | Kind::CData
                    | Kind::Pi
                    | Kind::Comment => {}
                    _ => return None,
                }
                let mut n = x.next(id)?;
                while !is_elem(n) {
                    n = x.next(n)?;
                }
                Some(XNode::Node(n))
            }
        }
    }

    fn next_descendant(&self, cur: Option<&XNode>) -> Option<XNode> {
        let x = self.x;
        let ctx = self.ctx_id();
        let mut cur = match cur {
            None => {
                let id = ctx?;
                if matches!(x.kind(id), Kind::Attribute) {
                    return None;
                }
                return x.first_child(id).map(XNode::Node);
            }
            Some(c) => c.id()?,
        };
        if let Some(c) = x.first_child(cur)
            && x.kind(c) != Kind::EntityDecl
        {
            cur = c;
            if x.kind(cur) != Kind::Dtd {
                return Some(XNode::Node(cur));
            }
        }
        if Some(cur) == ctx {
            return None;
        }
        while let Some(n) = x.next(cur) {
            cur = n;
            if !matches!(x.kind(cur), Kind::EntityDecl | Kind::Dtd) {
                return Some(XNode::Node(cur));
            }
        }
        loop {
            cur = x.parent(cur)?;
            if Some(cur) == ctx {
                return None;
            }
            if let Some(n) = x.next(cur) {
                return Some(XNode::Node(n));
            }
        }
    }

    fn next_descendant_or_self(&self, cur: Option<&XNode>) -> Option<XNode> {
        match cur {
            None => Some(self.node.clone()),
            Some(_) => {
                let id = self.ctx_id()?;
                if self.x.kind(id) == Kind::Attribute {
                    return None;
                }
                self.next_descendant(cur)
            }
        }
    }

    /// The parent step shared by the parent and ancestor axes.
    fn parent_of(&self, n: &XNode) -> Option<XNode> {
        let x = self.x;
        match n {
            XNode::Ns(ns) => ns.parent.map(XNode::Node),
            XNode::Node(id) => match x.kind(*id) {
                Kind::Document => None,
                Kind::Attribute => x.parent(*id).map(XNode::Node),
                _ => Some(XNode::Node(x.parent(*id).unwrap_or(self.root))),
            },
        }
    }

    fn next_parent(&self, cur: Option<&XNode>) -> Option<XNode> {
        match cur {
            None => self.parent_of(&self.node),
            Some(_) => None,
        }
    }

    fn next_ancestor(&self, cur: Option<&XNode>) -> Option<XNode> {
        let x = self.x;
        let Some(c) = cur else {
            return self.parent_of(&self.node);
        };
        if let XNode::Node(id) = c {
            if Some(*id) == x.first_child(self.root) {
                return Some(XNode::Node(self.root));
            }
            if *id == self.root {
                return None;
            }
            return match x.kind(*id) {
                Kind::Document => None,
                _ => x.parent(*id).map(XNode::Node),
            };
        }
        self.parent_of(c)
    }

    fn next_ancestor_or_self(&self, cur: Option<&XNode>) -> Option<XNode> {
        match cur {
            None => Some(self.node.clone()),
            Some(_) => self.next_ancestor(cur),
        }
    }

    fn next_following_sibling(&self, cur: Option<&XNode>) -> Option<XNode> {
        let id = self.ctx_id()?;
        if self.x.kind(id) == Kind::Attribute {
            return None;
        }
        match cur {
            Some(XNode::Node(r)) if *r == self.root => None,
            None => self.x.next(id).map(XNode::Node),
            Some(c) => self.x.next(c.id()?).map(XNode::Node),
        }
    }

    fn next_preceding_sibling(&self, cur: Option<&XNode>) -> Option<XNode> {
        let x = self.x;
        let id = self.ctx_id()?;
        if x.kind(id) == Kind::Attribute {
            return None;
        }
        match cur {
            Some(XNode::Node(r)) if *r == self.root => None,
            None => x.prev(id).map(XNode::Node),
            Some(c) => {
                let mut c = c.id()?;
                if let Some(p) = x.prev(c)
                    && x.kind(p) == Kind::Dtd
                {
                    c = p;
                }
                x.prev(c).map(XNode::Node)
            }
        }
    }

    fn next_following(&mut self, cur: Option<&XNode>) -> Option<XNode> {
        let x = self.x;
        self.following_steps += 1;
        if self.following_steps > 4 * x.doc.nodes.len() + 16 {
            return None;
        }
        if let Some(XNode::Node(c)) = cur
            && x.kind(*c) != Kind::Attribute
            && let Some(f) = x.first_child(*c)
        {
            return Some(XNode::Node(f));
        }
        let mut c = match cur {
            None => match &self.node {
                XNode::Ns(ns) => ns.parent?,
                XNode::Node(id) if x.kind(*id) == Kind::Attribute => x.parent(*id)?,
                XNode::Node(id) => *id,
            },
            Some(XNode::Node(id)) => *id,
            Some(XNode::Ns(_)) => return None,
        };
        if let Some(n) = x.next(c) {
            return Some(XNode::Node(n));
        }
        loop {
            c = x.parent(c)?;
            if c == self.root {
                return None;
            }
            if let Some(n) = x.next(c) {
                return Some(XNode::Node(n));
            }
        }
    }

    fn next_preceding(&mut self, cur: Option<&XNode>) -> Option<XNode> {
        let x = self.x;
        let mut c = match cur {
            None => {
                let c = match &self.node {
                    XNode::Ns(ns) => ns.parent?,
                    XNode::Node(id) if x.kind(*id) == Kind::Attribute => x.parent(*id)?,
                    XNode::Node(id) => *id,
                };
                self.ancestor = x.parent(c);
                c
            }
            Some(XNode::Node(id)) => *id,
            Some(XNode::Ns(_)) => return None,
        };
        if let Some(p) = x.prev(c)
            && x.kind(p) == Kind::Dtd
        {
            c = p;
        }
        while x.prev(c).is_none() {
            c = x.parent(c)?;
            if Some(c) == x.first_child(self.root) {
                return None;
            }
            if Some(c) != self.ancestor {
                return Some(XNode::Node(c));
            }
            self.ancestor = x.parent(c);
        }
        c = x.prev(c)?;
        while let Some(l) = x.last_child(c) {
            c = l;
        }
        Some(XNode::Node(c))
    }

    fn next_namespace(&mut self, cur: Option<&XNode>) -> Option<XNode> {
        let x = self.x;
        let id = self.ctx_id()?;
        if x.kind(id) != Kind::Element {
            return None;
        }
        if cur.is_none() {
            self.tmp_ns.clear();
            let mut node = Some(id);
            while let Some(n) = node {
                if x.kind(n) == Kind::Element {
                    for d in x.doc.nodes[n].ns_defs() {
                        if !self.tmp_ns.iter().any(|t| t.prefix == d.prefix) {
                            self.tmp_ns.push(Arc::new(NsNode {
                                parent: None,
                                prefix: d.prefix.clone(),
                                href: d.href.clone(),
                            }));
                        }
                    }
                }
                node = x.parent(n);
            }
            return Some(XNode::Ns(Arc::new(NsNode {
                parent: None,
                prefix: Some("xml".into()),
                href: Some(XML_NS.into()),
            })));
        }
        self.tmp_ns.pop().map(XNode::Ns)
    }

    fn next_attribute(&self, cur: Option<&XNode>) -> Option<XNode> {
        let x = self.x;
        let id = self.ctx_id()?;
        if x.kind(id) != Kind::Element {
            return None;
        }
        match cur {
            None => x.doc.first_attr(id).map(XNode::Node),
            Some(c) => x.next(c.id()?).map(XNode::Node),
        }
    }

    fn next_self(&self, cur: Option<&XNode>) -> Option<XNode> {
        match cur {
            None => Some(self.node.clone()),
            Some(_) => None,
        }
    }

    fn next_on(&mut self, axis: i32, elem_child: bool, cur: Option<&XNode>) -> Option<XNode> {
        match axis {
            AXIS_ANCESTOR => self.next_ancestor(cur),
            AXIS_ANCESTOR_OR_SELF => self.next_ancestor_or_self(cur),
            AXIS_ATTRIBUTE => self.next_attribute(cur),
            AXIS_CHILD if elem_child => self.next_child_element(cur),
            AXIS_CHILD => self.next_child(cur),
            AXIS_DESCENDANT => self.next_descendant(cur),
            AXIS_DESCENDANT_OR_SELF => self.next_descendant_or_self(cur),
            AXIS_FOLLOWING => self.next_following(cur),
            AXIS_FOLLOWING_SIBLING => self.next_following_sibling(cur),
            AXIS_NAMESPACE => self.next_namespace(cur),
            AXIS_PARENT => self.next_parent(cur),
            AXIS_PRECEDING => self.next_preceding(cur),
            AXIS_PRECEDING_SIBLING => self.next_preceding_sibling(cur),
            AXIS_SELF => self.next_self(cur),
            _ => None,
        }
    }

    // --- node tests

    fn ns_href(&self, id: NodeId) -> Option<&'a str> {
        self.x.doc.nodes[id]
            .ns
            .as_ref()
            .and_then(|n| n.href.as_deref())
    }

    #[allow(clippy::too_many_arguments)]
    fn test_node(
        &self,
        cur: &XNode,
        axis: i32,
        test: i32,
        ty: i32,
        prefix: Option<&str>,
        name: Option<&str>,
        uri: Option<&str>,
    ) -> Hit {
        let x = self.x;
        let kind = self.kind_of(cur);
        match test {
            TEST_TYPE => {
                if ty == TYPE_NODE {
                    match kind {
                        Some(
                            Kind::Document
                            | Kind::Element
                            | Kind::Attribute
                            | Kind::Pi
                            | Kind::Comment
                            | Kind::CData
                            | Kind::Text,
                        ) => Hit::Node,
                        None => {
                            if axis == AXIS_NAMESPACE {
                                Hit::Ns
                            } else {
                                Hit::Node
                            }
                        }
                        _ => Hit::No,
                    }
                } else if node_type(x, cur) == ty || (ty == TYPE_TEXT && kind == Some(Kind::CData))
                {
                    Hit::Node
                } else {
                    Hit::No
                }
            }
            TEST_PI => match cur {
                XNode::Node(id)
                    if kind == Some(Kind::Pi)
                        && name.is_none_or(|n| n == x.doc.nodes[*id].name) =>
                {
                    Hit::Node
                }
                _ => Hit::No,
            },
            TEST_ALL => {
                if axis == AXIS_ATTRIBUTE {
                    match cur {
                        XNode::Node(id)
                            if kind == Some(Kind::Attribute)
                                && (prefix.is_none()
                                    || (self.ns_href(*id).is_some()
                                        && self.ns_href(*id) == uri)) =>
                        {
                            Hit::Node
                        }
                        _ => Hit::No,
                    }
                } else if axis == AXIS_NAMESPACE {
                    if kind.is_none() { Hit::Ns } else { Hit::No }
                } else {
                    match cur {
                        XNode::Node(id)
                            if kind == Some(Kind::Element)
                                && (prefix.is_none()
                                    || (self.ns_href(*id).is_some()
                                        && self.ns_href(*id) == uri)) =>
                        {
                            Hit::Node
                        }
                        _ => Hit::No,
                    }
                }
            }
            TEST_NAME => {
                let want = if axis == AXIS_ATTRIBUTE {
                    Some(Kind::Attribute)
                } else if axis == AXIS_NAMESPACE {
                    None
                } else {
                    Some(Kind::Element)
                };
                if kind != want {
                    return Hit::No;
                }
                match cur {
                    XNode::Node(id) => {
                        let n = &x.doc.nodes[*id];
                        if name != Some(n.name.as_str()) {
                            return Hit::No;
                        }
                        let ok = match prefix {
                            None => match &n.ns {
                                None => true,
                                Some(ns) => kind == Some(Kind::Attribute) && ns.prefix.is_none(),
                            },
                            Some(_) => self.ns_href(*id).is_some() && self.ns_href(*id) == uri,
                        };
                        if ok { Hit::Node } else { Hit::No }
                    }
                    XNode::Ns(ns) => {
                        if ns.prefix.is_some() && name.is_some() && ns.prefix.as_deref() == name {
                            Hit::Ns
                        } else {
                            Hit::No
                        }
                    }
                }
            }
            _ => Hit::No,
        }
    }

    /// `xmlXPathIsPositionalPredicate`.
    fn is_positional(&self, op: i32, max_pos: &mut i32) -> bool {
        let s = self.step(op);
        if s.op != Op::Predicate && s.op != Op::Filter {
            return false;
        }
        if s.ch2.get() == -1 {
            return false;
        }
        let e = self.step(s.ch2.get());
        if e.op == Op::Value
            && let Some(Lit::Number(f)) = e.lit
            && f > i32::MIN as f64
            && f < i32::MAX as f64
        {
            *max_pos = f as i32;
            return f == *max_pos as f64;
        }
        false
    }

    /// `xmlXPathNodeCollectAndTest`.
    fn collect_and_test(
        &mut self,
        op: i32,
        first: Option<XNode>,
        last: Option<XNode>,
        to_bool: bool,
    ) -> i32 {
        let s = self.step(op);
        let axis = s.value.get();
        let (test, ty) = (s.value2, s.value3);
        let prefix = s.s4.as_deref();
        let name = s.s5.as_deref();
        if !self.top_is_strict_nodeset() {
            self.err(INVALID_TYPE);
            return 0;
        }
        let Some(XValue::NodeSet(ctx_seq)) = self.pop() else {
            return 0;
        };
        let mut uri = None;
        if let Some(p) = prefix {
            match self.ns.lookup(p) {
                Some(u) => uri = Some(u),
                None => {
                    self.err(UNDEF_PREFIX_ERROR);
                    return 0;
                }
            }
        }
        let (mut first, mut last) = (first, last);
        let mut no_dupls = false;
        let mut elem_child = false;
        match axis {
            AXIS_ANCESTOR
            | AXIS_ANCESTOR_OR_SELF
            | AXIS_PARENT
            | AXIS_PRECEDING
            | AXIS_PRECEDING_SIBLING => first = None,
            AXIS_ATTRIBUTE | AXIS_NAMESPACE | AXIS_SELF => {
                first = None;
                last = None;
                no_dupls = true;
            }
            AXIS_CHILD => {
                last = None;
                elem_child = (test == TEST_NAME || test == TEST_ALL) && ty == TYPE_NODE;
                no_dupls = true;
            }
            AXIS_DESCENDANT | AXIS_DESCENDANT_OR_SELF | AXIS_FOLLOWING | AXIS_FOLLOWING_SIBLING => {
                last = None
            }
            _ => return 0,
        }
        if ctx_seq.is_empty() {
            self.push(XValue::NodeSet(ctx_seq));
            return 0;
        }
        let mut max_pos = 0;
        let mut pred_op = None;
        let mut has_predicate_range = false;
        let mut has_axis_range = false;
        if s.ch2.get() != -1 {
            let p = s.ch2.get();
            pred_op = Some(p);
            if self.is_positional(p, &mut max_pos) {
                let inner = self.step(p).ch1.get();
                if inner != -1 {
                    pred_op = Some(inner);
                    has_predicate_range = true;
                } else {
                    pred_op = None;
                    has_axis_range = true;
                }
            }
        }
        let break_on_first_hit = to_bool && pred_op.is_none();
        let old_node = self.node.clone();
        let mut out: Option<Vec<XNode>> = None;
        let mut total = 0i32;
        let mut idx = 0;
        let put = |out: &mut Option<Vec<XNode>>, seq: Vec<XNode>| {
            *out = Some(match out.take() {
                None => seq,
                Some(o) if no_dupls => {
                    let mut o = o;
                    o.extend(seq);
                    o
                }
                Some(o) => merge_and_clear(o, seq),
            });
        };
        'ctx: while idx < ctx_seq.len() && self.error == 0 {
            self.node = ctx_seq[idx].clone();
            idx += 1;
            self.following_steps = 0;
            let mut seq: Vec<XNode> = Vec::new();
            let mut pos = 0;
            let mut cur: Option<XNode> = None;
            enum End {
                Normal,
                AxisRange,
                FirstHit,
            }
            let mut end = End::Normal;
            loop {
                cur = self.next_on(axis, elem_child, cur.as_ref());
                let Some(c) = cur.clone() else { break };
                if let Some(f) = &first {
                    if f.same(&c) {
                        break;
                    }
                    if total % 256 == 0 && self.x.cmp_nodes(f, &c) >= 0 {
                        break;
                    }
                }
                if let Some(l) = &last {
                    if l.same(&c) {
                        break;
                    }
                    if total % 256 == 0 && self.x.cmp_nodes(&c, l) >= 0 {
                        break;
                    }
                }
                total += 1;
                let hit = self.test_node(&c, axis, test, ty, prefix, name, uri);
                if hit != Hit::No {
                    let add = |seq: &mut Vec<XNode>, ctxn: &XNode| match (&c, hit) {
                        (XNode::Ns(ns), Hit::Ns) => {
                            if let Some(e) = ctxn.id() {
                                add_ns(seq, e, ns);
                            }
                        }
                        _ => add_unique(seq, &c),
                    };
                    if has_axis_range {
                        pos += 1;
                        if pos == max_pos {
                            add(&mut seq, &self.node);
                            end = End::AxisRange;
                            break;
                        }
                    } else {
                        add(&mut seq, &self.node);
                        if break_on_first_hit {
                            end = End::FirstHit;
                            break;
                        }
                    }
                }
                if self.error != 0 {
                    break;
                }
            }
            match end {
                End::AxisRange => {
                    put(&mut out, seq);
                    if to_bool {
                        break 'ctx;
                    }
                    continue;
                }
                End::FirstHit => {
                    put(&mut out, seq);
                    break 'ctx;
                }
                End::Normal => {}
            }
            if self.error != 0 {
                break;
            }
            if let Some(p) = pred_op
                && !seq.is_empty()
            {
                if has_predicate_range {
                    self.eval_predicate(p, &mut seq, max_pos, max_pos);
                } else {
                    let n = seq.len() as i32;
                    self.eval_predicate(p, &mut seq, 1, n);
                }
                if self.error != 0 {
                    total = 0;
                    break;
                }
            }
            if !seq.is_empty() {
                put(&mut out, seq);
                if to_bool {
                    break;
                }
            }
        }
        self.push(XValue::NodeSet(out.unwrap_or_default()));
        self.node = old_node;
        self.tmp_ns.clear();
        total
    }

    /// `xmlXPathCompOpEvalPredicate`.
    fn eval_predicate(&mut self, op: i32, set: &mut Vec<XNode>, min_pos: i32, max_pos: i32) {
        let s = self.step(op);
        if s.ch1.get() != -1 {
            if self.step(s.ch1.get()).op != Op::Predicate {
                self.err(INVALID_OPERAND);
                return;
            }
            if self.depth >= MAX_RECURSION_DEPTH {
                self.err(RECURSION_LIMIT_EXCEEDED);
                return;
            }
            self.depth += 1;
            let n = set.len() as i32;
            self.eval_predicate(s.ch1.get(), set, 1, n);
            self.depth -= 1;
            if self.error != 0 {
                return;
            }
        }
        if s.ch2.get() != -1 {
            self.node_set_filter(set, s.ch2.get(), min_pos, max_pos);
        }
    }

    /// `xmlXPathNodeSetFilter`.
    fn node_set_filter(
        &mut self,
        set: &mut Vec<XNode>,
        filter_op: i32,
        min_pos: i32,
        max_pos: i32,
    ) {
        if set.is_empty() {
            return;
        }
        if (set.len() as i32) < min_pos {
            set.clear();
            return;
        }
        let (old_node, old_size, old_pos) = (self.node.clone(), self.size, self.pos);
        self.size = set.len() as i32;
        let mut kept = Vec::new();
        let mut pos = 1;
        for (i, node) in set.iter().enumerate() {
            self.node = node.clone();
            self.pos = i as i32 + 1;
            let res = self.eval_to_boolean(filter_op, true);
            if self.error != 0 {
                break;
            }
            if res < 0 {
                self.err(EXPR_ERROR);
                break;
            }
            if res != 0 && pos >= min_pos && pos <= max_pos {
                kept.push(node.clone());
            }
            if res != 0 {
                if pos == max_pos {
                    break;
                }
                pos += 1;
            }
        }
        *set = kept;
        self.node = old_node;
        self.size = old_size;
        self.pos = old_pos;
    }

    /// `xmlXPathEvaluatePredicateResult`.
    fn predicate_result(&self, v: &XValue) -> bool {
        match v {
            XValue::Bool(b) => *b,
            XValue::Number(n) => *n == self.pos as f64,
            XValue::NodeSet(s) | XValue::Tree(s) => !s.is_empty(),
            XValue::String(s) => !s.is_empty(),
        }
    }

    /// `xmlXPathCompOpEvalToBoolean`.
    fn eval_to_boolean(&mut self, op: i32, is_predicate: bool) -> i32 {
        let mut op = op;
        let res = loop {
            let s = self.step(op);
            match s.op {
                Op::Value => {
                    let v = s
                        .lit
                        .as_ref()
                        .map(XValue::from_lit)
                        .unwrap_or(XValue::Bool(false));
                    return if is_predicate {
                        self.predicate_result(&v) as i32
                    } else {
                        cast_to_bool(&v) as i32
                    };
                }
                Op::Sort => {
                    if s.ch1.get() != -1 {
                        op = s.ch1.get();
                        continue;
                    }
                    return 0;
                }
                Op::Collect => {
                    if s.ch1.get() == -1 {
                        return 0;
                    }
                    self.eval_op(s.ch1.get());
                    if self.error != 0 {
                        return -1;
                    }
                    self.collect_and_test(op, None, None, true);
                    if self.error != 0 {
                        return -1;
                    }
                    match self.pop() {
                        Some(v) => break v,
                        None => return -1,
                    }
                }
                _ => {
                    self.eval_op(op);
                    if self.error != 0 {
                        return -1;
                    }
                    match self.pop() {
                        Some(v) => break v,
                        None => return -1,
                    }
                }
            }
        };
        let b = match &res {
            XValue::Bool(b) => *b,
            v if is_predicate => self.predicate_result(v),
            v => cast_to_bool(v),
        };
        b as i32
    }

    fn enter(&mut self) -> bool {
        if self.error != 0 {
            return false;
        }
        if self.depth >= MAX_RECURSION_DEPTH || crate::stack::check().is_err() {
            self.err(RECURSION_LIMIT_EXCEEDED);
            return false;
        }
        self.depth += 1;
        true
    }

    /// Sorts the node set on top of the stack, if any.
    fn sort_top(&mut self) {
        let x = self.x;
        if let Some(XValue::NodeSet(s)) = self.stack.last_mut()
            && s.len() > 1
        {
            x.sort(s);
        }
    }

    /// The union of the two node sets on the stack (shared by the evaluation variants).
    fn union_top(&mut self) -> bool {
        let arg2 = self.pop();
        let arg1 = self.pop();
        match (arg1, arg2) {
            (Some(XValue::NodeSet(a)), Some(XValue::NodeSet(b))) => {
                let a = if b.is_empty() { a } else { merge(a, &b) };
                self.push(XValue::NodeSet(a));
                true
            }
            _ => {
                self.err(INVALID_TYPE);
                false
            }
        }
    }

    /// `xmlXPathCompOpEvalFirst`.
    fn eval_first(&mut self, op: i32, first: &mut Option<XNode>) -> i32 {
        if !self.enter() {
            return 0;
        }
        let s = self.step(op);
        let mut total = 0;
        match s.op {
            Op::Union => {
                total = self.eval_first(s.ch1.get(), first);
                if self.error != 0 {
                    return 0;
                }
                if let Some(XValue::NodeSet(set)) = self.stack.last_mut()
                    && !set.is_empty()
                {
                    if set.len() > 1 {
                        self.x.sort(set);
                    }
                    *first = Some(set[0].clone());
                }
                let cur = self.eval_first(s.ch2.get(), first);
                if self.error != 0 {
                    return 0;
                }
                if !self.union_top() {
                    return 0;
                }
                if total > cur {
                    swap_children(s);
                }
                total += cur;
            }
            Op::Root => self.push(XValue::NodeSet(vec![XNode::Node(self.root)])),
            Op::Node => {
                if s.ch1.get() != -1 {
                    total += self.eval_op(s.ch1.get());
                }
                if self.error != 0 {
                    return 0;
                }
                if s.ch2.get() != -1 {
                    total += self.eval_op(s.ch2.get());
                }
                if self.error != 0 {
                    return 0;
                }
                let v = self.node_set_of(&self.node);
                self.push(v);
            }
            Op::Collect => {
                if s.ch1.get() != -1 {
                    total = self.eval_op(s.ch1.get());
                    if self.error != 0 {
                        return 0;
                    }
                    total += self.collect_and_test(op, first.clone(), None, false);
                }
            }
            Op::Value => {
                let v = s
                    .lit
                    .as_ref()
                    .map(XValue::from_lit)
                    .unwrap_or(XValue::Bool(false));
                self.push(v);
            }
            Op::Sort => {
                if s.ch1.get() != -1 {
                    total += self.eval_first(s.ch1.get(), first);
                }
                if self.error != 0 {
                    return 0;
                }
                self.sort_top();
            }
            Op::Filter => total += self.eval_filter_first(op, first),
            _ => total += self.eval_op(op),
        }
        self.depth -= 1;
        total
    }

    /// `xmlXPathCompOpEvalLast`.
    fn eval_last(&mut self, op: i32, last: &mut Option<XNode>) -> i32 {
        if !self.enter() {
            return 0;
        }
        let s = self.step(op);
        let mut total = 0;
        match s.op {
            Op::Union => {
                total = self.eval_last(s.ch1.get(), last);
                if self.error != 0 {
                    return 0;
                }
                if let Some(XValue::NodeSet(set)) = self.stack.last_mut()
                    && !set.is_empty()
                {
                    if set.len() > 1 {
                        self.x.sort(set);
                    }
                    *last = Some(set[set.len() - 1].clone());
                }
                let cur = self.eval_last(s.ch2.get(), last);
                if self.error != 0 {
                    return 0;
                }
                if !self.union_top() {
                    return 0;
                }
                if total > cur {
                    swap_children(s);
                }
                total += cur;
            }
            Op::Root => self.push(XValue::NodeSet(vec![XNode::Node(self.root)])),
            Op::Node => {
                if s.ch1.get() != -1 {
                    total += self.eval_op(s.ch1.get());
                }
                if self.error != 0 {
                    return 0;
                }
                if s.ch2.get() != -1 {
                    total += self.eval_op(s.ch2.get());
                }
                if self.error != 0 {
                    return 0;
                }
                let v = self.node_set_of(&self.node);
                self.push(v);
            }
            Op::Collect => {
                if s.ch1.get() != -1 {
                    total += self.eval_op(s.ch1.get());
                    if self.error != 0 {
                        return 0;
                    }
                    total += self.collect_and_test(op, None, last.clone(), false);
                }
            }
            Op::Value => {
                let v = s
                    .lit
                    .as_ref()
                    .map(XValue::from_lit)
                    .unwrap_or(XValue::Bool(false));
                self.push(v);
            }
            Op::Sort => {
                if s.ch1.get() != -1 {
                    total += self.eval_last(s.ch1.get(), last);
                }
                if self.error != 0 {
                    return 0;
                }
                self.sort_top();
            }
            _ => total += self.eval_op(op),
        }
        self.depth -= 1;
        total
    }

    /// Whether `op` is `[last()]` over a sorted set (the `()[last()]` shortcut).
    fn is_last_filter(&self, s: &Step) -> bool {
        if s.ch1.get() == -1 || s.ch2.get() == -1 {
            return false;
        }
        let (c1, c2) = (self.step(s.ch1.get()), self.step(s.ch2.get()));
        if c1.op != Op::Sort || c2.op != Op::Sort {
            return false;
        }
        let f = c2.ch1.get();
        if f == -1 {
            return false;
        }
        let f = self.step(f);
        f.op == Op::Function
            && f.s5.is_none()
            && f.value.get() == 0
            && f.s4.as_deref() == Some("last")
    }

    fn keep_last_on_top(&mut self) {
        if let Some(XValue::NodeSet(set)) = self.stack.last_mut()
            && set.len() > 1
        {
            let l = set.pop().unwrap();
            set.clear();
            set.push(l);
        }
    }

    /// `xmlXPathCompOpEvalFilterFirst`.
    fn eval_filter_first(&mut self, op: i32, first: &mut Option<XNode>) -> i32 {
        if self.error != 0 {
            return 0;
        }
        let s = self.step(op);
        let mut total = 0;
        if self.is_last_filter(s) {
            let mut last = None;
            total += self.eval_last(s.ch1.get(), &mut last);
            if self.error != 0 {
                return 0;
            }
            if let Some(XValue::NodeSet(set)) = self.stack.last()
                && set.len() > 1
            {
                self.keep_last_on_top();
                if let Some(XValue::NodeSet(set)) = self.stack.last() {
                    *first = set.first().cloned();
                }
            }
            return total;
        }
        if s.ch1.get() != -1 {
            total += self.eval_op(s.ch1.get());
        }
        if self.error != 0 {
            return 0;
        }
        if s.ch2.get() == -1 || self.stack.is_empty() {
            return total;
        }
        if !self.top_is_strict_nodeset() {
            self.err(INVALID_TYPE);
            return 0;
        }
        let Some(XValue::NodeSet(mut set)) = self.pop() else {
            return total;
        };
        self.node_set_filter(&mut set, s.ch2.get(), 1, 1);
        if let Some(f) = set.first() {
            *first = Some(f.clone());
        }
        self.push(XValue::NodeSet(set));
        total
    }

    /// `xmlXPathCompOpEval`.
    fn eval_op(&mut self, op: i32) -> i32 {
        if !self.enter() {
            return 0;
        }
        let s = self.step(op);
        let mut total = 0;
        match s.op {
            Op::And | Op::Or => {
                let is_and = s.op == Op::And;
                total += self.eval_op(s.ch1.get());
                if self.error != 0 {
                    return 0;
                }
                self.f_boolean(1);
                match self.stack.last() {
                    Some(XValue::Bool(b)) if *b != is_and => {}
                    Some(XValue::Bool(_)) => {
                        let arg2 = self.pop();
                        total += self.eval_op(s.ch2.get());
                        if self.error == 0 {
                            self.f_boolean(1);
                            let a2 = matches!(arg2, Some(XValue::Bool(true)));
                            if let Some(XValue::Bool(b)) = self.stack.last_mut() {
                                if is_and {
                                    *b &= a2;
                                } else {
                                    *b |= a2;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Op::Equal => {
                total += self.eval_op(s.ch1.get());
                if self.error != 0 {
                    return 0;
                }
                total += self.eval_op(s.ch2.get());
                if self.error != 0 {
                    return 0;
                }
                let eq = self.equal_values(s.value.get() == 0);
                self.push(XValue::Bool(eq));
            }
            Op::Cmp => {
                total += self.eval_op(s.ch1.get());
                if self.error != 0 {
                    return 0;
                }
                total += self.eval_op(s.ch2.get());
                if self.error != 0 {
                    return 0;
                }
                let r = self.compare_values(s.value.get() != 0, s.value2 != 0);
                self.push(XValue::Bool(r));
            }
            Op::Plus => {
                total += self.eval_op(s.ch1.get());
                if self.error != 0 {
                    return 0;
                }
                if s.ch2.get() != -1 {
                    total += self.eval_op(s.ch2.get());
                }
                if self.error != 0 {
                    return 0;
                }
                match s.value.get() {
                    0 => self.arith(|a, b| a - b),
                    1 => self.arith(|a, b| a + b),
                    2 => {
                        self.cast_top_to_number();
                        if self.check_top(|v| matches!(v, XValue::Number(_)))
                            && let Some(n) = self.top_number()
                        {
                            *n = -*n;
                        }
                    }
                    3 => {
                        self.cast_top_to_number();
                        if !self.check_top(|v| matches!(v, XValue::Number(_))) {
                            return 0;
                        }
                    }
                    _ => {}
                }
            }
            Op::Mult => {
                total += self.eval_op(s.ch1.get());
                if self.error != 0 {
                    return 0;
                }
                total += self.eval_op(s.ch2.get());
                if self.error != 0 {
                    return 0;
                }
                match s.value.get() {
                    0 => self.arith(|a, b| a * b),
                    1 => self.arith(|a, b| a / b),
                    2 => self.arith(|a, b| if b == 0.0 { f64::NAN } else { a % b }),
                    _ => {}
                }
            }
            Op::Union => {
                total += self.eval_op(s.ch1.get());
                if self.error != 0 {
                    return 0;
                }
                total += self.eval_op(s.ch2.get());
                if self.error != 0 {
                    return 0;
                }
                if !self.union_top() {
                    return 0;
                }
            }
            Op::Root => self.push(XValue::NodeSet(vec![XNode::Node(self.root)])),
            Op::Node => {
                if s.ch1.get() != -1 {
                    total += self.eval_op(s.ch1.get());
                }
                if self.error != 0 {
                    return 0;
                }
                if s.ch2.get() != -1 {
                    total += self.eval_op(s.ch2.get());
                }
                if self.error != 0 {
                    return 0;
                }
                let v = self.node_set_of(&self.node);
                self.push(v);
            }
            Op::Collect => {
                if s.ch1.get() != -1 {
                    total += self.eval_op(s.ch1.get());
                    if self.error != 0 {
                        return 0;
                    }
                    total += self.collect_and_test(op, None, None, false);
                }
            }
            Op::Value => {
                let v = s
                    .lit
                    .as_ref()
                    .map(XValue::from_lit)
                    .unwrap_or(XValue::Bool(false));
                self.push(v);
            }
            Op::Variable => {
                if s.ch1.get() != -1 {
                    self.eval_op(s.ch1.get());
                }
                let uri = match s.s5.as_deref() {
                    None => None,
                    Some(p) => match self.ns.lookup(p) {
                        None => {
                            self.err(UNDEF_PREFIX_ERROR);
                            return 0;
                        }
                        Some(u) => Some(u),
                    },
                };
                let name = s.s4.as_deref().unwrap_or("");
                match self.host.and_then(|h| h.variable(name, uri)) {
                    Some(v) => self.push(v),
                    None => {
                        self.err(UNDEF_VARIABLE_ERROR);
                        return 0;
                    }
                }
            }
            Op::Function => {
                let frame = self.stack.len();
                let nargs = s.value.get();
                if s.ch1.get() != -1 {
                    total += self.eval_op(s.ch1.get());
                    if self.error != 0 {
                        self.depth -= 1;
                        return total;
                    }
                }
                if (self.stack.len() as i64) < frame as i64 + nargs as i64 {
                    self.err(INVALID_OPERAND);
                    return 0;
                }
                let name = s.s4.as_deref().unwrap_or("");
                let (f, uri) = match s.s5.as_deref() {
                    None => (builtin(name), None),
                    Some(p) => match self.ns.lookup(p) {
                        None => {
                            self.err(UNDEF_PREFIX_ERROR);
                            return 0;
                        }
                        Some(uri) if uri == XQUERY_FUNCTIONS_NS && name == "escape-uri" => {
                            (Some(Func::EscapeUri), None)
                        }
                        Some(uri) => (None, Some(uri)),
                    },
                };
                match f {
                    Some(f) => self.call(f, nargs),
                    None => match self.host.filter(|h| h.has_function(name, uri)) {
                        Some(h) => {
                            let at = self.stack.len().saturating_sub(nargs.max(0) as usize);
                            let args = self.stack.split_off(at);
                            let call = HostCall { x: self.x, ns: self.ns, node: &self.node, pos: self.pos, size: self.size };
                            match h.call(&call, name, uri, args) {
                                Ok(v) => self.push(v),
                                Err(code) => {
                                    self.err(code);
                                    return 0;
                                }
                            }
                        }
                        None => {
                            self.err(UNKNOWN_FUNC_ERROR);
                            return 0;
                        }
                    },
                }
                if self.error == 0 && self.stack.len() != frame + 1 {
                    self.err(STACK_ERROR);
                    return 0;
                }
            }
            Op::Arg => {
                if s.ch1.get() != -1 {
                    total += self.eval_op(s.ch1.get());
                    if self.error != 0 {
                        return 0;
                    }
                }
                if s.ch2.get() != -1 {
                    total += self.eval_op(s.ch2.get());
                    if self.error != 0 {
                        return 0;
                    }
                }
            }
            Op::Predicate | Op::Filter => {
                let (c1, c2) = (s.ch1.get(), s.ch2.get());
                if c1 != -1
                    && c2 != -1
                    && matches!(self.step(c1).op, Op::Sort | Op::Filter)
                    && self.step(c2).op == Op::Value
                    && matches!(self.step(c2).lit, Some(Lit::Number(f)) if f == 1.0)
                {
                    let mut first = None;
                    total += self.eval_first(c1, &mut first);
                    if self.error != 0 {
                        return 0;
                    }
                    if let Some(XValue::NodeSet(set)) = self.stack.last_mut()
                        && set.len() > 1
                    {
                        set.truncate(1);
                    }
                } else if self.is_last_filter(s) {
                    let mut last = None;
                    total += self.eval_last(c1, &mut last);
                    if self.error != 0 {
                        return 0;
                    }
                    self.keep_last_on_top();
                } else {
                    if c1 != -1 {
                        total += self.eval_op(c1);
                    }
                    if self.error != 0 {
                        return 0;
                    }
                    if c2 != -1 && !self.stack.is_empty() {
                        if !self.top_is_strict_nodeset() {
                            self.err(INVALID_TYPE);
                            return 0;
                        }
                        if let Some(XValue::NodeSet(mut set)) = self.pop() {
                            let n = set.len() as i32;
                            self.node_set_filter(&mut set, c2, 1, n);
                            self.push(XValue::NodeSet(set));
                        }
                    }
                }
            }
            Op::Sort => {
                if s.ch1.get() != -1 {
                    total += self.eval_op(s.ch1.get());
                }
                if self.error != 0 {
                    return 0;
                }
                self.sort_top();
            }
        }
        self.depth -= 1;
        total
    }

    /// `xmlXPathAddValues` and the other arithmetic operators.
    fn arith(&mut self, f: fn(f64, f64) -> f64) {
        let Some(mut arg) = self.pop() else {
            self.err(INVALID_OPERAND);
            return;
        };
        let val = cast_to_number(self.x, &mut arg);
        self.cast_top_to_number();
        if !self.check_top(|v| matches!(v, XValue::Number(_))) {
            return;
        }
        if let Some(n) = self.top_number() {
            *n = f(*n, val);
        }
    }

    // --- comparisons

    /// `xmlXPathEqualValues` / `xmlXPathNotEqualValues`.
    fn equal_values(&mut self, neq: bool) -> bool {
        let arg2 = self.pop();
        let arg1 = self.pop();
        let (Some(mut a1), Some(mut a2)) = (arg1, arg2) else {
            self.err(INVALID_OPERAND);
            return false;
        };
        let is_set = |v: &XValue| v.nodes().is_some();
        if is_set(&a1) || is_set(&a2) {
            if !is_set(&a1) {
                std::mem::swap(&mut a1, &mut a2);
            }
            let Some(ns1) = a1.nodes() else {
                return false;
            };
            return match &a2 {
                XValue::NodeSet(ns2) | XValue::Tree(ns2) => self.equal_node_sets(ns1, ns2, neq),
                XValue::Bool(b) => (!ns1.is_empty() == *b) != neq,
                XValue::Number(f) => self.equal_node_set_float(ns1, *f, neq),
                XValue::String(s) => self.equal_node_set_string(ns1, s, neq),
            };
        }
        let r = self.equal_values_common(&a1, &a2);
        if neq { !r } else { r }
    }

    /// `xmlXPathEqualValuesCommon`.
    fn equal_values_common(&self, a1: &XValue, a2: &XValue) -> bool {
        match (a1, a2) {
            (XValue::Bool(a), XValue::Bool(b)) => a == b,
            (XValue::Bool(a), XValue::Number(n)) => *a == number_to_bool(*n),
            (XValue::Bool(a), XValue::String(s)) => *a == !s.is_empty(),
            (XValue::Number(n), XValue::Bool(b)) => *b == number_to_bool(*n),
            (XValue::Number(a), XValue::String(s)) => equal_numbers(*a, string_to_number(s)),
            (XValue::Number(a), XValue::Number(b)) => equal_numbers(*a, *b),
            (XValue::String(s), XValue::Bool(b)) => *b == !s.is_empty(),
            (XValue::String(a), XValue::String(b)) => a == b,
            (XValue::String(s), XValue::Number(b)) => equal_numbers(string_to_number(s), *b),
            _ => false,
        }
    }

    /// `xmlXPathEqualNodeSets`.
    fn equal_node_sets(&self, ns1: &[XNode], ns2: &[XNode], neq: bool) -> bool {
        if ns1.is_empty() || ns2.is_empty() {
            return false;
        }
        if !neq && ns1.iter().any(|a| ns2.iter().any(|b| a.same(b))) {
            return true;
        }
        let x = self.x;
        let hashs2: Vec<u32> = ns2.iter().map(|n| x.node_hash(n)).collect();
        let mut values2: Vec<Option<String>> = vec![None; ns2.len()];
        for n1 in ns1 {
            let h1 = x.node_hash(n1);
            let mut v1: Option<String> = None;
            for (j, n2) in ns2.iter().enumerate() {
                if h1 != hashs2[j] {
                    if neq {
                        return true;
                    }
                } else {
                    let a = v1.get_or_insert_with(|| x.node_string(n1));
                    let b = values2[j].get_or_insert_with(|| x.node_string(n2));
                    if (a == b) != neq {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// `xmlXPathEqualNodeSetString`.
    fn equal_node_set_string(&self, ns: &[XNode], s: &str, neq: bool) -> bool {
        let hash = string_hash(s);
        for n in ns {
            if self.x.node_hash(n) == hash {
                let v = self.x.node_string(n);
                if v == s {
                    if neq {
                        continue;
                    }
                    return true;
                } else if neq {
                    return true;
                }
            } else if neq {
                return true;
            }
        }
        false
    }

    /// `xmlXPathEqualNodeSetFloat`.
    fn equal_node_set_float(&self, ns: &[XNode], f: f64, neq: bool) -> bool {
        let mut ret = false;
        for n in ns {
            let v = string_to_number(&self.x.node_string(n));
            if !v.is_nan() {
                if (!neq && v == f) || (neq && v != f) {
                    return true;
                }
            } else if neq {
                ret = true;
            }
        }
        ret
    }

    /// `xmlXPathCompareValues`.
    fn compare_values(&mut self, inf: bool, strict: bool) -> bool {
        let arg2 = self.pop();
        let arg1 = self.pop();
        let (Some(mut a1), Some(mut a2)) = (arg1, arg2) else {
            self.err(INVALID_OPERAND);
            return false;
        };
        let x = self.x;
        let is_set = |v: &XValue| v.nodes().is_some();
        if is_set(&a1) || is_set(&a2) {
            if is_set(&a1) && is_set(&a2) {
                return self.compare_node_sets(inf, strict, &a1, &a2);
            }
            return if is_set(&a1) {
                self.compare_node_set_value(inf, strict, &a1, &a2)
            } else {
                self.compare_node_set_value(!inf, strict, &a2, &a1)
            };
        }
        let n1 = cast_to_number(x, &mut a1);
        let n2 = cast_to_number(x, &mut a2);
        compare_numbers(inf, strict, n1, n2)
    }

    /// `xmlXPathCompareNodeSetValue`.
    fn compare_node_set_value(&self, inf: bool, strict: bool, arg: &XValue, val: &XValue) -> bool {
        let Some(ns) = arg.nodes() else {
            return false;
        };
        let x = self.x;
        match val {
            XValue::Number(f) => ns
                .iter()
                .any(|n| compare_numbers(inf, strict, string_to_number(&x.node_string(n)), *f)),
            XValue::String(s) => {
                let f = string_to_number(s);
                ns.iter()
                    .any(|n| compare_numbers(inf, strict, string_to_number(&x.node_string(n)), f))
            }
            XValue::NodeSet(_) | XValue::Tree(_) => self.compare_node_sets(inf, strict, arg, val),
            XValue::Bool(b) => compare_numbers(
                inf,
                strict,
                bool_to_number(!ns.is_empty()),
                bool_to_number(*b),
            ),
        }
    }

    /// `xmlXPathCompareNodeSets`.
    fn compare_node_sets(&self, inf: bool, strict: bool, a1: &XValue, a2: &XValue) -> bool {
        let (Some(ns1), Some(ns2)) = (a1.nodes(), a2.nodes()) else {
            return false;
        };
        if ns1.is_empty() || ns2.is_empty() {
            return false;
        }
        let x = self.x;
        let values2: Vec<f64> = ns2
            .iter()
            .map(|n| string_to_number(&x.node_string(n)))
            .collect();
        for n1 in ns1 {
            let v1 = string_to_number(&x.node_string(n1));
            if v1.is_nan() {
                continue;
            }
            for &v2 in &values2 {
                if v2.is_nan() {
                    continue;
                }
                let r = match (inf, strict) {
                    (true, true) => v1 < v2,
                    (true, false) => v1 <= v2,
                    (false, true) => v1 > v2,
                    (false, false) => v1 >= v2,
                };
                if r {
                    return true;
                }
            }
        }
        false
    }
}

/// `xmlXPathCompSwap`.
fn swap_children(s: &Step) {
    let t = s.ch1.get();
    s.ch1.set(s.ch2.get());
    s.ch2.set(t);
}

// ---------------------------------------------------------------------------------------------
// Core function library (xmlXPathRegisterAllFunctions)

#[derive(Clone, Copy, Debug)]
enum Func {
    Boolean,
    Ceiling,
    Count,
    Concat,
    Contains,
    Id,
    False,
    Floor,
    Last,
    Lang,
    LocalName,
    Not,
    Name,
    NamespaceUri,
    NormalizeSpace,
    Number,
    Position,
    Round,
    String,
    StringLength,
    StartsWith,
    Substring,
    SubstringBefore,
    SubstringAfter,
    Sum,
    True,
    Translate,
    EscapeUri,
}

fn builtin(name: &str) -> Option<Func> {
    Some(match name {
        "boolean" => Func::Boolean,
        "ceiling" => Func::Ceiling,
        "count" => Func::Count,
        "concat" => Func::Concat,
        "contains" => Func::Contains,
        "id" => Func::Id,
        "false" => Func::False,
        "floor" => Func::Floor,
        "last" => Func::Last,
        "lang" => Func::Lang,
        "local-name" => Func::LocalName,
        "not" => Func::Not,
        "name" => Func::Name,
        "namespace-uri" => Func::NamespaceUri,
        "normalize-space" => Func::NormalizeSpace,
        "number" => Func::Number,
        "position" => Func::Position,
        "round" => Func::Round,
        "string" => Func::String,
        "string-length" => Func::StringLength,
        "starts-with" => Func::StartsWith,
        "substring" => Func::Substring,
        "substring-before" => Func::SubstringBefore,
        "substring-after" => Func::SubstringAfter,
        "sum" => Func::Sum,
        "true" => Func::True,
        "translate" => Func::Translate,
        _ => return None,
    })
}

fn is_string(v: &XValue) -> bool {
    matches!(v, XValue::String(_))
}

fn is_number(v: &XValue) -> bool {
    matches!(v, XValue::Number(_))
}

fn is_bool(v: &XValue) -> bool {
    matches!(v, XValue::Bool(_))
}

impl Ctx<'_> {
    fn call(&mut self, f: Func, nargs: i32) {
        match f {
            Func::Boolean => self.f_boolean(nargs),
            Func::Ceiling => self.f_round_like(nargs, f64::ceil),
            Func::Count => self.f_count(nargs),
            Func::Concat => self.f_concat(nargs),
            Func::Contains => self.f_contains(nargs, false),
            Func::Id => self.f_id(nargs),
            Func::False => {
                if self.check_arity(nargs, 0) {
                    self.push(XValue::Bool(false));
                }
            }
            Func::Floor => self.f_round_like(nargs, f64::floor),
            Func::Last => {
                if self.check_arity(nargs, 0) {
                    if self.size >= 0 {
                        self.push(XValue::Number(self.size as f64));
                    } else {
                        self.err(INVALID_CTXT_SIZE);
                    }
                }
            }
            Func::Lang => self.f_lang(nargs),
            Func::LocalName => self.f_local_name(nargs),
            Func::Not => {
                if !self.check_arity(nargs, 1) {
                    return;
                }
                self.cast_top_to_boolean();
                if !self.check_top(is_bool) {
                    return;
                }
                if let Some(XValue::Bool(b)) = self.stack.last_mut() {
                    *b = !*b;
                }
            }
            Func::Name => self.f_name(nargs),
            Func::NamespaceUri => self.f_namespace_uri(nargs),
            Func::NormalizeSpace => self.f_normalize_space(nargs),
            Func::Number => self.f_number(nargs),
            Func::Position => {
                if self.check_arity(nargs, 0) {
                    if self.pos >= 0 {
                        self.push(XValue::Number(self.pos as f64));
                    } else {
                        self.err(INVALID_CTXT_POSITION);
                    }
                }
            }
            Func::Round => self.f_round_like(nargs, |f| {
                if (-0.5..0.5).contains(&f) {
                    f * 0.0
                } else {
                    let r = f.floor();
                    if f - r >= 0.5 { r + 1.0 } else { r }
                }
            }),
            Func::String => self.f_string(nargs),
            Func::StringLength => self.f_string_length(nargs),
            Func::StartsWith => self.f_contains(nargs, true),
            Func::Substring => self.f_substring(nargs),
            Func::SubstringBefore => self.f_substring_around(nargs, true),
            Func::SubstringAfter => self.f_substring_around(nargs, false),
            Func::Sum => self.f_sum(nargs),
            Func::True => {
                if self.check_arity(nargs, 0) {
                    self.push(XValue::Bool(true));
                }
            }
            Func::Translate => self.f_translate(nargs),
            Func::EscapeUri => self.f_escape_uri(nargs),
        }
    }

    fn f_boolean(&mut self, nargs: i32) {
        if !self.check_arity(nargs, 1) {
            return;
        }
        let Some(cur) = self.pop() else {
            self.err(INVALID_OPERAND);
            return;
        };
        let b = match cur {
            XValue::Bool(b) => b,
            ref v => cast_to_bool(v),
        };
        self.push(XValue::Bool(b));
    }

    fn f_number(&mut self, nargs: i32) {
        if nargs == 0 {
            let s = self.x.node_string(&self.node);
            self.push(XValue::Number(string_to_number(&s)));
            return;
        }
        if !self.check_arity(nargs, 1) {
            return;
        }
        let Some(mut cur) = self.pop() else { return };
        if !is_number(&cur) {
            cur = XValue::Number(cast_to_number(self.x, &mut cur));
        }
        self.push(cur);
    }

    fn f_string(&mut self, nargs: i32) {
        if nargs == 0 {
            let s = self.x.node_string(&self.node);
            self.push(XValue::String(s));
            return;
        }
        if !self.check_arity(nargs, 1) {
            return;
        }
        let Some(mut cur) = self.pop() else {
            self.err(INVALID_OPERAND);
            return;
        };
        if !is_string(&cur) {
            cur = XValue::String(cast_to_string(self.x, &mut cur));
        }
        self.push(cur);
    }

    fn f_round_like(&mut self, nargs: i32, f: fn(f64) -> f64) {
        if !self.check_arity(nargs, 1) {
            return;
        }
        self.cast_top_to_number();
        if !self.check_top(is_number) {
            return;
        }
        if let Some(n) = self.top_number() {
            *n = f(*n);
        }
    }

    fn f_count(&mut self, nargs: i32) {
        if !self.check_arity(nargs, 1) {
            return;
        }
        if !self.top_is_nodeset() {
            self.err(INVALID_TYPE);
            return;
        }
        if let Some(s) = self.pop_nodes() {
            self.push(XValue::Number(s.len() as f64));
        }
    }

    fn f_sum(&mut self, nargs: i32) {
        if !self.check_arity(nargs, 1) {
            return;
        }
        if !self.top_is_nodeset() {
            self.err(INVALID_TYPE);
            return;
        }
        if let Some(s) = self.pop_nodes() {
            let mut res = 0.0;
            for n in &s {
                res += string_to_number(&self.x.node_string(n));
            }
            self.push(XValue::Number(res));
        }
    }

    /// The node-set argument of local-name(), namespace-uri() and name(), defaulting to the
    /// context node.
    fn node_arg(&mut self, nargs: i32) -> Option<Vec<XNode>> {
        let mut nargs = nargs;
        if nargs == 0 {
            let v = self.node_set_of(&self.node);
            self.push(v);
            nargs = 1;
        }
        if !self.check_arity(nargs, 1) {
            return None;
        }
        if !self.top_is_nodeset() {
            self.err(INVALID_TYPE);
            return None;
        }
        self.pop_nodes()
    }

    fn local_name_of(&self, n: &XNode) -> String {
        match n {
            XNode::Ns(ns) => ns.prefix.clone().unwrap_or_default(),
            XNode::Node(id) => match self.x.kind(*id) {
                Kind::Element | Kind::Attribute | Kind::Pi => self.x.doc.nodes[*id].name.to_string(),
                _ => String::new(),
            },
        }
    }

    fn f_local_name(&mut self, nargs: i32) {
        let Some(s) = self.node_arg(nargs) else {
            return;
        };
        let v = s.first().map(|n| self.local_name_of(n)).unwrap_or_default();
        self.push(XValue::String(v));
    }

    fn f_namespace_uri(&mut self, nargs: i32) {
        let Some(s) = self.node_arg(nargs) else {
            return;
        };
        let v = match s.first() {
            Some(XNode::Node(id))
                if matches!(self.x.kind(*id), Kind::Element | Kind::Attribute) =>
            {
                self.ns_href(*id).unwrap_or("").to_string()
            }
            _ => String::new(),
        };
        self.push(XValue::String(v));
    }

    fn f_name(&mut self, nargs: i32) {
        let Some(s) = self.node_arg(nargs) else {
            return;
        };
        let v = match s.first() {
            None => String::new(),
            Some(XNode::Node(id))
                if matches!(self.x.kind(*id), Kind::Element | Kind::Attribute) =>
            {
                let n = &self.x.doc.nodes[*id];
                match n.ns.as_ref().and_then(|ns| ns.prefix.as_deref()) {
                    Some(p) => format!("{p}:{}", n.name),
                    None => n.name.to_string(),
                }
            }
            Some(n) => self.local_name_of(n),
        };
        self.push(XValue::String(v));
    }

    fn f_string_length(&mut self, nargs: i32) {
        if nargs == 0 {
            let s = self.x.node_string(&self.node);
            self.push(XValue::Number(s.chars().count() as f64));
            return;
        }
        if !self.check_arity(nargs, 1) {
            return;
        }
        self.cast_top_to_string();
        if !self.check_top(is_string) {
            return;
        }
        if let Some(XValue::String(s)) = self.pop() {
            self.push(XValue::Number(s.chars().count() as f64));
        }
    }

    fn f_concat(&mut self, nargs: i32) {
        if nargs < 2 {
            self.check_arity(nargs, 2);
            return;
        }
        self.cast_top_to_string();
        let mut cur = match self.pop() {
            Some(XValue::String(s)) => s,
            _ => return,
        };
        for _ in 1..nargs {
            self.cast_top_to_string();
            match self.pop() {
                Some(XValue::String(mut s)) => {
                    s.push_str(&cur);
                    cur = s;
                }
                _ => {
                    self.err(INVALID_TYPE);
                    return;
                }
            }
        }
        self.push(XValue::String(cur));
    }

    /// contains() and starts-with().
    fn f_contains(&mut self, nargs: i32, starts: bool) {
        if !self.check_arity(nargs, 2) {
            return;
        }
        self.cast_top_to_string();
        if !self.check_top(is_string) {
            return;
        }
        let Some(XValue::String(needle)) = self.pop() else {
            return;
        };
        self.cast_top_to_string();
        let hay = match self.pop() {
            Some(XValue::String(s)) => s,
            _ => {
                self.err(INVALID_TYPE);
                return;
            }
        };
        let r = if starts {
            hay.as_bytes().starts_with(needle.as_bytes())
        } else {
            hay.contains(needle.as_str())
        };
        self.push(XValue::Bool(r));
    }

    fn f_substring(&mut self, nargs: i32) {
        if nargs < 2 {
            self.check_arity(nargs, 2);
            return;
        }
        if nargs > 3 {
            self.check_arity(nargs, 3);
            return;
        }
        let mut le = 0.0;
        if nargs == 3 {
            self.cast_top_to_number();
            if !self.check_top(is_number) {
                return;
            }
            if let Some(XValue::Number(n)) = self.pop() {
                le = n;
            }
        }
        self.cast_top_to_number();
        if !self.check_top(is_number) {
            return;
        }
        let Some(XValue::Number(start)) = self.pop() else {
            return;
        };
        self.cast_top_to_string();
        if !self.check_top(is_string) {
            return;
        }
        let Some(XValue::String(s)) = self.pop() else {
            return;
        };
        let mut i: i32 = 1;
        let mut j: i32 = i32::MAX;
        // NaN too.
        if start.partial_cmp(&(i32::MAX as f64)) != Some(std::cmp::Ordering::Less) {
            i = i32::MAX;
        } else if start >= 1.0 {
            i = start as i32;
            if start - start.floor() >= 0.5 {
                i += 1;
            }
        }
        if nargs == 3 {
            let mut rin = start.floor();
            if start - rin >= 0.5 {
                rin += 1.0;
            }
            let mut rle = le.floor();
            if le - rle >= 0.5 {
                rle += 1.0;
            }
            let end = rin + rle;
            if end
                .partial_cmp(&1.0)
                .is_none_or(|o| o == std::cmp::Ordering::Less)
            {
                j = 1;
            } else if end < i32::MAX as f64 {
                j = end as i32;
            }
        }
        i -= 1;
        j -= 1;
        let len = s.chars().count() as i64;
        let r = if i < j && (i as i64) < len {
            s.chars().skip(i as usize).take((j - i) as usize).collect()
        } else {
            String::new()
        };
        self.push(XValue::String(r));
    }

    /// substring-before() and substring-after().
    fn f_substring_around(&mut self, nargs: i32, before: bool) {
        if !self.check_arity(nargs, 2) {
            return;
        }
        self.cast_top_to_string();
        let find = self.pop();
        self.cast_top_to_string();
        let s = self.pop();
        if self.error != 0 {
            return;
        }
        let (Some(XValue::String(s)), Some(XValue::String(find))) = (s, find) else {
            return;
        };
        let r = match s.find(find.as_str()) {
            None => String::new(),
            Some(p) if before => s[..p].to_string(),
            Some(p) => s[p + find.len()..].to_string(),
        };
        self.push(XValue::String(r));
    }

    fn f_normalize_space(&mut self, nargs: i32) {
        let mut nargs = nargs;
        if nargs == 0 {
            let s = self.x.node_string(&self.node);
            self.push(XValue::String(s));
            nargs = 1;
        }
        if !self.check_arity(nargs, 1) {
            return;
        }
        self.cast_top_to_string();
        if !self.check_top(is_string) {
            return;
        }
        if let Some(XValue::String(s)) = self.stack.last_mut() {
            let mut out = String::with_capacity(s.len());
            let mut blank = false;
            for c in s
                .trim_start_matches(|c: char| c.is_ascii() && is_blank(c as u8))
                .chars()
            {
                if c.is_ascii() && is_blank(c as u8) {
                    blank = true;
                } else {
                    if blank {
                        out.push(' ');
                        blank = false;
                    }
                    out.push(c);
                }
            }
            *s = out;
        }
    }

    fn f_translate(&mut self, nargs: i32) {
        if !self.check_arity(nargs, 3) {
            return;
        }
        self.cast_top_to_string();
        let to = self.pop();
        self.cast_top_to_string();
        let from = self.pop();
        self.cast_top_to_string();
        let s = self.pop();
        if self.error != 0 {
            return;
        }
        let (Some(XValue::String(s)), Some(XValue::String(from)), Some(XValue::String(to))) =
            (s, from, to)
        else {
            return;
        };
        let to: Vec<char> = to.chars().collect();
        let mut out = String::new();
        for c in s.chars() {
            match from.chars().position(|f| f == c) {
                Some(off) => {
                    if let Some(t) = to.get(off) {
                        out.push(*t);
                    }
                }
                None => out.push(c),
            }
        }
        self.push(XValue::String(out));
    }

    fn f_lang(&mut self, nargs: i32) {
        if !self.check_arity(nargs, 1) {
            return;
        }
        self.cast_top_to_string();
        if !self.check_top(is_string) {
            return;
        }
        let Some(XValue::String(lang)) = self.pop() else {
            return;
        };
        let x = self.x;
        let mut the_lang = None;
        let mut cur = match &self.node {
            XNode::Node(id) => Some(*id),
            XNode::Ns(_) => None,
        };
        while let Some(c) = cur {
            if x.kind(c) == Kind::Element
                && let Some(a) = x.doc.attrs(c).find(|&a| {
                    let an = &x.doc.nodes[a];
                    an.name == "lang"
                        && an.ns.as_ref().and_then(|n| n.href.as_deref()) == Some(XML_NS)
                })
            {
                the_lang = Some(x.node_content(a));
                break;
            }
            cur = x.parent(c);
        }
        let mut ret = false;
        if let Some(t) = the_lang {
            let (l, t) = (lang.as_bytes(), t.as_bytes());
            let mut i = 0;
            let mut equal = true;
            while i < l.len() {
                if !l[i].eq_ignore_ascii_case(&t.get(i).copied().unwrap_or(0)) {
                    equal = false;
                    break;
                }
                i += 1;
            }
            if equal && (i == t.len() || t[i] == b'-') {
                ret = true;
            }
        }
        self.push(XValue::Bool(ret));
    }

    /// `xmlXPathGetElementsByIds`.
    fn elements_by_ids(&self, ids: &str) -> Vec<XNode> {
        let mut ret = Vec::new();
        let table = self.x.ids();
        // Tokens run to the next blank, the first from the very start: leading blanks stay
        // part of it.
        let b = ids.as_bytes();
        let mut tokens = Vec::new();
        let (mut start, mut cur) = (0, 0);
        while cur < b.len() && is_blank(b[cur]) {
            cur += 1;
        }
        while cur < b.len() {
            while cur < b.len() && !is_blank(b[cur]) {
                cur += 1;
            }
            tokens.push(&ids[start..cur]);
            while cur < b.len() && is_blank(b[cur]) {
                cur += 1;
            }
            start = cur;
        }
        for id in tokens {
            if let Some(&attr) = table.get(id)
                && let Some(elem) = self.x.parent(attr)
                && !ret.iter().any(|n: &XNode| n.same(&XNode::Node(elem)))
            {
                ret.push(XNode::Node(elem));
            }
        }
        ret
    }

    fn f_id(&mut self, nargs: i32) {
        if !self.check_arity(nargs, 1) {
            return;
        }
        let Some(mut obj) = self.pop() else {
            self.err(INVALID_OPERAND);
            return;
        };
        if let Some(s) = obj.nodes() {
            let mut ret = Vec::new();
            for n in s {
                let tokens = self.x.node_string(n);
                let ns = self.elements_by_ids(&tokens);
                ret = merge(ret, &ns);
            }
            self.push(XValue::NodeSet(ret));
            return;
        }
        let tokens = cast_to_string(self.x, &mut obj);
        let ret = self.elements_by_ids(&tokens);
        self.push(XValue::NodeSet(ret));
    }

    fn f_escape_uri(&mut self, nargs: i32) {
        if !self.check_arity(nargs, 2) {
            return;
        }
        let escape_reserved = match self.pop() {
            Some(v) => cast_to_bool(&v),
            None => {
                self.err(INVALID_OPERAND);
                false
            }
        };
        self.cast_top_to_string();
        let Some(mut v) = self.pop() else { return };
        let s = cast_to_string(self.x, &mut v);
        let b = s.as_bytes();
        let hex = |d: u8| d.is_ascii_hexdigit();
        let mut out = String::new();
        for (i, &c) in b.iter().enumerate() {
            let keep = c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
                )
                || (c == b'%'
                    && hex(b.get(i + 1).copied().unwrap_or(0))
                    && hex(b.get(i + 2).copied().unwrap_or(0)))
                || (!escape_reserved
                    && matches!(
                        c,
                        b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b','
                    ));
            if keep {
                out.push(c as char);
            } else {
                out.push_str(&format!("%{c:02X}"));
            }
        }
        self.push(XValue::String(out));
    }
}

// ---------------------------------------------------------------------------------------------
// Running an expression

impl Ctx<'_> {
    /// `xmlXPathRunEval` + `xmlXPathCompiledEvalInternal`.
    fn run(&mut self) -> Option<XValue> {
        if self.comp.last < 0 {
            self.err(STACK_ERROR);
            return None;
        }
        self.eval_op(self.comp.last);
        if self.error != 0 {
            return None;
        }
        if self.stack.len() != 1 {
            self.err(STACK_ERROR);
            return None;
        }
        self.stack.pop()
    }
}

/// What an embedding XPath context adds (libxslt's): variable values and functions beyond
/// XPath's core library. Looked up after the core functions, as libxml's function hash has
/// the core ones registered first.
pub trait Host {
    /// `varLookupFunc`: None for an undefined variable.
    fn variable(&self, name: &str, uri: Option<&str>) -> Option<XValue>;
    fn has_function(&self, name: &str, uri: Option<&str>) -> bool;
    /// Calls a function `has_function` knows with its arguments in order; an error is an
    /// XPath error code (`XP_ERROR`).
    fn call(&self, c: &HostCall, name: &str, uri: Option<&str>, args: Vec<XValue>) -> std::result::Result<XValue, i32>;
}

/// The evaluation state a host function sees.
pub struct HostCall<'a> {
    pub x: &'a XDoc,
    pub ns: &'a NsMap,
    pub node: &'a XNode,
    pub pos: i32,
    pub size: i32,
}

/// The error codes a host function can raise.
pub mod err {
    pub const INVALID_TYPE: i32 = super::INVALID_TYPE;
    pub const INVALID_ARITY: i32 = super::INVALID_ARITY;
    pub const INVALID_OPERAND: i32 = super::INVALID_OPERAND;
    pub const UNKNOWN_FUNC: i32 = super::UNKNOWN_FUNC_ERROR;
    pub const STACK: i32 = super::STACK_ERROR;
}

/// An evaluation with an embedding context: the context position and size, the document `/`
/// selects, and the host's variables and functions. Errors are XPath error codes.
pub struct Env<'a> {
    pub ns: &'a NsMap,
    pub pos: i32,
    pub size: i32,
    pub root: NodeId,
    pub host: &'a dyn Host,
}

/// `xmlXPathCompiledEval` in an embedding context; the error is the XPath error code.
pub fn eval_env(comp: &Compiled, x: &XDoc, node: &XNode, env: &Env) -> std::result::Result<XValue, i32> {
    let mut c = Ctx::new(x, comp, env.ns, node.clone());
    c.pos = env.pos;
    c.size = env.size;
    c.root = env.root;
    c.host = Some(env.host);
    match c.run() {
        Some(v) if c.error == 0 => Ok(v),
        _ => Err(if c.error == 0 { STACK_ERROR } else { c.error }),
    }
}

/// libxml's message for an XPath error code (`xmlXPathErrorMessages`).
pub fn error_message(code: i32) -> &'static str {
    message(code)
}

/// Evaluates `comp` with `node` as the context node and the registered namespaces
/// (`xmlXPathCompiledEval`), failing with Postgres's "could not create XPath object".
pub fn eval(comp: &Compiled, x: &XDoc, node: &XNode, ns: &NsMap) -> Result<XValue> {
    let mut c = Ctx::new(x, comp, ns, node.clone());
    match c.run() {
        Some(v) if c.error == 0 => Ok(v),
        _ => Err(xpath_error(
            "could not create XPath object",
            if c.error == 0 { STACK_ERROR } else { c.error },
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing the document

/// `xmlCtxtReadMemory(..., 0)` under `PG_XML_STRICTNESS_ALL`, as XMLTABLE parses its document
/// (the text as given, declaration included).
pub fn parse_document(s: &str) -> Result<XDoc> {
    let opts = Options {
        replace_entities: false,
        keep_blanks: true,
        dtd_attr: false,
    };
    let mut p = Parser::new(Doc::new(None), opts);
    let ok = p.parse_document(s.as_bytes());
    let detail = report(&p.errors, Strictness::All);
    if !ok || detail.is_some() {
        let e = Error::new(code::INVALID_XML_DOCUMENT, "could not parse XML document");
        return Err(match detail {
            Some(d) if !d.is_empty() => e.detail(d),
            _ => e,
        });
    }
    Ok(XDoc::new(p.doc))
}

/// The document xpath() works on: in a UTF-8 database Postgres skips the XML declaration
/// first, ignoring a malformed one.
fn parse_for_xpath(s: &str) -> Result<XDoc> {
    let len = parse_xml_decl(s).map(|d| d.len).unwrap_or(0);
    parse_document(&s[len..])
}

// ---------------------------------------------------------------------------------------------
// Results as xml values (xml_xmlnodetoxmltype)

/// `xml_xmlnodetoxmltype`: attribute, text and namespace nodes become their escaped string
/// value; other nodes are copied (picking up the namespace declarations they need) and dumped.
pub fn xml_xmlnodetoxmltype(x: &XDoc, node: &XNode) -> String {
    let id = match node {
        XNode::Ns(_) => return escape_xml(&x.node_string(node)),
        XNode::Node(id) => *id,
    };
    match x.kind(id) {
        Kind::Attribute | Kind::Text => escape_xml(&x.node_content(id)),
        Kind::Document => {
            let mut s = Saver::new(&x.doc, false, false);
            s.copy_dump();
            s.save_doc();
            s.out
        }
        _ => {
            let mut s = Saver::new(&x.doc, false, false);
            s.node_dump();
            if x.kind(id) == Kind::Element {
                s.set_extra_ns(id, copy_namespaces(x, id));
            }
            s.save_tree(id);
            s.out
        }
    }
}

/// The namespace declarations `xmlCopyNode` adds to the copy's root for prefixes the subtree
/// uses but declares outside it (`xmlStaticCopyNode`, `xmlCopyPropInternal`), in the order the
/// copy meets them.
fn copy_namespaces(x: &XDoc, root: NodeId) -> Vec<Ns> {
    let d = &x.doc;
    let mut extra: Vec<Ns> = Vec::new();
    // Whether `prefix` resolves inside the copy from `el` (its own declarations, its copied
    // ancestors' declarations and namespaces, and what was added to the root).
    let in_copy = |extra: &Vec<Ns>, el: NodeId, prefix: Option<&str>| {
        let mut n = el;
        loop {
            if d.nodes[n]
                .ns_defs()
                .iter()
                .any(|ns| ns.prefix.as_deref() == prefix && ns.href.is_some())
            {
                return true;
            }
            if n != el
                && let Some(ns) = &d.nodes[n].ns
                && ns.prefix.as_deref() == prefix
                && ns.href.is_some()
            {
                return true;
            }
            if n == root {
                return extra.iter().any(|ns| ns.prefix.as_deref() == prefix);
            }
            match d.parent(n) {
                Some(p) => n = p,
                None => return false,
            }
        }
    };
    // The declaration in scope in the original tree.
    let original = |el: NodeId, prefix: Option<&str>| -> Option<Ns> {
        let mut n = Some(el);
        while let Some(e) = n {
            if d.nodes[e].kind != Kind::Element {
                break;
            }
            if let Some(ns) = d.nodes[e]
                .ns_defs()
                .iter()
                .find(|ns| ns.prefix.as_deref() == prefix && ns.href.is_some())
            {
                return Some(Ns::clone(ns));
            }
            if e != el
                && let Some(ns) = &d.nodes[e].ns
                && ns.prefix.as_deref() == prefix
                && ns.href.is_some()
            {
                return Some(Ns::clone(ns));
            }
            n = d.parent(e);
        }
        None
    };
    let add = |extra: &mut Vec<Ns>, el: NodeId, prefix: Option<&str>| {
        if prefix == Some("xml") || in_copy(extra, el, prefix) {
            return;
        }
        if let Some(ns) = original(el, prefix)
            && !d.nodes[root]
                .ns_defs()
                .iter()
                .map(|n| &**n)
                .chain(extra.iter())
                .any(|o| o.prefix == ns.prefix)
        {
            extra.push(ns);
        }
    };
    let mut stack = vec![root];
    while let Some(el) = stack.pop() {
        let n = &d.nodes[el];
        if n.kind != Kind::Element {
            continue;
        }
        if let Some(ns) = &n.ns {
            add(&mut extra, el, ns.prefix.as_deref());
        }
        for a in d.attrs(el) {
            if let Some(ns) = &d.nodes[a].ns {
                add(&mut extra, el, ns.prefix.as_deref());
            }
        }
        for c in d.children(el).rev() {
            stack.push(c);
        }
    }
    extra
}

// ---------------------------------------------------------------------------------------------
// The SQL functions

/// Registers the (prefix, URI) pairs of xpath()'s namespace argument (already checked to be
/// two-dimensional), in order.
fn register_namespaces(a: Option<&crate::types::array::ArrayValue>) -> Result<NsMap> {
    let mut map = NsMap::new();
    let Some(a) = a else { return Ok(map) };
    for pair in a.items.chunks(2) {
        let (Value::Text(name), Value::Text(uri)) = (&pair[0], &pair[1]) else {
            return Err(Error::new(
                code::NULL_VALUE_NOT_ALLOWED,
                "neither namespace name nor URI may be null",
            ));
        };
        if !map.register(&name[..], &uri[..]) {
            return Err(Error::new(
                code::INTERNAL_ERROR,
                format!("could not register XML namespace with name \"{name}\" and URI \"{uri}\""),
            ));
        }
    }
    Ok(map)
}

/// `xpath_internal`: the result of `expr` over the document `data`.
fn xpath_internal(
    expr: &str,
    data: &str,
    ns: Option<&crate::types::array::ArrayValue>,
) -> Result<(XDoc, XValue)> {
    if let Some(a) = ns
        && !a.dims.is_empty()
        && (a.dims.len() != 2 || a.dims[1].0 != 2)
    {
        return Err(Error::new(
            code::DATA_EXCEPTION,
            "invalid array for XML namespace mapping",
        )
        .detail("The array must be two-dimensional with length of the second axis equal to 2."));
    }
    if expr.is_empty() {
        return Err(Error::new(
            code::INVALID_ARGUMENT_FOR_XQUERY,
            "empty XPath expression",
        ));
    }
    let x = parse_for_xpath(data)?;
    let map = register_namespaces(ns)?;
    let comp = compile(expr)?;
    let v = eval(&comp, &x, &XNode::Node(DOC), &map)?;
    Ok((x, v))
}

/// `xpath(text, xml, text[])`: the results as xml values.
pub fn xpath(
    expr: &str,
    data: &str,
    ns: Option<&crate::types::array::ArrayValue>,
    fmt: &crate::types::text::FormatCtx,
) -> Result<Vec<String>> {
    let (x, v) = xpath_internal(expr, data, ns)?;
    Ok(match v {
        XValue::NodeSet(set) | XValue::Tree(set) => set.iter().map(|n| xml_xmlnodetoxmltype(&x, n)).collect(),
        XValue::Bool(b) => vec![(if b { "true" } else { "false" }).to_string()],
        XValue::Number(n) => vec![escape_xml(&crate::types::text::format_value(
            &Value::Float8(n),
            &crate::types::DataType::Float8,
            fmt,
        ))],
        XValue::String(s) => vec![escape_xml(&s)],
    })
}

/// `xmlexists` / `xpath_exists`: whether the expression selects anything (a non-node-set
/// result counts as one item).
pub fn exists(
    expr: &str,
    data: &str,
    ns: Option<&crate::types::array::ArrayValue>,
) -> Result<bool> {
    let (_, v) = xpath_internal(expr, data, ns)?;
    Ok(match v {
        XValue::NodeSet(s) => !s.is_empty(),
        _ => true,
    })
}
