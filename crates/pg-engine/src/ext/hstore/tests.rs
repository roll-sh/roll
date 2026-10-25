use super::io::{input, output};
use super::{Hs, hstore_cmp};
use std::cmp::Ordering;

fn roundtrip(s: &str) -> String {
    output(&input(s).unwrap())
}

#[test]
fn parse_and_print() {
    assert_eq!(roundtrip(""), "");
    assert_eq!(roundtrip(" a=>b"), "\"a\"=>\"b\"");
    assert_eq!(roundtrip("aa=>bb , \"cc\"=>dd"), "\"aa\"=>\"bb\", \"cc\"=>\"dd\"");
    assert_eq!(roundtrip("aa=>null"), "\"aa\"=>NULL");
    assert_eq!(roundtrip("aa=>\"NuLl\""), "\"aa\"=>\"NuLl\"");
    assert_eq!(roundtrip("\\=a=>q=w"), "\"=a\"=>\"q=w\"");
    assert_eq!(roundtrip("\"\\\"a\"=>q>w"), "\"\\\"a\"=>\"q>w\"");
    // Sorted by length, then bytes.
    assert_eq!(roundtrip("b=>1, aa=>2, a=>3"), "\"a\"=>\"3\", \"b\"=>\"1\", \"aa\"=>\"2\"");
}

#[test]
fn errors() {
    let e = input("  =>null").unwrap_err();
    assert_eq!(e.message, "syntax error in hstore, near \"=\" at position 2");
    let e = input("aa=>\"").unwrap_err();
    assert_eq!(e.message, "syntax error in hstore: unexpected end of string");
    let e = input("a=b").unwrap_err();
    assert_eq!(e.message, "syntax error in hstore, near \"b\" at position 2");
}

#[test]
fn layout_and_order() {
    let b = input("a=>NULL, bb=>x").unwrap();
    // size_ word, four HEntry words, then "a" "bb" "x".
    assert_eq!(b.len(), 4 + 16 + 4);
    let h = Hs::new(&b);
    assert_eq!(h.count(), 2);
    assert_eq!(h.val(0), None);
    assert_eq!(h.val(1), Some(&b"x"[..]));
    assert_eq!(hstore_cmp(&input("").unwrap(), &b), Ordering::Less);
    assert_eq!(hstore_cmp(&b, &b), Ordering::Equal);
    assert_eq!(hstore_cmp(&input("a=>1").unwrap(), &input("a=>2").unwrap()), Ordering::Less);
}
