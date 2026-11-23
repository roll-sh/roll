//! A page's HTML: parsed by `html5ever` (the WHATWG algorithm, so malformed pages parse as
//! browsers parse them) into a small arena DOM, each node and text run charged to the request
//! breaker as the tree builder makes it, and read the way Open Crawler reads a page
//! (`Crawler::Data::CrawlResult::HTML`, `ContentEngine::Transformer` and `::Utils`): title,
//! body text with `data-elastic-include`/`-exclude` and `exclude_tags`, meta tags, links,
//! headings, the canonical URL and robots directives. Nothing in the page runs. Reading never
//! recurses. html5ever puts no limit on nesting, and each element it opens scans its stack of open
//! elements, so a page that nests deeper than [`MAX_DEPTH`] elements stops the parse and fails the
//! page, and a parse is fed in small chunks against a deadline ([`read_page`]).

use super::config::{L, Limits};
use super::url::Link;
use crate::breaker::Reservation;
use html5ever::interface::tree_builder::{ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, LocalName, Namespace, QualName, local_name, ns};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use unicode_segmentation::UnicodeSegmentation;

/// Elements deeper than this aren't read.
pub const MAX_DEPTH: usize = 512;
/// What a node costs the breaker beyond its text and attributes.
const NODE_COST: u64 = 96;
const NONE: usize = usize::MAX;
/// The handle the builder gives out once the breaker has tripped: never attached.
const DETACHED: usize = 1;

enum Data {
    Document,
    Element { name: QualName, attrs: Vec<Attribute>, template: usize },
    Text(String),
    /// Comments, processing instructions and template contents: kept only as places in the tree.
    Other,
}

struct Node {
    data: Data,
    /// Elements above it when it was attached.
    depth: u32,
    parent: usize,
    first: usize,
    last: usize,
    prev: usize,
    next: usize,
}

impl Node {
    fn new(data: Data) -> Node {
        Node { data, depth: 0, parent: NONE, first: NONE, last: NONE, prev: NONE, next: NONE }
    }
}

/// A parsed page.
pub struct Dom {
    nodes: Vec<Node>,
}

/// An element's name as the tree builder asks for it.
#[derive(Debug)]
pub struct Name(QualName);

impl ElemName for Name {
    fn ns(&self) -> &Namespace {
        &self.0.ns
    }
    fn local_name(&self) -> &LocalName {
        &self.0.local
    }
}

struct Builder {
    nodes: RefCell<Vec<Node>>,
    held: RefCell<Reservation>,
    /// Why the breaker tripped, once it has: from then on nothing more is built.
    tripped: RefCell<Option<String>>,
    stop: Cell<bool>,
}

impl Builder {
    fn charge(&self, bytes: u64) -> bool {
        if self.stop.get() {
            return false;
        }
        if let Err(e) = self.held.borrow_mut().add(bytes, "crawled page") {
            self.fail(e.reason().to_string());
            return false;
        }
        true
    }

    /// Stops building: the page fails with `why`.
    fn fail(&self, why: String) {
        *self.tripped.borrow_mut() = Some(why);
        self.stop.set(true);
    }

    fn push(&self, data: Data, cost: u64) -> usize {
        if !self.charge(NODE_COST + cost) {
            return DETACHED;
        }
        let mut n = self.nodes.borrow_mut();
        n.push(Node::new(data));
        n.len() - 1
    }

    fn detach(n: &mut [Node], i: usize) {
        let (p, prev, next) = (n[i].parent, n[i].prev, n[i].next);
        if p == NONE {
            return;
        }
        if prev != NONE {
            n[prev].next = next;
        } else {
            n[p].first = next;
        }
        if next != NONE {
            n[next].prev = prev;
        } else {
            n[p].last = prev;
        }
        n[i].parent = NONE;
        n[i].prev = NONE;
        n[i].next = NONE;
    }

    /// Puts `i` under `parent`, before `before` (or last).
    fn attach(n: &mut [Node], parent: usize, i: usize, before: usize) {
        Self::detach(n, i);
        n[i].parent = parent;
        if before == NONE {
            let last = n[parent].last;
            n[i].prev = last;
            if last != NONE {
                n[last].next = i;
            } else {
                n[parent].first = i;
            }
            n[parent].last = i;
        } else {
            let prev = n[before].prev;
            n[i].prev = prev;
            n[i].next = before;
            n[before].prev = i;
            if prev != NONE {
                n[prev].next = i;
            } else {
                n[parent].first = i;
            }
        }
    }

    /// Adds `child` under `parent` before `before` (or last), merging text with the text before
    /// it.
    fn insert(&self, parent: usize, before: usize, child: NodeOrText<usize>) {
        if self.stop.get() || parent == DETACHED || parent == NONE {
            return;
        }
        match child {
            NodeOrText::AppendNode(c) => {
                if c == DETACHED {
                    return;
                }
                let depth = self.nodes.borrow()[parent].depth + 1;
                if depth as usize > MAX_DEPTH {
                    self.fail(format!("the page nests elements more than {MAX_DEPTH} deep"));
                    return;
                }
                let mut n = self.nodes.borrow_mut();
                Self::attach(&mut n, parent, c, before);
                n[c].depth = depth;
            }
            NodeOrText::AppendText(t) => {
                let prev = {
                    let n = self.nodes.borrow();
                    if before == NONE { n[parent].last } else { n[before].prev }
                };
                let merges = prev != NONE && matches!(self.nodes.borrow()[prev].data, Data::Text(_));
                if merges {
                    if self.charge(t.len() as u64)
                        && let Data::Text(s) = &mut self.nodes.borrow_mut()[prev].data
                    {
                        s.push_str(&t);
                    }
                    return;
                }
                let i = self.push(Data::Text(t.to_string()), t.len() as u64);
                if i != DETACHED {
                    Self::attach(&mut self.nodes.borrow_mut(), parent, i, before);
                }
            }
        }
    }
}

fn div() -> QualName {
    QualName::new(None, ns!(html), local_name!("div"))
}

impl TreeSink for Builder {
    type Handle = usize;
    type Output = Builder;
    type ElemName<'a> = Name;

    fn finish(self) -> Builder {
        self
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> usize {
        0
    }

    fn elem_name<'a>(&'a self, target: &'a usize) -> Name {
        match &self.nodes.borrow()[*target].data {
            Data::Element { name, .. } => Name(name.clone()),
            _ => Name(div()),
        }
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> usize {
        let cost: usize = attrs.iter().map(|a| a.name.local.len() + a.value.len() + 16).sum();
        let template = if flags.template { self.push(Data::Other, 0) } else { NONE };
        self.push(Data::Element { name, attrs, template }, cost as u64)
    }

    fn create_comment(&self, _text: StrTendril) -> usize {
        self.push(Data::Other, 0)
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> usize {
        self.push(Data::Other, 0)
    }

    fn append(&self, parent: &usize, child: NodeOrText<usize>) {
        self.insert(*parent, NONE, child);
    }

    fn append_based_on_parent_node(&self, element: &usize, prev_element: &usize, child: NodeOrText<usize>) {
        let has_parent = self.nodes.borrow()[*element].parent != NONE;
        if has_parent { self.append_before_sibling(element, child) } else { self.append(prev_element, child) }
    }

    fn append_doctype_to_document(&self, _name: StrTendril, _public_id: StrTendril, _system_id: StrTendril) {}

    fn get_template_contents(&self, target: &usize) -> usize {
        match &self.nodes.borrow()[*target].data {
            Data::Element { template, .. } if *template != NONE => *template,
            _ => DETACHED,
        }
    }

    fn same_node(&self, x: &usize, y: &usize) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &usize, new_node: NodeOrText<usize>) {
        let parent = self.nodes.borrow()[*sibling].parent;
        if parent != NONE {
            self.insert(parent, *sibling, new_node);
        }
    }

    fn add_attrs_if_missing(&self, target: &usize, attrs: Vec<Attribute>) {
        if *target == DETACHED || self.stop.get() {
            return;
        }
        let cost: usize = attrs.iter().map(|a| a.name.local.len() + a.value.len() + 16).sum();
        if !self.charge(cost as u64) {
            return;
        }
        if let Data::Element { attrs: have, .. } = &mut self.nodes.borrow_mut()[*target].data {
            for a in attrs {
                if !have.iter().any(|h| h.name == a.name) {
                    have.push(a);
                }
            }
        }
    }

    fn remove_from_parent(&self, target: &usize) {
        if *target != DETACHED {
            Self::detach(&mut self.nodes.borrow_mut(), *target);
        }
    }

    fn reparent_children(&self, node: &usize, new_parent: &usize) {
        if *node == DETACHED || *new_parent == DETACHED {
            return;
        }
        let mut n = self.nodes.borrow_mut();
        let mut c = n[*node].first;
        while c != NONE {
            let next = n[c].next;
            Self::attach(&mut n, *new_parent, c, NONE);
            c = next;
        }
    }
}

/// Parses `html`, charging what it builds to `held`: an error (the breaker's reason) once the
/// page costs more than the breaker allows, nests deeper than [`MAX_DEPTH`], or is still parsing
/// at `deadline`. The charge stays on `held` while the DOM lives.
pub fn parse(html: &str, held: Reservation, deadline: std::time::Instant) -> Result<(Dom, Reservation), String> {
    let nodes = vec![Node::new(Data::Document), Node::new(Data::Other)];
    let builder = Builder { nodes: RefCell::new(nodes), held: RefCell::new(held), tripped: RefCell::new(None), stop: Cell::new(false) };
    let mut opts = html5ever::ParseOpts::default();
    // As Jsoup parses: `<noscript>` holds markup, not text.
    opts.tree_builder.scripting_enabled = false;
    let mut parser = html5ever::parse_document(builder, opts);
    let mut rest = html;
    while !rest.is_empty() {
        if std::time::Instant::now() >= deadline {
            parser.tokenizer.sink.sink.fail("the page took too long to parse".into());
            break;
        }
        let mut at = rest.len().min(4 << 10);
        while !rest.is_char_boundary(at) {
            at += 1;
        }
        parser.process(StrTendril::from_slice(&rest[..at]));
        rest = &rest[at..];
        if parser.tokenizer.sink.sink.stop.get() {
            break;
        }
    }
    let b = parser.finish();
    if let Some(why) = b.tripped.into_inner() {
        return Err(why);
    }
    Ok((Dom { nodes: b.nodes.into_inner() }, b.held.into_inner()))
}

/// What a page says, as Open Crawler extracts it.
#[derive(Debug, Default)]
pub struct Page {
    pub title: String,
    pub body: String,
    pub keywords: String,
    pub description: String,
    /// The `links` field: up to `max_indexed_links_count`, sorted.
    pub links: Vec<String>,
    /// Links to follow (not `rel=nofollow`), up to `max_extracted_links_count`, in page order.
    pub follow: Vec<Link>,
    pub headings: Vec<String>,
    pub canonical: Option<Link>,
    pub noindex: bool,
    pub nofollow: bool,
    /// `<meta class="elastic">` and `data-elastic-name` fields.
    pub fields: Vec<(String, String)>,
}

/// Fields Open Crawler's documents always have (`Constants::RESERVED_FIELD_NAMES`): a page's
/// own meta tags can't set them.
const RESERVED: &[&str] = &[
    "id", "any", "all", "none", "or", "and", "not", "additional_urls", "body_content", "body", "domains", "headings", "last_crawled_at", "links", "meta_description", "meta_keywords", "title",
    "url", "url_host", "url_path", "url_path_dir1", "url_path_dir2", "url_path_dir3", "url_port", "url_scheme", "full_html", "_id",
];
const MAX_FIELDS: usize = 100;
/// Tags whose content isn't text (Open Crawler's `NON_CONTENT_TAGS`).
const NON_CONTENT: &[&str] = &["object", "script", "style", "svg", "video", "template"];

fn valid_field(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.starts_with('_') && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') && !RESERVED.contains(&name)
}

/// Ruby's `squish`: trimmed, every run of whitespace one space.
pub fn squish(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Open Crawler's `limit_bytesize`: cut on a grapheme to `limit` bytes, `…` included.
pub fn limit_bytes(s: String, limit: usize) -> String {
    if s.len() <= limit {
        return s;
    }
    let real = limit.saturating_sub('…'.len_utf8());
    let mut cut = String::with_capacity(limit);
    for g in s.graphemes(true) {
        if cut.len() + g.len() > real {
            break;
        }
        cut.push_str(g);
    }
    cut.push('…');
    cut
}

impl Dom {
    fn element(&self, i: usize) -> Option<(&str, &[Attribute])> {
        match &self.nodes[i].data {
            Data::Element { name, attrs, .. } => Some((&*name.local, attrs.as_slice())),
            _ => None,
        }
    }

    fn attr<'a>(&'a self, i: usize, key: &str) -> Option<&'a str> {
        self.element(i)?.1.iter().find(|a| &*a.name.local == key).map(|a| &*a.value)
    }

    /// Every node to [`MAX_DEPTH`], in document order, with its depth.
    fn walk(&self, from: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut stack = vec![(from, 0usize)];
        while let Some((i, d)) = stack.pop() {
            out.push((i, d));
            if d >= MAX_DEPTH {
                continue;
            }
            let mut kids = Vec::new();
            let mut c = self.nodes[i].first;
            while c != NONE {
                kids.push(c);
                c = self.nodes[c].next;
            }
            stack.extend(kids.into_iter().rev().map(|k| (k, d + 1)));
        }
        out
    }

    /// Open Crawler's `node_descendant_text` of node `from`: spaces around elements, `br` and
    /// `hr` a space, text squished, an element's `content` attribute counted, non-content tags
    /// and `skip` tags left out; text only where the nearest `data-elastic-include` or `-exclude`
    /// ancestor includes it when `transform` (`ContentEngine::Transformer`). Stops gathering
    /// past `max` bytes.
    fn text(&self, from: usize, skip: &[String], transform: bool, max: usize) -> String {
        enum Step {
            Node(usize, usize, bool),
            Space,
        }
        let mut out = String::new();
        let mut stack = vec![Step::Node(from, 0, true)];
        let push = |out: &mut String, s: &str| {
            let s = squish(s);
            if !s.is_empty() {
                out.push_str(&s);
            }
        };
        while let Some(step) = stack.pop() {
            if out.len() > max + 1024 {
                break;
            }
            let (i, depth, include) = match step {
                Step::Space => {
                    if !out.ends_with(' ') {
                        out.push(' ');
                    }
                    continue;
                }
                Step::Node(i, d, inc) => (i, d, inc),
            };
            match &self.nodes[i].data {
                Data::Text(t) => {
                    if include {
                        push(&mut out, t);
                    }
                    continue;
                }
                Data::Element { name, attrs, .. } => {
                    let tag: &str = &name.local;
                    if NON_CONTENT.contains(&tag) || skip.iter().any(|s| s.as_str() == tag) {
                        continue;
                    }
                    if matches!(tag, "br" | "hr") {
                        if !out.ends_with(' ') {
                            out.push(' ');
                        }
                        continue;
                    }
                    if let Some(c) = attrs.iter().find(|a| &*a.name.local == "content") {
                        push(&mut out, &c.value);
                    }
                }
                Data::Document => {}
                Data::Other => continue,
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            let mode = |c: usize| match transform {
                true if self.attr(c, "data-elastic-include").is_some() => true,
                true if self.attr(c, "data-elastic-exclude").is_some() => false,
                _ => include,
            };
            stack.push(Step::Space);
            let mut kids = Vec::new();
            let mut c = self.nodes[i].first;
            while c != NONE {
                kids.push(c);
                c = self.nodes[c].next;
            }
            for k in kids.into_iter().rev() {
                stack.push(Step::Node(k, depth + 1, mode(k)));
            }
            stack.push(Step::Space);
        }
        squish(&out)
    }

    /// The page at `url`, read within `limits`; `exclude_tags` are the domain's.
    pub fn read(&self, url: &Link, exclude_tags: &[String], limits: &Limits) -> Page {
        let mut p = Page::default();
        let all = self.walk(0);
        let tag_is = |i: usize, t: &str| self.element(i).is_some_and(|(n, _)| n == t);
        let lower = |s: Option<&str>| s.map(|v| v.to_ascii_lowercase()).unwrap_or_default();
        let body = all.iter().map(|(i, _)| *i).find(|i| tag_is(*i, "body"));
        let mut base: Option<reqwest::Url> = None;
        let mut title: Option<usize> = None;
        let mut keywords: Option<String> = None;
        let mut description: Option<String> = None;
        let mut canonical: Option<String> = None;
        let mut hrefs: Vec<(String, bool)> = Vec::new();
        let max_links = limits.usize(L::ExtractedLinks);
        for (i, _) in &all {
            let Some((tag, _)) = self.element(*i) else { continue };
            match tag {
                "title" if title.is_none() => title = Some(*i),
                "base" if base.is_none() => base = self.attr(*i, "href").and_then(|h| url.url().join(h.trim()).ok()),
                "link" if canonical.is_none() && lower(self.attr(*i, "rel")) == "canonical" => canonical = self.attr(*i, "href").filter(|h| !h.trim().is_empty()).map(str::to_string),
                "a" => {
                    if let Some(h) = self.attr(*i, "href")
                        && hrefs.len() < max_links
                    {
                        let nofollow = lower(self.attr(*i, "rel")).split_ascii_whitespace().any(|r| r == "nofollow");
                        hrefs.push((h.to_string(), nofollow));
                    }
                }
                "meta" => {
                    let name = lower(self.attr(*i, "name"));
                    let content = self.attr(*i, "content");
                    match name.as_str() {
                        "robots" => {
                            let c = lower(content);
                            p.noindex |= c.contains("noindex");
                            p.nofollow |= c.contains("nofollow");
                        }
                        "keywords" if keywords.is_none() => keywords = content.map(str::to_string),
                        "description" if description.is_none() => description = content.map(str::to_string),
                        _ => {}
                    }
                    let elastic = lower(self.attr(*i, "class")).split_ascii_whitespace().any(|c| c == "elastic");
                    if elastic
                        && let (Some(n), Some(c)) = (self.attr(*i, "name"), content)
                        && valid_field(n)
                    {
                        let v = limit_bytes(c.to_string(), limits.usize(L::ElasticTagSize));
                        set_field(&mut p.fields, n, v);
                    }
                }
                _ => {}
            }
        }
        p.title = title.map(|t| limit_bytes(self.text(t, &[], false, limits.usize(L::TitleSize)), limits.usize(L::TitleSize))).unwrap_or_default();
        p.keywords = keywords.map(|k| limit_bytes(k, limits.usize(L::KeywordsSize))).unwrap_or_default();
        p.description = description.map(|d| limit_bytes(d, limits.usize(L::DescriptionSize))).unwrap_or_default();
        p.canonical = canonical.and_then(|c| Link::parse(&c, Some(url.url())));
        let base = base.unwrap_or_else(|| url.url().clone());
        let mut indexed = std::collections::BTreeSet::new();
        let max_indexed = limits.usize(L::IndexedLinks);
        for (h, nofollow) in hrefs {
            let Some(l) = Link::parse(&h, Some(&base)) else { continue };
            if indexed.len() < max_indexed {
                indexed.insert(l.as_str().to_string());
            }
            if !nofollow {
                p.follow.push(l);
            }
        }
        p.links = indexed.into_iter().collect();
        if let Some(b) = body {
            let max_body = limits.usize(L::BodySize);
            p.body = limit_bytes(self.text(b, exclude_tags, true, max_body), max_body);
            let max_headings = limits.usize(L::Headings);
            for (i, _) in self.walk(b) {
                let Some((tag, _)) = self.element(i) else { continue };
                if matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") && p.headings.len() < max_headings {
                    let h = self.text(i, &[], false, 4096);
                    if !h.is_empty() && !p.headings.contains(&h) {
                        p.headings.push(h);
                    }
                }
                if let Some(n) = self.attr(i, "data-elastic-name")
                    && valid_field(n)
                {
                    let v = limit_bytes(self.text(i, &[], false, 4096), limits.usize(L::ElasticTagSize));
                    set_field(&mut p.fields, n, v);
                }
            }
        }
        p
    }
}

fn set_field(fields: &mut Vec<(String, String)>, name: &str, v: String) {
    match fields.iter().position(|(n, _)| n == name) {
        Some(i) => fields[i].1 = v,
        None if fields.len() < MAX_FIELDS => fields.push((name.to_string(), v)),
        None => {}
    }
}

/// Parses `html` (charged to `held`) and reads it as the page at `url`. The DOM never outlives
/// this call: its strings aren't `Send`, so it can't be held across a crawl's awaits.
pub fn read_page(html: &str, held: Reservation, url: &Link, exclude_tags: &[String], limits: &Limits, deadline: std::time::Instant) -> Result<(Page, Reservation), String> {
    let (dom, held) = parse(html, held, deadline)?;
    Ok((dom.read(url, exclude_tags, limits), held))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(html: &str, exclude: &[&str]) -> Page {
        let budget = search_aggs::memory::MemoryBudget::new(u64::MAX);
        let (dom, _held) = parse(html, Reservation::new(budget), std::time::Instant::now() + std::time::Duration::from_secs(30)).unwrap();
        let limits = super::super::config::plan(
            &super::super::config::Fields { urls: vec!["https://e.com/".into()], sitemap_urls: vec![], sitemap_discovery: true, max_crawl_depth: None, max_unique_url_count: None },
            None,
            &super::super::config::Ceilings::new(|_| None, 3_600_000),
        )
        .unwrap()
        .limits;
        let tags: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
        dom.read(&Link::parse("https://e.com/docs/page", None).unwrap(), &tags, &limits)
    }

    /// Open Crawler's directives and fields: robots meta, rel=nofollow, canonical, base,
    /// data-elastic-include/exclude (nestable), meta.elastic, exclude_tags, headings.
    #[test]
    fn directives_and_fields() {
        let p = read(
            r#"<html><head><title> A   page </title><base href="/base/">
            <meta name="Robots" content="NOINDEX, follow"><meta name="description" content="About it">
            <meta class="elastic x" name="product_price" content="9.99"><meta class="elastic" name="title" content="no">
            <link rel="canonical" href="https://e.com/canon"><script>var secret = 1;</script></head>
            <body><header>Site nav</header><h1>Head one</h1><p>Hello<br>world <b>bold</b></p>
            <div data-elastic-exclude>hidden <span data-elastic-include>shown again</span></div>
            <a href="next">Next</a><a rel="external nofollow" href="https://e.com/nf">nf</a><a href="mailto:x@e.com">m</a>
            <span data-elastic-name="sku">AB-1</span><style>.x{}</style></body></html>"#,
            &["header"],
        );
        assert_eq!(p.title, "A page");
        assert_eq!(p.body, "Head one Hello world bold shown again Next nf m AB-1");
        assert!(p.noindex && !p.nofollow);
        assert_eq!(p.description, "About it");
        assert_eq!(p.canonical.as_ref().map(Link::as_str), Some("https://e.com/canon"));
        assert_eq!(p.follow.iter().map(Link::as_str).collect::<Vec<_>>(), ["https://e.com/base/next"], "nofollow and mailto aren't followed");
        assert_eq!(p.links, ["https://e.com/base/next", "https://e.com/nf"]);
        assert_eq!(p.headings, ["Head one"]);
        assert_eq!(p.fields, [("product_price".to_string(), "9.99".to_string()), ("sku".to_string(), "AB-1".to_string())], "a reserved name isn't set");
        let nf = read(r#"<meta name="robots" content="nofollow"><a href="/x">x</a>"#, &[]);
        assert!(nf.nofollow && !nf.noindex);
        assert_eq!(limit_bytes("héllo world".into(), 7), "hél…");
    }

    /// html5ever scans its open elements for each element it opens: a 10 MB page of two million
    /// nested `<div>`s (hours of work uncapped) fails at MAX_DEPTH in a moment, while a page
    /// nested to just under it reads.
    #[test]
    fn deep_pages_fail_fast() {
        let budget = search_aggs::memory::MemoryBudget::new(u64::MAX);
        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_secs(60);
        let err = parse(&"<div>".repeat(2_000_000), Reservation::new(budget.clone()), deadline).err().unwrap();
        assert!(err.contains("more than 512 deep") && started.elapsed() < std::time::Duration::from_secs(10), "{err} after {:?}", started.elapsed());
        let p = read(&format!("<p>top</p>{}deep", "<div>".repeat(MAX_DEPTH - 8)), &[]);
        assert_eq!(p.body, "top deep");
    }
}
