//! Unpacking a bundle whose tree is hostile (pushed trees can repeat names and hold symlinks):
//! every bundle here is refused, and nothing is written outside the tree.

use roll_fetch::{Limits, VERSION, unpack};
use std::io::Write;
use std::path::Path;

enum E<'a> {
    File(&'a str, &'a [u8]),
    Dir(&'a str),
    Link(&'a str, &'a str),
    Hard(&'a str, &'a str),
    /// Any entry type with a body.
    Raw(&'a str, tar::EntryType, &'a [u8]),
}

/// A bundle written header by header, names taken as they are (the tar crate's builder refuses
/// `..` and absolute paths, so they're set in the raw header).
fn bundle(entries: &[E]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut put = |name: &str, kind: tar::EntryType, link: &str, data: &[u8]| {
        let mut h = tar::Header::new_old();
        let raw = &mut h.as_old_mut().name;
        raw[..name.len()].copy_from_slice(name.as_bytes());
        let l = &mut h.as_old_mut().linkname;
        l[..link.len()].copy_from_slice(link.as_bytes());
        h.set_entry_type(kind);
        h.set_mode(0o644);
        h.set_size(data.len() as u64);
        h.set_cksum();
        b.append(&h, data).unwrap();
    };
    put(VERSION, tar::EntryType::Regular, "", b"");
    put("env", tar::EntryType::Regular, "", b"export A='1'\n");
    for e in entries {
        match e {
            E::File(n, d) => put(n, tar::EntryType::Regular, "", d),
            E::Dir(n) => put(n, tar::EntryType::Directory, "", b""),
            E::Link(n, t) => put(n, tar::EntryType::Symlink, t, b""),
            E::Hard(n, t) => put(n, tar::EntryType::Link, t, b""),
            E::Raw(n, k, d) => put(n, *k, "", d),
        }
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&b.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

fn try_unpack(entries: &[E], limits: Limits) -> (anyhow::Result<roll_fetch::Unpacked>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    let r = unpack(bundle(entries).as_slice(), &tree, &dir.path().join("env"), limits);
    (r, dir)
}

#[test]
fn hostile_bundles_are_refused_and_nothing_lands_outside_the_tree() {
    let outside = tempfile::tempdir().unwrap();
    let out = outside.path().to_str().unwrap().to_string();
    let escape = format!("{out}/x");
    let cases: Vec<(&str, Vec<E>)> = vec![
        ("a repeated file", vec![E::File("tree/a", b"1"), E::File("tree/a", b"2")]),
        ("a file over a directory", vec![E::Dir("tree/d"), E::File("tree/d", b"x")]),
        ("a file over a symlink", vec![E::Link("tree/l", &escape), E::File("tree/l", b"x")]),
        ("a write through a symlinked directory", vec![E::Link("tree/l", &out), E::File("tree/l/x", b"x")]),
        ("a directory through a symlink", vec![E::Link("tree/l", &out), E::Dir("tree/l/d")]),
        ("a symlink through a symlink", vec![E::Link("tree/l", &out), E::Link("tree/l/x", "/etc/passwd")]),
        ("..", vec![E::File("tree/../x", b"x")]),
        ("an absolute path", vec![E::File(&escape, b"x")]),
        ("outside tree/", vec![E::File("x", b"x")]),
        ("a hard link", vec![E::File("tree/a", b"1"), E::Hard("tree/b", &escape)]),
        ("a second env", vec![E::File("env", b"export B='2'\n")]),
        ("a long name over PATH_MAX", vec![E::Raw("././@LongLink", tar::EntryType::GNULongName, &[b'a'; 8192]), E::File("tree/x", b"x")]),
        ("a PAX header", vec![E::Raw("pax", tar::EntryType::XHeader, b"30 path=tree/aaaaaaaaaaaaaaaa\n"), E::File("tree/x", b"x")]),
        ("a symlink with a body", vec![E::Raw("tree/l", tar::EntryType::Symlink, b"padding")]),
        ("a dangling long name", vec![E::Raw("././@LongLink", tar::EntryType::GNULongName, b"tree/x\0")]),
    ];
    for (what, entries) in cases {
        let (r, _dir) = try_unpack(&entries, Limits::default());
        assert!(r.is_err(), "{what} was accepted");
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none(), "{what} wrote outside the tree");
    }
    // A symlink pointing out is kept as a symlink (the customer's container sees it), never followed.
    let (r, dir) = try_unpack(&[E::Link("tree/l", &out), E::File("tree/ok", b"fine")], Limits::default());
    r.unwrap();
    assert_eq!(std::fs::read_link(dir.path().join("tree/l")).unwrap(), Path::new(&out));
    // Sizes are bounded by the bytes read, per file and in all.
    let small = Limits { max_bytes: 10, max_file_bytes: 6, max_entries: 3 };
    assert!(try_unpack(&[E::File("tree/a", b"1234567")], small).0.is_err(), "a file over the limit");
    assert!(try_unpack(&[E::File("tree/a", b"123456"), E::File("tree/b", b"123456")], small).0.is_err(), "a tree over the limit");
    assert!(try_unpack(&[E::Dir("tree/a"), E::Dir("tree/b"), E::Dir("tree/c"), E::Dir("tree/d")], small).0.is_err(), "too many entries");
    assert!(try_unpack(&[E::File("tree/a", b"123456")], small).0.is_ok());
}
