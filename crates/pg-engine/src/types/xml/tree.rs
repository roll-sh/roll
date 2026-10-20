//! The document tree libxml2 builds (tree.c): an arena of nodes with the same shape and
//! node types, so serialization and XPath walk what Postgres walks.

use std::collections::HashMap;

pub type NodeId = usize;

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

/// A namespace binding (`xmlNs`). `href` is None for the placeholder libxml declares when a
/// prefix can't be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ns {
    pub prefix: Option<String>,
    pub href: Option<String>,
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
#[derive(Clone, Debug)]
pub enum Decl {
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

#[derive(Clone, Debug)]
pub struct Node {
    pub kind: Kind,
    pub name: String,
    pub ns: Option<Ns>,
    pub content: String,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub attrs: Vec<NodeId>,
    pub ns_defs: Vec<Ns>,
    pub line: u32,
    pub decl: Decl,
    /// A processing instruction without data (NULL `content`).
    pub no_content: bool,
}

impl Node {
    pub fn new(kind: Kind, name: impl Into<String>) -> Node {
        Node {
            kind,
            name: name.into(),
            ns: None,
            content: String::new(),
            parent: None,
            children: Vec::new(),
            attrs: Vec::new(),
            ns_defs: Vec::new(),
            line: 0,
            decl: Decl::None,
            no_content: false,
        }
    }
}

/// An `xmlDoc`: node 0 is the document node.
#[derive(Clone, Debug)]
pub struct Doc {
    pub nodes: Vec<Node>,
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
            nodes: vec![Node::new(Kind::Document, "")],
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

    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
    }

    pub fn text(&mut self, content: &str) -> NodeId {
        let mut n = Node::new(Kind::Text, "text");
        n.content = content.to_string();
        self.add(n)
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
        self.nodes[DOC]
            .children
            .iter()
            .copied()
            .find(|&c| self.nodes[c].kind == Kind::Element)
    }

    /// Element/attribute qualified name as serialized.
    pub fn qname(&self, id: NodeId) -> String {
        let n = &self.nodes[id];
        match n.ns.as_ref().and_then(|ns| ns.prefix.as_deref()) {
            Some(p) => format!("{p}:{}", n.name),
            None => n.name.clone(),
        }
    }

    /// `xmlNodeGetContent` / `xmlBufGetNodeContent`.
    pub fn content(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.content_into(id, &mut out);
        out
    }

    fn content_into(&self, id: NodeId, out: &mut String) {
        let n = &self.nodes[id];
        match n.kind {
            Kind::Element | Kind::Document => {
                // xmlBufGetChildContent: text-ish descendants in document order.
                self.child_content(id, out);
            }
            Kind::Attribute => {
                for &c in &n.children {
                    match self.nodes[c].kind {
                        Kind::Text => out.push_str(&self.nodes[c].content),
                        Kind::EntityRef => self.content_into(c, out),
                        _ => {}
                    }
                }
            }
            Kind::Text | Kind::CData | Kind::Comment | Kind::Pi => out.push_str(&n.content),
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
                if let Decl::Entity(e) = n.decl {
                    for &c in &self.entities[e].children {
                        self.content_into(c, out);
                    }
                }
            }
            _ => {}
        }
    }

    fn child_content(&self, id: NodeId, out: &mut String) {
        for &c in &self.nodes[id].children {
            let k = self.nodes[c].kind;
            match k {
                Kind::Text | Kind::CData => out.push_str(&self.nodes[c].content),
                Kind::EntityRef => self.content_into(c, out),
                Kind::Element => self.child_content(c, out),
                _ => {}
            }
        }
    }
}
