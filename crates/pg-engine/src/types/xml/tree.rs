//! The document tree libxml2 builds (tree.c): an arena of nodes with the same shape and
//! node types, so serialization and XPath walk what Postgres walks.
//!
//! Nodes are compact and allocation-free: libxml's parent / child / sibling pointers are
//! 32-bit indexes into the arena, names are shared (`Name`, interned while parsing), a
//! node's content is a span of the document's one text buffer, and the rare parts (namespace
//! declarations, DTD declarations) are boxed out of line.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, LazyLock};

pub type NodeId = usize;

/// "No node" in the arena's links.
const NIL: u32 = u32::MAX;

fn link(id: Option<NodeId>) -> u32 {
    id.map_or(NIL, |i| u32::try_from(i).expect("XML tree over 4G nodes"))
}

fn unlink_id(l: u32) -> Option<NodeId> {
    (l != NIL).then_some(l as NodeId)
}

/// A node or namespace name: shared, so equal names cost one string per document.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(Arc<str>);

static EMPTY: LazyLock<Name> = LazyLock::new(|| Name(Arc::from("")));
static TEXT: LazyLock<Name> = LazyLock::new(|| Name(Arc::from("text")));

impl Name {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// libxml's `xmlStringText`, every text node's name.
    pub fn text() -> Name {
        TEXT.clone()
    }
}

impl Default for Name {
    fn default() -> Name {
        EMPTY.clone()
    }
}

impl Deref for Name {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Name {
    fn from(s: &str) -> Name {
        match s {
            "" => Name::default(),
            "text" => Name::text(),
            _ => Name(Arc::from(s)),
        }
    }
}

impl From<String> for Name {
    fn from(s: String) -> Name {
        Name::from(s.as_str())
    }
}

impl From<&String> for Name {
    fn from(s: &String) -> Name {
        Name::from(s.as_str())
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, o: &str) -> bool {
        &*self.0 == o
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, o: &&str) -> bool {
        &*self.0 == *o
    }
}

impl PartialEq<String> for Name {
    fn eq(&self, o: &String) -> bool {
        *self.0 == **o
    }
}

impl PartialEq<Name> for str {
    fn eq(&self, o: &Name) -> bool {
        self == &*o.0
    }
}

impl PartialEq<Name> for &str {
    fn eq(&self, o: &Name) -> bool {
        *self == &*o.0
    }
}

impl PartialEq<Name> for String {
    fn eq(&self, o: &Name) -> bool {
        **self == *o.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

/// Interns names while a document is built.
#[derive(Default)]
pub struct Names(HashSet<Name>);

impl Names {
    pub fn get(&mut self, s: &str) -> Name {
        if let Some(n) = self.0.get(s) {
            return n.clone();
        }
        let n = Name::from(s);
        self.0.insert(n.clone());
        n
    }
}

/// A namespace binding (`xmlNs`). `href` is None for the placeholder libxml declares when a
/// prefix can't be resolved.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ns {
    pub prefix: Option<String>,
    pub href: Option<String>,
}

/// A shared `xmlNs`: a node's namespace points at a declaration, as in libxml.
pub type NsRef = Arc<Ns>;

pub fn ns_ref(prefix: Option<&str>, href: Option<&str>) -> NsRef {
    Arc::new(Ns {
        prefix: prefix.map(str::to_string),
        href: href.map(str::to_string),
    })
}

/// `xmlElementType`, the subset the parser and XPath produce.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Element,
    Attribute,
    Text,
    CData,
    EntityRef,
    Pi,
    Comment,
    Document,
    Dtd,
    ElementDecl,
    AttributeDecl,
    EntityDecl,
    /// An `xmlNs` handed out by XPath's namespace axis.
    Namespace,
}

/// `xmlEntityType`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntityType {
    InternalGeneral,
    ExternalGeneralParsed,
    ExternalGeneralUnparsed,
    InternalParameter,
    ExternalParameter,
    InternalPredefined,
}

#[derive(Clone, Debug)]
pub struct Entity {
    pub name: String,
    pub etype: EntityType,
    pub external_id: Option<String>,
    pub system_id: Option<String>,
    pub content: Option<String>,
    pub orig: Option<String>,
    /// The parsed replacement text (`ent->children`), filled on first reference.
    pub children: Vec<NodeId>,
    pub expanding: bool,
    pub parsed: bool,
}

/// An element content model (`xmlElementContent`).
#[derive(Clone, Debug)]
pub struct ElemContent {
    pub kind: ContentKind,
    pub ocur: Occur,
    pub name: Option<String>,
    pub prefix: Option<String>,
    pub c1: Option<Box<ElemContent>>,
    pub c2: Option<Box<ElemContent>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContentKind {
    Pcdata,
    Element,
    Seq,
    Or,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Occur {
    Once,
    Opt,
    Mult,
    Plus,
}

/// `xmlElementTypeVal`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElemType {
    Empty,
    Any,
    Mixed,
    Element,
}

/// `xmlAttributeType`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttrType {
    Cdata,
    Id,
    Idref,
    Idrefs,
    Entity,
    Entities,
    Nmtoken,
    Nmtokens,
    Enumeration,
    Notation,
}

/// `xmlAttributeDefault`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttrDefault {
    None,
    Required,
    Implied,
    Fixed,
}

/// Declaration payloads hanging off DTD children.
#[derive(Clone, Debug, Default)]
pub enum Decl {
    #[default]
    None,
    Dtd {
        external_id: Option<String>,
        system_id: Option<String>,
    },
    Element {
        etype: ElemType,
        content: Option<ElemContent>,
        prefix: Option<String>,
    },
    Attribute {
        elem: String,
        prefix: Option<String>,
        atype: AttrType,
        def: AttrDefault,
        default: Option<String>,
        tree: Vec<String>,
    },
    Entity(usize),
}

/// The rarely used parts of a node, boxed so the common node stays small.
#[derive(Debug, Default)]
struct Ext {
    ns_defs: Vec<NsRef>,
    decl: Decl,
}

static NO_DECL: Decl = Decl::None;

/// A span of the document's text buffer.
#[derive(Clone, Copy, Debug, Default)]
struct Span {
    off: u32,
    len: u32,
}

#[derive(Debug)]
pub struct Node {
    pub kind: Kind,
    /// A processing instruction without data (NULL `content`).
    pub no_content: bool,
    pub line: u32,
    pub name: Name,
    pub ns: Option<NsRef>,
    text: Span,
    parent: u32,
    first: u32,
    last: u32,
    next: u32,
    prev: u32,
    /// The first attribute; attributes are a sibling list of their own.
    attr: u32,
    ext: Option<Box<Ext>>,
}

impl Node {
    pub fn new(kind: Kind, name: impl Into<Name>) -> Node {
        Node {
            kind,
            no_content: false,
            line: 0,
            name: name.into(),
            ns: None,
            text: Span::default(),
            parent: NIL,
            first: NIL,
            last: NIL,
            next: NIL,
            prev: NIL,
            attr: NIL,
            ext: None,
        }
    }

    /// The namespaces declared on this element (`nsDef`).
    pub fn ns_defs(&self) -> &[NsRef] {
        self.ext.as_ref().map_or(&[], |e| &e.ns_defs)
    }

    pub fn ns_defs_mut(&mut self) -> &mut Vec<NsRef> {
        &mut self.ext.get_or_insert_with(Default::default).ns_defs
    }

    pub fn decl(&self) -> &Decl {
        self.ext.as_ref().map_or(&NO_DECL, |e| &e.decl)
    }

    pub fn set_decl(&mut self, d: Decl) {
        self.ext.get_or_insert_with(Default::default).decl = d;
    }

    /// The node's parent: an attribute's element, an attribute value's attribute.
    pub fn parent(&self) -> Option<NodeId> {
        unlink_id(self.parent)
    }

    /// A copy of this node with no links (its text a span of the same document).
    fn detached(&self) -> Node {
        Node {
            kind: self.kind,
            no_content: self.no_content,
            line: self.line,
            name: self.name.clone(),
            ns: self.ns.clone(),
            text: self.text,
            parent: NIL,
            first: NIL,
            last: NIL,
            next: NIL,
            prev: NIL,
            attr: NIL,
            ext: self.ext.as_ref().map(|e| {
                Box::new(Ext {
                    ns_defs: e.ns_defs.clone(),
                    decl: e.decl.clone(),
                })
            }),
        }
    }
}

/// log2 of the nodes per chunk of the arena.
const CHUNK_BITS: usize = 12;
const CHUNK: usize = 1 << CHUNK_BITS;

/// The node arena: chunks of `CHUNK` nodes (the first grows up to that), so a big
/// document's arena grows without copying itself into an allocation twice its size.
#[derive(Debug, Default)]
pub struct Nodes {
    chunks: Vec<Vec<Node>>,
    len: usize,
}

impl Nodes {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, n: Node) {
        match self.chunks.last_mut() {
            Some(c) if c.len() < CHUNK => c.push(n),
            _ => {
                let mut c = Vec::with_capacity(if self.chunks.is_empty() { 4 } else { CHUNK });
                c.push(n);
                self.chunks.push(c);
            }
        }
        self.len += 1;
    }

    pub fn iter(&self) -> impl Iterator<Item = &Node> {
        self.chunks.iter().flatten()
    }

    fn into_nodes(self) -> impl Iterator<Item = Node> {
        self.chunks.into_iter().flatten()
    }
}

impl std::ops::Index<usize> for Nodes {
    type Output = Node;
    fn index(&self, i: usize) -> &Node {
        &self.chunks[i >> CHUNK_BITS][i & (CHUNK - 1)]
    }
}

impl std::ops::IndexMut<usize> for Nodes {
    fn index_mut(&mut self, i: usize) -> &mut Node {
        &mut self.chunks[i >> CHUNK_BITS][i & (CHUNK - 1)]
    }
}

/// Children or attributes in order: a sibling chain.
#[derive(Clone)]
pub struct Siblings<'d> {
    nodes: &'d Nodes,
    front: u32,
    back: u32,
}

impl Iterator for Siblings<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let id = unlink_id(self.front)?;
        if self.front == self.back {
            self.front = NIL;
            self.back = NIL;
        } else {
            self.front = self.nodes[id].next;
        }
        Some(id)
    }
}

impl DoubleEndedIterator for Siblings<'_> {
    fn next_back(&mut self) -> Option<NodeId> {
        let id = unlink_id(self.back)?;
        if self.front == self.back {
            self.front = NIL;
            self.back = NIL;
        } else {
            self.back = self.nodes[id].prev;
        }
        Some(id)
    }
}

/// An `xmlDoc`: node 0 is the document node.
#[derive(Debug)]
pub struct Doc {
    pub nodes: Nodes,
    /// Every node's content, each a span of this.
    buf: String,
    pub version: Option<String>,
    pub encoding: Option<String>,
    pub standalone: i32,
    pub int_subset: Option<NodeId>,
    pub has_ext_subset: bool,
    pub entities: Vec<Entity>,
    pub general: HashMap<String, usize>,
    pub parameter: HashMap<String, usize>,
    pub elem_decls: HashMap<(String, Option<String>), NodeId>,
    pub attr_decls: HashMap<(String, String, Option<String>), NodeId>,
    pub notations: Vec<(String, Option<String>, Option<String>)>,
}

pub const DOC: NodeId = 0;

impl Doc {
    pub fn new(version: Option<&str>) -> Doc {
        Doc {
            nodes: {
                let mut n = Nodes::default();
                n.push(Node::new(Kind::Document, ""));
                n
            },
            buf: String::new(),
            version: version.map(str::to_string),
            encoding: None,
            standalone: -1,
            int_subset: None,
            has_ext_subset: false,
            entities: Vec::new(),
            general: HashMap::new(),
            parameter: HashMap::new(),
            elem_decls: HashMap::new(),
            attr_decls: HashMap::new(),
            notations: Vec::new(),
        }
    }

    pub fn add(&mut self, n: Node) -> NodeId {
        self.nodes.push(n);
        self.nodes.len() - 1
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    // --- content

    /// A node's own content (libxml's `node->content`).
    pub fn text(&self, n: &Node) -> &str {
        &self.buf[n.text.off as usize..(n.text.off + n.text.len) as usize]
    }

    pub fn text_of(&self, id: NodeId) -> &str {
        self.text(&self.nodes[id])
    }

    fn put(&mut self, s: &str) -> Span {
        let off = u32::try_from(self.buf.len()).expect("XML text over 4 GB");
        self.buf.push_str(s);
        Span {
            off,
            len: s.len() as u32,
        }
    }

    pub fn set_text(&mut self, id: NodeId, s: &str) {
        self.nodes[id].text = self.put(s);
    }

    /// Appends to a node's content (`xmlNodeAddContent`): in place when it ends the buffer.
    pub fn push_text(&mut self, id: NodeId, s: &str) {
        let t = self.nodes[id].text;
        if (t.off + t.len) as usize != self.buf.len() {
            let old = t.off as usize..(t.off + t.len) as usize;
            self.buf.extend_from_within(old);
            self.nodes[id].text.off = (self.buf.len() - t.len as usize) as u32;
        }
        self.buf.push_str(s);
        let len = u32::try_from(self.nodes[id].text.len as usize + s.len()).expect("XML text over 4 GB");
        self.nodes[id].text.len = len;
    }

    /// A new detached text node.
    pub fn new_text(&mut self, content: &str) -> NodeId {
        let id = self.add(Node::new(Kind::Text, Name::text()));
        self.set_text(id, content);
        id
    }

    /// A new detached node of `kind` with content.
    pub fn new_node(&mut self, kind: Kind, name: impl Into<Name>, content: &str) -> NodeId {
        let id = self.add(Node::new(kind, name));
        self.set_text(id, content);
        id
    }

    // --- links

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        unlink_id(self.nodes[id].parent)
    }

    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        unlink_id(self.nodes[id].first)
    }

    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        unlink_id(self.nodes[id].last)
    }

    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        unlink_id(self.nodes[id].next)
    }

    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        unlink_id(self.nodes[id].prev)
    }

    pub fn has_children(&self, id: NodeId) -> bool {
        self.nodes[id].first != NIL
    }

    pub fn children(&self, id: NodeId) -> Siblings<'_> {
        let n = &self.nodes[id];
        Siblings {
            nodes: &self.nodes,
            front: n.first,
            back: n.last,
        }
    }

    /// `id` and the siblings after it.
    pub fn following(&self, id: Option<NodeId>) -> Siblings<'_> {
        let mut back = link(id);
        if let Some(mut i) = id {
            while let Some(n) = self.next_sibling(i) {
                i = n;
            }
            back = i as u32;
        }
        Siblings {
            nodes: &self.nodes,
            front: link(id),
            back,
        }
    }

    pub fn first_attr(&self, id: NodeId) -> Option<NodeId> {
        unlink_id(self.nodes[id].attr)
    }

    pub fn attrs(&self, id: NodeId) -> Siblings<'_> {
        self.following(self.first_attr(id))
    }

    pub fn child_count(&self, id: NodeId) -> usize {
        self.children(id).count()
    }

    /// Appends the detached node `child` to `parent`'s children (`xmlAddChild`, without
    /// merging text).
    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        let last = self.nodes[parent].last;
        let c = &mut self.nodes[child];
        c.parent = parent as u32;
        c.prev = last;
        c.next = NIL;
        if last == NIL {
            self.nodes[parent].first = child as u32;
        } else {
            self.nodes[last as usize].next = child as u32;
        }
        self.nodes[parent].last = child as u32;
    }

    /// Makes the detached node `child` `parent`'s first child.
    pub fn prepend(&mut self, parent: NodeId, child: NodeId) {
        match self.first_child(parent) {
            Some(f) => self.insert_before(f, child),
            None => self.append(parent, child),
        }
    }

    /// Inserts the detached node `new` before the child `at`.
    pub fn insert_before(&mut self, at: NodeId, new: NodeId) {
        let (parent, prev) = (self.nodes[at].parent, self.nodes[at].prev);
        let n = &mut self.nodes[new];
        n.parent = parent;
        n.prev = prev;
        n.next = at as u32;
        self.nodes[at].prev = new as u32;
        if prev != NIL {
            self.nodes[prev as usize].next = new as u32;
        } else if parent != NIL {
            let p = &mut self.nodes[parent as usize];
            if p.attr == at as u32 {
                p.attr = new as u32;
            } else {
                p.first = new as u32;
            }
        }
    }

    /// Appends the detached attribute `attr` to `elem`'s attributes.
    pub fn append_attr(&mut self, elem: NodeId, attr: NodeId) {
        let mut last = NIL;
        let mut a = self.nodes[elem].attr;
        while a != NIL {
            last = a;
            a = self.nodes[a as usize].next;
        }
        let n = &mut self.nodes[attr];
        n.parent = elem as u32;
        n.prev = last;
        n.next = NIL;
        if last == NIL {
            self.nodes[elem].attr = attr as u32;
        } else {
            self.nodes[last as usize].next = attr as u32;
        }
    }

    /// Takes `id` out of its parent's children or attributes and its sibling chain
    /// (`xmlUnlinkNode`).
    pub fn unlink(&mut self, id: NodeId) {
        let (parent, prev, next) = {
            let n = &self.nodes[id];
            (n.parent, n.prev, n.next)
        };
        if prev != NIL {
            self.nodes[prev as usize].next = next;
        }
        if next != NIL {
            self.nodes[next as usize].prev = prev;
        }
        if parent != NIL {
            let p = &mut self.nodes[parent as usize];
            if p.attr == id as u32 {
                p.attr = next;
            } else {
                if p.first == id as u32 {
                    p.first = next;
                }
                if p.last == id as u32 {
                    p.last = prev;
                }
            }
        }
        let n = &mut self.nodes[id];
        n.parent = NIL;
        n.prev = NIL;
        n.next = NIL;
    }

    /// Detaches every child of `id`, keeping them linked to each other (their parent unset),
    /// and returns them.
    pub fn take_children(&mut self, id: NodeId) -> Vec<NodeId> {
        let kids: Vec<NodeId> = self.children(id).collect();
        for &c in &kids {
            self.nodes[c].parent = NIL;
        }
        self.nodes[id].first = NIL;
        self.nodes[id].last = NIL;
        kids
    }

    /// Detaches every child of `id` from it and from each other.
    pub fn clear_children(&mut self, id: NodeId) {
        for c in self.take_children(id) {
            self.nodes[c].prev = NIL;
            self.nodes[c].next = NIL;
        }
    }

    /// Detaches every attribute of `id`.
    pub fn clear_attrs(&mut self, id: NodeId) {
        while let Some(a) = self.first_attr(id) {
            self.unlink(a);
        }
    }

    /// Links detached nodes as siblings without a parent (a node list such as an entity's
    /// content or a parsed chunk).
    pub fn link_list(&mut self, list: &[NodeId]) {
        for w in list.windows(2) {
            self.nodes[w[0]].next = w[1] as u32;
            self.nodes[w[1]].prev = w[0] as u32;
        }
    }

    /// A detached shallow copy of `id` in this document.
    pub fn dup(&mut self, id: NodeId) -> NodeId {
        let n = self.nodes[id].detached();
        self.add(n)
    }

    /// A detached shallow copy of another document's node (its content copied).
    pub fn import(&mut self, src: &Doc, id: NodeId) -> NodeId {
        let mut n = src.nodes[id].detached();
        n.text = self.put(src.text_of(id));
        self.add(n)
    }

    /// Moves another document's nodes into this arena (their links and content kept);
    /// returns the index its node 0 got.
    pub fn absorb(&mut self, other: Doc) -> NodeId {
        let base = self.nodes.len();
        let shift = |l: u32| if l == NIL { NIL } else { link(Some(l as usize + base)) };
        let toff = u32::try_from(self.buf.len()).expect("XML text over 4 GB");
        self.buf.push_str(&other.buf);
        for mut n in other.nodes.into_nodes() {
            n.parent = shift(n.parent);
            n.first = shift(n.first);
            n.last = shift(n.last);
            n.next = shift(n.next);
            n.prev = shift(n.prev);
            n.attr = shift(n.attr);
            n.text.off += toff;
            self.nodes.push(n);
        }
        base
    }

    /// `xmlGetPredefinedEntity`.
    pub fn predefined(name: &str) -> Option<&'static str> {
        Some(match name {
            "lt" => "<",
            "gt" => ">",
            "amp" => "&",
            "apos" => "'",
            "quot" => "\"",
            _ => return None,
        })
    }

    /// The general entity `name` (`xmlGetDocEntity`, without the predefined ones).
    pub fn entity(&self, name: &str) -> Option<usize> {
        self.general.get(name).copied()
    }

    /// The root element (`xmlDocGetRootElement`).
    pub fn root_element(&self) -> Option<NodeId> {
        self.children(DOC).find(|&c| self.nodes[c].kind == Kind::Element)
    }

    /// Element/attribute qualified name as serialized.
    pub fn qname(&self, id: NodeId) -> String {
        let n = &self.nodes[id];
        match n.ns.as_ref().and_then(|ns| ns.prefix.as_deref()) {
            Some(p) => format!("{p}:{}", n.name),
            None => n.name.to_string(),
        }
    }

    /// `xmlNodeGetContent` / `xmlBufGetNodeContent`.
    pub fn content(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.content_into(id, &mut out);
        out
    }

    pub fn content_into(&self, id: NodeId, out: &mut String) {
        let n = &self.nodes[id];
        match n.kind {
            Kind::Element | Kind::Document => {
                // xmlBufGetChildContent: text-ish descendants in document order.
                self.child_content(id, out);
            }
            Kind::Attribute => {
                for c in self.children(id) {
                    match self.nodes[c].kind {
                        Kind::Text => out.push_str(self.text_of(c)),
                        Kind::EntityRef => self.content_into(c, out),
                        _ => {}
                    }
                }
            }
            Kind::Text | Kind::CData | Kind::Comment | Kind::Pi => out.push_str(self.text(n)),
            Kind::EntityRef => {
                if let Some(e) = self.entity(&n.name) {
                    for &c in &self.entities[e].children {
                        self.content_into(c, out);
                    }
                }
            }
            Kind::Namespace => {
                if let Some(h) = n.ns.as_ref().and_then(|ns| ns.href.as_deref()) {
                    out.push_str(h);
                }
            }
            Kind::EntityDecl => {
                if let Decl::Entity(e) = *n.decl() {
                    for &c in &self.entities[e].children {
                        self.content_into(c, out);
                    }
                }
            }
            _ => {}
        }
    }

    /// Without recursion: deeply nested documents mustn't overflow the stack.
    fn child_content(&self, id: NodeId, out: &mut String) {
        let mut cur = self.first_child(id);
        while let Some(c) = cur {
            let n = &self.nodes[c];
            match n.kind {
                Kind::Text | Kind::CData => out.push_str(self.text(n)),
                Kind::EntityRef => self.content_into(c, out),
                Kind::Element if n.first != NIL => {
                    cur = unlink_id(n.first);
                    continue;
                }
                _ => {}
            }
            // the next node in document order below `id`
            let mut at = c;
            cur = loop {
                if let Some(s) = self.next_sibling(at) {
                    break Some(s);
                }
                match self.parent(at) {
                    Some(p) if p != id => at = p,
                    _ => break None,
                }
            };
        }
    }
}
