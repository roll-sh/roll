//! xsl:number's counting (libxslt numbers.c: xsltNumberFormatGetSingleLevel /
//! GetMultipleLevel / GetAnyLevel); the formatting is `number`.

use super::apply::Ctx;
use super::exec::{Exec, R, Stop};
use super::style::*;
use crate::types::xml::tree::{Kind, NodeId};
use crate::types::xml::xpath::{self, XNode};

impl Exec<'_, '_> {
    /// Whether `n` is counted: the count pattern, else the context node's type and name.
    fn counted(&self, n: NodeId, count: Option<&super::pattern::Pattern>, like: NodeId) -> R<bool> {
        if let Some(p) = count {
            return p.matches(&self.x, &XNode::Node(n), self).map_err(|_| Stop::Stopped);
        }
        let (a, b) = (&self.x.doc.nodes[n], &self.x.doc.nodes[like]);
        Ok(a.kind == b.kind
            && match a.kind {
                Kind::Element | Kind::Attribute => {
                    a.name == b.name
                        && a.ns.as_ref().and_then(|x| x.href.as_deref()) == b.ns.as_ref().and_then(|x| x.href.as_deref())
                }
                Kind::Pi => a.name == b.name,
                _ => true,
            })
    }

    fn is_from(&self, n: NodeId, from: Option<&super::pattern::Pattern>) -> R<bool> {
        match from {
            Some(p) => p.matches(&self.x, &XNode::Node(n), self).map_err(|_| Stop::Stopped),
            None => Ok(false),
        }
    }

    /// The preceding siblings of `n` that count, plus one.
    fn sibling_number(&self, n: NodeId, count: Option<&super::pattern::Pattern>, like: NodeId) -> R<f64> {
        let mut k = 1.0;
        let Some(p) = self.x.parent(n) else { return Ok(k) };
        if self.x.doc.nodes[n].kind == Kind::Attribute {
            return Ok(k);
        }
        for &s in &self.x.doc.nodes[p].children {
            if s == n {
                break;
            }
            if self.counted(s, count, like)? {
                k += 1.0;
            }
        }
        Ok(k)
    }

    pub fn number(&mut self, n: &NumberInstr, c: &Ctx) -> R<()> {
        let format = self.avt(&n.format, &c.node, c.pos, c.size)?;
        let gsep = match &n.grouping_separator {
            Some(a) => Some(self.avt(a, &c.node, c.pos, c.size)?),
            None => None,
        };
        let gsize = match &n.grouping_size {
            Some(a) => Some(self.avt(a, &c.node, c.pos, c.size)?),
            None => None,
        };
        let numbers: Vec<f64> = if let Some(v) = &n.value {
            let mut v = self.eval(v, &c.node, c.pos, c.size)?;
            vec![xpath::cast_to_number(&self.x, &mut v)]
        } else {
            let XNode::Node(node) = c.node else { return Ok(()) };
            let count = n.count.as_deref();
            let from = n.from.as_deref();
            let mut out = Vec::new();
            match n.level {
                Level::Single => {
                    let mut cur = Some(node);
                    while let Some(a) = cur {
                        if self.x.doc.nodes[a].kind == Kind::Document || self.is_from(a, from)? {
                            break;
                        }
                        if self.counted(a, count, node)? {
                            out.push(self.sibling_number(a, count, node)?);
                            break;
                        }
                        cur = self.x.parent(a);
                    }
                }
                Level::Multiple => {
                    let mut cur = Some(node);
                    while let Some(a) = cur {
                        if self.x.doc.nodes[a].kind == Kind::Document || self.is_from(a, from)? {
                            break;
                        }
                        if self.counted(a, count, node)? {
                            out.insert(0, self.sibling_number(a, count, node)?);
                        }
                        cur = self.x.parent(a);
                    }
                }
                Level::Any => {
                    let mut k = 0.0;
                    let mut cur = Some(node);
                    while let Some(a) = cur {
                        if self.is_from(a, from)? {
                            break;
                        }
                        if self.counted(a, count, node)? {
                            k += 1.0;
                        }
                        cur = self.preceding_or_ancestor(a);
                    }
                    if k > 0.0 {
                        out.push(k);
                    }
                }
            }
            out
        };
        let s = super::number::format_numbers(&numbers, &format, gsep.as_deref(), gsize.as_deref());
        if !s.is_empty() {
            self.add_text(&s, false);
        }
        Ok(())
    }

    /// The node before `n` in document order (its previous sibling's last descendant, or its
    /// parent).
    fn preceding_or_ancestor(&self, n: NodeId) -> Option<NodeId> {
        let p = self.x.parent(n)?;
        if self.x.doc.nodes[n].kind == Kind::Attribute {
            return Some(p);
        }
        let sibs = &self.x.doc.nodes[p].children;
        let i = sibs.iter().position(|&s| s == n)?;
        if i == 0 {
            return Some(p);
        }
        let mut c = sibs[i - 1];
        while let Some(&l) = self.x.doc.nodes[c].children.last() {
            c = l;
        }
        Some(c)
    }
}
