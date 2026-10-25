//! The hyphenation range tables of isn (`EAN13.h`, `ISBN.h`, `ISMN.h`, `ISSN.h`, `UPC.h`,
//! contrib/isn at REL_18_6, in `data/`), read from the headers themselves at first use.

use std::sync::OnceLock;

/// A range table: per leading digit, (first entry, count); the entries' (low, high) bounds.
pub(super) struct Table {
    pub index: [[u32; 2]; 10],
    pub range: Vec<[String; 2]>,
}

const EAN13_H: &str = include_str!("data/EAN13.h");
const ISBN_H: &str = include_str!("data/ISBN.h");
const ISMN_H: &str = include_str!("data/ISMN.h");
const ISSN_H: &str = include_str!("data/ISSN.h");
const UPC_H: &str = include_str!("data/UPC.h");

fn strip_comments(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        rest = rest[i..].find("*/").map_or("", |j| &rest[i + j + 2..]);
    }
    out.push_str(rest);
    out
}

/// The initializer of `decl` (up to its `};`), comments removed.
fn body(src: &str, decl: &str) -> String {
    let src = strip_comments(src);
    let at = src.find(decl).unwrap_or_else(|| panic!("isn table {decl}")) + decl.len();
    let rest = &src[at..];
    rest[..rest.find("};").unwrap_or(rest.len())].to_string()
}

fn parse(src: &str, index_name: &str, range_name: &str) -> Table {
    let idx = body(src, &format!("{index_name}[10][2] = {{"));
    let nums: Vec<u32> = idx
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
        .map(|t| t.parse().unwrap())
        .collect();
    let mut index = [[0u32; 2]; 10];
    for (i, pair) in nums.chunks(2).take(10).enumerate() {
        index[i] = [pair[0], pair[1]];
    }
    // Quoted bounds in pairs (the table ends with {NULL, NULL}).
    let r = body(src, &format!("{range_name}[][2] = {{"));
    let strs: Vec<String> = r.split('"').skip(1).step_by(2).map(str::to_string).collect();
    let range = strs.chunks(2).map(|p| [p[0].clone(), p[1].clone()]).collect();
    Table { index, range }
}

macro_rules! table {
    ($f:ident, $src:expr, $index:expr, $range:expr) => {
        pub(super) fn $f() -> &'static Table {
            static T: OnceLock<Table> = OnceLock::new();
            T.get_or_init(|| parse($src, $index, $range))
        }
    };
}

table!(ean13, EAN13_H, "EAN13_index", "EAN13_range");
table!(isbn, ISBN_H, "ISBN_index", "ISBN_range");
table!(isbn_new, ISBN_H, "ISBN_index_new", "ISBN_range_new");
table!(ismn, ISMN_H, "ISMN_index", "ISMN_range");
table!(issn, ISSN_H, "ISSN_index", "ISSN_range");
table!(upc, UPC_H, "UPC_index", "UPC_range");

#[cfg(test)]
mod tests {
    #[test]
    fn tables_parse() {
        let t = super::isbn();
        assert_eq!(t.index[9], [192, 718]);
        assert_eq!(t.range[0], ["0-00".to_string(), "0-19".to_string()]);
        assert_eq!(t.range.len(), 910);
        assert!(super::upc().range.is_empty());
        assert_eq!(super::ean13().range.last().unwrap()[1], "999");
    }
}
