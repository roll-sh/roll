//! Pack reading: packs git writes (OFS and REF deltas, thin packs) and hostile input. A global
//! allocator fails the test if anything allocates more than 256 MiB at once.

mod common;

use code_git::pack::{self, Limits, PackError, Resolver};
use code_git::{Kind, Object, ObjectId};
use common::git;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use sha1::{Digest, Sha1};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

struct Guard;

const MAX_ALLOC: usize = 256 << 20;

unsafe impl GlobalAlloc for Guard {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        assert!(l.size() <= MAX_ALLOC, "allocation of {} bytes", l.size());
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        assert!(n <= MAX_ALLOC, "reallocation to {n} bytes");
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Guard = Guard;

fn pack_objects(dir: &Path, args: &[&str], input: &str) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("pack-objects")
        .args(args)
        .arg("--stdout")
        .current_dir(dir)
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    out.stdout
}

fn cat_file(dir: &Path, id: &ObjectId) -> Object {
    let kind = git(dir, dir, &["cat-file", "-t", &id.to_string()]);
    let out = Command::new("git").args(["cat-file", kind.trim(), &id.to_string()]).current_dir(dir).env("HOME", dir).output().unwrap();
    Object::new(Kind::parse(kind.trim().as_bytes()).unwrap(), out.stdout)
}

/// A repository where a large file changes a little per commit, so git writes deltas.
fn repo_with_deltas(dir: &Path) {
    git(dir, dir, &["init", "-q", "."]);
    let mut text: String = (0..4000).map(|i| format!("line number {i} of a file that changes slowly\n")).collect();
    for i in 0..6 {
        text.push_str(&format!("appended {i}\n"));
        std::fs::write(dir.join("big.txt"), &text).unwrap();
        std::fs::write(dir.join(format!("small{i}.txt")), format!("{i}\n")).unwrap();
        git(dir, dir, &["add", "-A"]);
        git(dir, dir, &["commit", "-qm", &format!("c{i}")]);
    }
}

#[test]
fn reads_git_packs_with_deltas() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    repo_with_deltas(dir);
    let listed: Vec<ObjectId> = git(dir, dir, &["rev-list", "--objects", "--all"])
        .lines()
        .map(|l| l.split(' ').next().unwrap().parse().unwrap())
        .collect();

    for (args, ofs) in [(&["--revs", "--all", "--delta-base-offset"][..], true), (&["--revs", "--all"][..], false)] {
        let data = pack_objects(dir, args, "");
        let parsed = pack::parse(&data, &Limits::default()).unwrap();
        let stats = parsed.stats();
        if ofs {
            assert!(stats.ofs_deltas > 0, "{stats:?}");
        } else {
            assert!(stats.ref_deltas > 0, "{stats:?}");
        }
        let mut r = Resolver::new(parsed);
        r.run().unwrap();
        let objects = r.finish().unwrap();
        let mut got: Vec<_> = objects.iter().map(|o| o.id).collect();
        let mut want = listed.clone();
        got.sort();
        want.sort();
        assert_eq!(got, want);
        for o in &objects {
            assert_eq!(ObjectId::hash(o.kind, &o.data), o.id);
        }
        let big = objects.iter().find(|o| o.kind == Kind::Blob && o.data.len() > 100_000).unwrap();
        assert_eq!(*big, cat_file(dir, &big.id));
    }
}

#[tokio::test]
async fn thin_packs_resolve_external_bases() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    repo_with_deltas(&dir);
    let data = pack_objects(&dir, &["--revs", "--thin"], "HEAD\n^HEAD~1\n");
    let mut r = Resolver::new(pack::parse(&data, &Limits::default()).unwrap());
    r.run().unwrap();
    let missing = r.missing();
    assert!(!missing.is_empty(), "expected a thin pack");

    // Through the async helper, looking bases up in the repository.
    let d2 = dir.clone();
    let objects = pack::read_with(data.into(), Limits::default(), move |id| {
        let d = d2.clone();
        async move { Ok(Some(cat_file(&d, &id))) }
    })
    .await
    .unwrap();
    let head: ObjectId = git(&dir, &dir, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    assert!(objects.iter().any(|o| o.id == head));
    assert!(objects.iter().all(|o| !missing.contains(&o.id)));

    // Without the bases it fails cleanly.
    let data = pack_objects(&dir, &["--revs", "--thin"], "HEAD\n^HEAD~1\n");
    assert!(matches!(pack::read(&data, &Limits::default()), Err(PackError::MissingBase(_))));
}

#[test]
fn truncated_and_corrupt_packs_fail_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    repo_with_deltas(tmp.path());
    let data = pack_objects(tmp.path(), &["--revs", "--all", "--delta-base-offset"], "");
    let limits = Limits::default();
    for cut in (0..data.len()).step_by(97) {
        assert!(pack::read(&data[..cut], &limits).is_err());
    }
    // Flip bytes and fix up the checksum so the parser itself sees the damage.
    for pos in (12..data.len() - 20).step_by(53) {
        let mut d = data.clone();
        d[pos] ^= 0x5a;
        assert!(matches!(pack::read(&d, &limits), Err(PackError::Checksum)));
        let end = d.len() - 20;
        let sum = Sha1::digest(&d[..end]);
        d[end..].copy_from_slice(&sum);
        let _ = pack::read(&d, &limits); // must not panic
    }
}

fn finish(mut body: Vec<u8>, count: u32) -> Vec<u8> {
    let mut p = b"PACK".to_vec();
    p.extend_from_slice(&2u32.to_be_bytes());
    p.extend_from_slice(&count.to_be_bytes());
    p.append(&mut body);
    let sum = Sha1::digest(&p);
    p.extend_from_slice(&sum);
    p
}

fn header(ty: u8, size: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut s = size;
    let mut c = (ty << 4) | (s & 15) as u8;
    s >>= 4;
    while s > 0 {
        out.push(c | 0x80);
        c = (s & 0x7f) as u8;
        s >>= 7;
    }
    out.push(c);
    out
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::best());
    z.write_all(data).unwrap();
    z.finish().unwrap()
}

fn varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

#[test]
fn hostile_sizes_do_not_allocate() {
    let limits = Limits::default();
    // A huge object count with no entries.
    assert!(matches!(pack::read(&finish(Vec::new(), u32::MAX), &limits), Err(PackError::Truncated)));

    // A declared size beyond the object limit.
    let mut e = header(3, 1 << 40);
    e.extend(zlib(b"tiny"));
    assert!(matches!(pack::read(&finish(e, 1), &limits), Err(PackError::ObjectTooLarge(_))));

    // A declared size within the limit that the stream doesn't fill.
    let mut e = header(3, 90 << 20);
    e.extend(zlib(b"tiny"));
    assert!(matches!(pack::read(&finish(e, 1), &limits), Err(PackError::Zlib(_))));

    // A delta claiming a 90 MiB result from a tiny base.
    let base = b"base";
    let mut e = header(3, base.len() as u64);
    e.extend(zlib(base));
    let mut delta = varint(4);
    delta.extend(varint(90 << 20));
    delta.extend([0x90 | 0x01, 0, 4]); // copy 4 bytes from offset 0
    let off = e.len();
    let mut d = header(6, delta.len() as u64);
    d.extend(varint(off as u64)); // back to the first entry, `off` bytes earlier
    d.extend(zlib(&delta));
    e.extend(d);
    let r = pack::read(&finish(e, 2), &limits);
    assert!(matches!(r, Err(PackError::BadDelta)), "{r:?}");
}

#[test]
fn delta_bombs_hit_the_unpacked_limit() {
    let limits = Limits { max_pack_bytes: 64 << 20, max_object_bytes: 16 << 20, max_unpacked_bytes: 64 << 20 };
    // A 64 KiB base and many deltas that each copy it 200 times (12.5 MiB each).
    let base: Vec<u8> = (0..65536u32).map(|i| i as u8).collect();
    let mut body = header(3, base.len() as u64);
    body.extend(zlib(&base));
    let mut delta = varint(base.len() as u64);
    delta.extend(varint(base.len() as u64 * 200));
    delta.extend(std::iter::repeat_n(0x80, 200)); // copy offset 0, size 0 (= 0x10000)
    let n = 50;
    for _ in 0..n {
        let mut d = header(7, delta.len() as u64);
        d.extend(ObjectId::hash(Kind::Blob, &base).0);
        d.extend(zlib(&delta));
        body.extend(d);
    }
    let data = finish(body, n + 1);
    assert!(data.len() < 1 << 20);
    assert!(matches!(pack::read(&data, &limits), Err(PackError::UnpackedTooLarge(_))));
}

#[test]
fn written_packs_index_with_git() {
    let tmp = tempfile::tempdir().unwrap();
    let objects: Vec<Object> = (0..50).map(|i| Object::new(Kind::Blob, format!("blob {i}\n").repeat(i * 100 + 1))).collect();
    let data = pack::write(&objects);
    let path = tmp.path().join("p.pack");
    std::fs::write(&path, &data).unwrap();
    let out = Command::new("git").args(["index-pack", "--strict"]).arg(&path).current_dir(tmp.path()).env("HOME", tmp.path()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let by_id: HashMap<_, _> = pack::read(&data, &Limits::default()).unwrap().into_iter().map(|o| (o.id, o)).collect();
    assert_eq!(by_id.len(), 50);
}
