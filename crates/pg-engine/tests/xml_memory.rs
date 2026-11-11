//! Peak heap memory of the XML entry points on a 1 MB document stays at or below what
//! PostgreSQL 18.6's backend grows by for the same statement (BENCH.md, "XML"): every XML
//! function shares the parser and tree (`types/xml`), whose node size once made parsing that
//! document peak at about 200 MB. A counting allocator measures each statement's peak over
//! what was allocated before it.
//!
//!     cargo test -p pg-engine --test xml_memory -- --nocapture   (prints the peaks)

mod common;

use common::{Event, engine_query, engine_session, open_store};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Counting;

static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let n = CUR.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(n, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CUR.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new > l.size() {
                let n = CUR.fetch_add(new - l.size(), Relaxed) + new - l.size();
                // a moving realloc holds both blocks for a moment
                PEAK.fetch_max(n + l.size().min(new), Relaxed);
            } else {
                CUR.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

const MB: f64 = 1048576.0;

/// (statement, PostgreSQL's peak in MB for it at 1 MB, from `tools/bench/xml.sh`).
const CASES: &[(&str, f64)] = &[
    ("SELECT t::xml IS NOT NULL FROM d", 22.6),
    ("SELECT xmlparse(document t) IS NOT NULL FROM d", 17.3),
    ("SELECT length(xmlserialize(document x AS text)) FROM d", 20.0),
    ("SELECT length(xmlserialize(document x AS text indent)) FROM d", 21.8),
    ("SELECT array_length(xpath('/rows/row/a/text()', x), 1) FROM d", 20.5),
    ("SELECT (xpath('count(/rows/row)', x))[1]::text FROM d", 20.5),
    ("SELECT xpath_exists('/rows/row[b = 5]', x) FROM d", 20.5),
    (
        "SELECT count(*), sum(r.b) FROM d, xmltable('/rows/row' PASSING x \
         COLUMNS id int PATH '@id', a text PATH 'a', b int PATH 'b') r",
        20.5,
    ),
    ("SELECT length(xpath_string(t, '/rows/row[last()]/a')) FROM d", 20.5),
    ("SELECT length(xpath_list(t, '/rows/row/b')) FROM d", 20.5),
    ("SELECT xpath_number(t, 'count(/rows/row)') FROM d", 20.5),
    ("SELECT length(xslt_process(t, (SELECT s FROM st))) FROM d", 57.0),
    (
        "SELECT length(xmlelement(name rows, xmlagg(xmlelement(name row, \
         xmlattributes(i AS id), xmlforest(md5(i::text) AS a, i * 2 AS b))))::text) \
         FROM generate_series(1, 11600) i",
        5.0,
    ),
];

#[test]
fn xml_peak_memory() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "xml_memory");
    for q in [
        "CREATE EXTENSION xml2",
        "CREATE TABLE d (t text, x xml)",
        "INSERT INTO d SELECT s, s::xml FROM (SELECT '<rows>' || string_agg(format(\
         '<row id=\"%s\"><a>%s</a><b>%s</b><c>x &amp; y</c></row>', i, md5(i::text), i) \
         || chr(10), '' ORDER BY i) || '</rows>' s FROM generate_series(1, 11600) i) q",
        "CREATE TABLE st (s text)",
        "INSERT INTO st VALUES ('<xsl:stylesheet version=\"1.0\" \
         xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\"><xsl:template match=\"@*|node()\">\
         <xsl:copy><xsl:apply-templates select=\"@*|node()\"/></xsl:copy></xsl:template>\
         </xsl:stylesheet>')",
    ] {
        engine_query(&mut s, q);
    }
    let mut over = Vec::new();
    for &(q, pg) in CASES {
        engine_query(&mut s, q); // warm the plan and catalog caches
        PEAK.store(CUR.load(Relaxed), Relaxed);
        let before = CUR.load(Relaxed);
        let ev = engine_query(&mut s, q);
        let peak = (PEAK.load(Relaxed) - before) as f64 / MB;
        println!("{peak:7.1} MB (PostgreSQL {pg:5.1})  {q}");
        assert!(!ev.iter().any(|e| matches!(e, Event::Error(_))), "{q} failed");
        if peak > pg {
            over.push(format!("{q}: {peak:.1} MB > {pg} MB"));
        }
    }
    assert!(over.is_empty(), "over PostgreSQL's peak:\n{}", over.join("\n"));
}
