//! The compiled stylesheet (libxslt's xslt.c, preproc.c): the stylesheet document after
//! xsltPreprocessStylesheet, its templates, globals, keys and output settings, each template
//! body compiled to instructions.

use super::pattern::Pattern;
use crate::types::xml::tree::{Doc, NodeId, NsRef};
use crate::types::xml::xpath::{Compiled, NsMap};
use std::collections::HashMap;
use std::rc::Rc;

pub const XSLT_NS: &str = "http://www.w3.org/1999/XSL/Transform";
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// An expanded name: (namespace URI, local name).
pub type QName = (Option<String>, String);

/// A compiled XPath expression with the namespaces in scope at its instruction.
#[derive(Clone)]
pub struct Expr {
    pub comp: Rc<Compiled>,
    pub ns: Rc<NsMap>,
}

/// An attribute value template: literal parts and expressions.
#[derive(Clone)]
pub enum AvtPart {
    Lit(String),
    Expr(Expr),
    /// An unmatched '}': an error when evaluated.
    Fail,
}

#[derive(Clone)]
pub struct Avt(pub Vec<AvtPart>);

impl Avt {
    /// The value if it has no expressions.
    pub fn constant(&self) -> Option<String> {
        let mut s = String::new();
        for p in &self.0 {
            match p {
                AvtPart::Lit(l) => s.push_str(l),
                AvtPart::Expr(_) | AvtPart::Fail => return None,
            }
        }
        Some(s)
    }
}

/// xsl:variable / xsl:param / xsl:with-param.
#[derive(Clone)]
pub struct VarDecl {
    pub name: QName,
    pub select: Option<Expr>,
    pub body: Rc<Vec<Instr>>,
}

#[derive(Clone)]
pub struct Sort {
    pub select: Expr,
    pub data_type: Option<Avt>,
    pub order: Option<Avt>,
}

/// xsl:number.
#[derive(Clone)]
pub struct NumberInstr {
    pub level: Level,
    pub count: Option<Rc<Pattern>>,
    pub from: Option<Rc<Pattern>>,
    pub value: Option<Expr>,
    pub format: Avt,
    pub grouping_separator: Option<Avt>,
    pub grouping_size: Option<Avt>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Single,
    Multiple,
    Any,
}

/// A literal result element: its stylesheet node supplies the name, namespace and the
/// namespace declarations; attributes are value templates.
#[derive(Clone)]
pub struct Literal {
    pub node: NodeId,
    pub attrs: Vec<(NodeId, Avt)>,
    pub use_sets: Vec<QName>,
    pub body: Vec<Instr>,
}

#[derive(Clone)]
pub enum Instr {
    /// A text node of the stylesheet, or xsl:text's (disable-output-escaping).
    Text(String, bool),
    Literal(Box<Literal>),
    ApplyTemplates {
        select: Option<Expr>,
        mode: Option<QName>,
        sorts: Vec<Sort>,
        params: Vec<VarDecl>,
    },
    CallTemplate {
        name: QName,
        params: Vec<VarDecl>,
    },
    ApplyImports,
    ForEach {
        select: Expr,
        sorts: Vec<Sort>,
        body: Vec<Instr>,
    },
    ValueOf(Expr, bool),
    CopyOf(Expr),
    Copy {
        use_sets: Vec<QName>,
        body: Vec<Instr>,
    },
    If(Expr, Vec<Instr>),
    Choose(Vec<(Expr, Vec<Instr>)>, Option<Vec<Instr>>),
    Variable(VarDecl),
    Param(VarDecl),
    Element {
        node: NodeId,
        name: Avt,
        namespace: Option<Avt>,
        use_sets: Vec<QName>,
        body: Vec<Instr>,
    },
    Attribute {
        node: NodeId,
        name: Avt,
        namespace: Option<Avt>,
        body: Vec<Instr>,
    },
    Comment(Vec<Instr>),
    Pi(Avt, Vec<Instr>),
    Number(Box<NumberInstr>),
    Message {
        /// None: a terminate value other than yes / no (an error).
        terminate: Option<bool>,
        body: Vec<Instr>,
    },
    /// An instruction libxslt fails at run time (an XSLT element it didn't compile, an
    /// extension element it doesn't know) with its xsl:fallback bodies, if any.
    Fallback(Vec<Vec<Instr>>),
    /// xsl:document, saxon:output and the like: writing a file, which xml2's security
    /// preferences forbid, stopping the transformation.
    WriteFile,
}

pub struct Template {
    pub pattern: Option<Pattern>,
    pub mode: Option<QName>,
    /// The priority attribute, as the float libxslt keeps.
    pub priority: Option<f32>,
    pub params: Vec<VarDecl>,
    pub body: Rc<Vec<Instr>>,
    /// Namespaces in scope at xsl:template, less XSLT's and the excluded ones
    /// (xsltGetInheritedNsList), declared on the body's top-level literal elements.
    pub inherited_ns: Vec<NsRef>,
}

/// A global xsl:variable or xsl:param.
pub struct Global {
    pub decl: VarDecl,
    pub is_param: bool,
}

pub struct KeyDef {
    pub name: QName,
    pub pattern: Pattern,
    pub use_: Expr,
}

/// xsl:output.
#[derive(Default, Clone)]
pub struct Output {
    pub method: Option<String>,
    /// A method with a prefix (`methodURI`), which serialization refuses.
    pub method_uri: Option<String>,
    pub version: Option<String>,
    pub encoding: Option<String>,
    pub omit_xml_declaration: i32,
    pub standalone: i32,
    pub indent: i32,
    pub doctype_public: Option<String>,
    pub doctype_system: Option<String>,
    pub media_type: Option<String>,
    pub cdata_elements: Vec<QName>,
}

/// The compiled stylesheet.
pub struct Sheet {
    pub doc: Doc,
    pub templates: Vec<Template>,
    pub named: HashMap<QName, usize>,
    pub globals: Vec<Global>,
    pub keys: Vec<KeyDef>,
    pub output: Output,
    /// xsl:strip-space / preserve-space by (name, URI), `*` with a URI for `p:*`; the first
    /// rule for a name holds (xmlHashAddEntry2).
    pub space: HashMap<(String, Option<String>), bool>,
    /// `stripAll`: 1 after strip-space `*`, -1 after preserve-space `*`.
    pub strip_all: i32,
    pub has_space_rules: bool,
    pub decimal_formats: Vec<(Option<QName>, super::number::DecimalFormat)>,
    /// Attribute sets resolved (xsltResolveStylesheetAttributeSet): each set's xsl:attribute
    /// instructions, its used sets' merged in after them.
    pub attr_sets: HashMap<QName, Vec<Instr>>,
    /// namespace-alias: stylesheet URI → result URI (None: the default namespace undeclared).
    pub aliases: HashMap<String, Option<String>>,
}
