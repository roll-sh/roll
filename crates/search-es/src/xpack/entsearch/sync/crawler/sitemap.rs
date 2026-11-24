//! Sitemaps (sitemaps.org): a `<urlset>` of pages or a `<sitemapindex>` of sitemaps, read with
//! `quick-xml`, which builds no tree. A document with a DOCTYPE isn't read at all, so no entity
//! is ever declared, let alone expanded; only XML's five predefined entities and character
//! references are resolved in a `<loc>`. Gzipped sitemaps are decompressed within the response
//! cap.

use quick_xml::events::Event;

/// The most `<loc>` entries one sitemap gives (the protocol's own limit).
pub const MAX_ENTRIES: usize = 50_000;

#[derive(Debug, PartialEq, Eq)]
pub enum Sitemap {
    Pages(Vec<String>),
    Index(Vec<String>),
}

/// `body`, gunzipped when it is gzip (by its magic bytes), up to `max` bytes.
pub fn gunzip(body: Vec<u8>, max: usize) -> Result<Vec<u8>, String> {
    if !body.starts_with(&[0x1f, 0x8b]) {
        return Ok(body);
    }
    let mut out = super::fetch::Inflate::gzip(max);
    out.write(&body)?;
    out.finish()
}

pub fn parse(xml: &[u8]) -> Result<Sitemap, String> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut index: Option<bool> = None;
    let mut locs = Vec::new();
    let mut in_loc = false;
    let mut loc = String::new();
    // A `<loc>` holding an entity other than XML's own is dropped.
    let mut bad = false;
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(|e| format!("the sitemap isn't XML: {e}"))?;
        match ev {
            Event::DocType(_) => return Err("the sitemap has a DOCTYPE, which roll's crawler doesn't read".into()),
            Event::Start(e) => match e.local_name().as_ref() {
                b"urlset" if index.is_none() => index = Some(false),
                b"sitemapindex" if index.is_none() => index = Some(true),
                b"loc" => {
                    in_loc = true;
                    loc.clear();
                    bad = false;
                }
                _ => {}
            },
            Event::Text(t) if in_loc => loc.push_str(&t.decode().map_err(|e| e.to_string())?),
            Event::CData(t) if in_loc => loc.push_str(&String::from_utf8_lossy(&t)),
            Event::GeneralRef(r) if in_loc => match r.resolve_char_ref() {
                Ok(Some(c)) => loc.push(c),
                _ => match r.as_ref() {
                    b"amp" => loc.push('&'),
                    b"lt" => loc.push('<'),
                    b"gt" => loc.push('>'),
                    b"quot" => loc.push('"'),
                    b"apos" => loc.push('\''),
                    _ => bad = true,
                },
            },
            Event::End(e) if e.local_name().as_ref() == b"loc" => {
                in_loc = false;
                if !bad && !loc.trim().is_empty() && locs.len() < MAX_ENTRIES {
                    locs.push(loc.trim().to_string());
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    match index {
        Some(true) => Ok(Sitemap::Index(locs)),
        Some(false) => Ok(Sitemap::Pages(locs)),
        None => Err("the sitemap is neither a <urlset> nor a <sitemapindex>".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sitemaps_and_indexes() {
        let set = br#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc> https://e.com/a?x=1&amp;y=&#50; </loc><lastmod>2026-01-01</lastmod></url><url><loc><![CDATA[https://e.com/b]]></loc></url></urlset>"#;
        assert_eq!(parse(set).unwrap(), Sitemap::Pages(vec!["https://e.com/a?x=1&y=2".into(), "https://e.com/b".into()]));
        let idx = br#"<sitemapindex><sitemap><loc>https://e.com/s1.xml.gz</loc></sitemap></sitemapindex>"#;
        assert_eq!(parse(idx).unwrap(), Sitemap::Index(vec!["https://e.com/s1.xml.gz".into()]));
        // An undeclared entity drops its entry; a DOCTYPE drops the sitemap.
        assert_eq!(parse(b"<urlset><url><loc>https://e.com/&x;</loc></url></urlset>").unwrap(), Sitemap::Pages(vec![]));
        let laughs = br#"<?xml version="1.0"?><!DOCTYPE urlset [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;">]><urlset><url><loc>https://e.com/&b;</loc></url></urlset>"#;
        assert!(parse(laughs).unwrap_err().contains("DOCTYPE"));
    }
}
