//! Every committed fixture (`tests/fixtures/<format>.tar.zst`, made by `generate.sh`) scans
//! cleanly — chunk CRCs, CRC.db and Digest.crc32 checked — and every format holds the same
//! partitions, rows and range tombstone markers. `ROLL_IMPORT_SCAN=/dir` scans another backup
//! directory and prints it (`ROLL_IMPORT_SCAN_DUMP=1`: every row).

use nosql_sstable_import::descriptor::Format;
use nosql_sstable_import::layout;
use nosql_sstable_import::rows::Item;
use nosql_sstable_import::source::Location;
use nosql_sstable_import::sstable::open;
use std::collections::BTreeMap;
use std::path::Path;

/// (partitions, rows, markers) per table: the same in every format.
const COUNTS: &[(&str, (u64, u64, u64))] = &[
    ("all_types", (5, 6, 0)),
    ("c_deflate", (7, 150, 0)),
    ("c_lz4", (7, 150, 0)),
    ("c_none", (7, 150, 0)),
    ("c_snappy", (7, 150, 0)),
    ("c_zstd", (7, 150, 0)),
    ("colls", (4, 4, 0)),
    ("composite", (2, 3, 2)),
    ("counters", (3, 4, 0)),
    ("deletes", (4, 6, 7)),
    ("dropped", (2, 2, 0)),
    ("statics", (4, 3, 0)),
    ("ttls", (2, 4, 0)),
    ("wide", (1, 1450, 2)),
];

async fn scan(dir: &Path, dump: bool) -> BTreeMap<String, (u64, u64, u64, Vec<String>)> {
    let loc = Location::parse(&format!("file://{}", dir.display())).unwrap();
    let files = loc.list().await.unwrap();
    let p = layout::plan(&files);
    let tag = layout::choose_snapshot_tag(&p, None, &Default::default()).unwrap();
    let tables = layout::snapshot_tables(&files, tag.as_deref()).unwrap();
    let store = loc.store(tokio::runtime::Handle::current());
    tokio::task::spawn_blocking(move || {
        let mut out = BTreeMap::new();
        for t in tables {
            let e = out.entry(t.table.clone().unwrap_or_default()).or_insert((0, 0, 0, vec![]));
            for s in &t.sstables {
                e.3.push(format!("{}-{}", s.version.as_str(), s.version.format.name()));
                let mut o = open(&store, s, 64 << 10).unwrap_or_else(|e| panic!("{}: {e:#}", s.data_key()));
                while let Some(h) = o.scanner.next_partition().unwrap() {
                    e.0 += 1;
                    if dump {
                        println!("  partition {:02x?} {:?} static {:?}", &h.key[..], h.deletion, h.static_row);
                    }
                    while let Some(i) = o.scanner.next_item().unwrap() {
                        if dump {
                            println!("    {i:?}");
                        }
                        match i {
                            Item::Row(_) => e.1 += 1,
                            Item::Marker(_) => e.2 += 1,
                        }
                    }
                }
                o.scanner.input().finish().unwrap_or_else(|e| panic!("{}: {e:#}", s.data_key()));
            }
        }
        out
    })
    .await
    .unwrap()
}

fn extract(fmt: &str, into: &Path) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let raw = std::fs::read(dir.join(format!("{fmt}.tar.zst"))).unwrap();
    for (path, body) in nosql_sstable_import::tar::entries(&zstd::decode_all(&raw[..]).unwrap()).unwrap() {
        let p = into.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_format_reads_the_same_data() {
    for (fmt, format) in [("nb", Format::Big), ("oa", Format::Big), ("da", Format::Bti), ("me", Format::Big), ("ms", Format::Big), ("mt", Format::Big)] {
        let dir = tempfile::tempdir().unwrap();
        extract(fmt, dir.path());
        let got = scan(dir.path(), false).await;
        for (table, want) in COUNTS {
            let (p, r, m, versions) = &got[*table];
            assert_eq!((*p, *r, *m), *want, "{fmt} {table}");
            assert!(versions.iter().all(|v| *v == format!("{fmt}-{}", format.name())), "{fmt} {table}: {versions:?}");
        }
        assert_eq!(got.len(), COUNTS.len(), "{fmt}");
    }
}

/// ScyllaDB's dictionary compressors, with dictionaries trained by `retrain_dict` (tables
/// `c_zdict` / `c_ldict` of 600 rows, written before and after training).
#[tokio::test(flavor = "multi_thread")]
async fn scylla_dictionary_compression() {
    use nosql_sstable_import::compression::{Codec, CompressionInfo};
    let dir = tempfile::tempdir().unwrap();
    extract("mt-dicts", dir.path());
    let got = scan(dir.path(), false).await;
    assert_eq!(got["c_zdict"].0..=got["c_zdict"].1, 5..=600);
    assert_eq!(got["c_ldict"].0..=got["c_ldict"].1, 5..=600);
    let mut codecs = vec![];
    for f in walk(dir.path()) {
        if f.to_string_lossy().ends_with("CompressionInfo.db") {
            let raw = bytes::Bytes::from(std::fs::read(&f).unwrap());
            let n = raw.len() as u64;
            codecs.push(match CompressionInfo::parse(raw, n, false).unwrap().codec {
                Codec::ZstdDict(_) => "zstd+dict",
                Codec::Lz4Dict(_) => "lz4+dict",
                _ => "plain",
            });
        }
    }
    assert!(codecs.contains(&"zstd+dict") && codecs.contains(&"lz4+dict"), "{codecs:?}");
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_dir() {
    let Ok(dir) = std::env::var("ROLL_IMPORT_SCAN") else { return };
    for (t, c) in scan(Path::new(&dir), std::env::var("ROLL_IMPORT_SCAN_DUMP").is_ok()).await {
        println!("{t}: {} partitions, {} rows, {} markers in {:?}", c.0, c.1, c.2, c.3);
    }
}

/// Opens and scans every SSTable of a backup directory; the errors, as `{:#}` strings.
async fn errors(dir: &Path) -> Vec<String> {
    let loc = Location::parse(&format!("file://{}", dir.display())).unwrap();
    let files = loc.list().await.unwrap();
    let p = layout::plan(&files);
    let tag = layout::choose_snapshot_tag(&p, None, &Default::default()).unwrap();
    let tables = match layout::snapshot_tables(&files, tag.as_deref()) {
        Ok(t) => t,
        Err(e) => return vec![format!("{e:#}")],
    };
    let store = loc.store(tokio::runtime::Handle::current());
    tokio::task::spawn_blocking(move || {
        let mut out = vec![];
        for s in tables.iter().flat_map(|t| &t.sstables) {
            let r = open(&store, s, 64 << 10).and_then(|mut o| {
                while o.scanner.next_partition()?.is_some() {
                    while o.scanner.next_item()?.is_some() {}
                }
                o.scanner.input().finish()
            });
            if let Err(e) = r {
                out.push(format!("{e:#}"));
            }
        }
        out
    })
    .await
    .unwrap()
}

/// A file of `table` (of its first SSTable) whose name ends with `component`.
fn component(dir: &Path, table: &str, component: &str) -> std::path::PathBuf {
    let mut found: Vec<_> = walk(dir).into_iter().filter(|p| p.to_string_lossy().contains(&format!("/{table}-")) && p.to_string_lossy().ends_with(component)).collect();
    found.sort();
    found.into_iter().next().unwrap_or_else(|| panic!("{table}: no {component}"))
}

/// Damaged or unsupported components fail the SSTable they belong to with an error naming it,
/// and leave the other tables readable.
#[tokio::test(flavor = "multi_thread")]
async fn damaged_and_unsupported_components() {
    let one = |errs: Vec<String>, needles: &[&str]| {
        assert_eq!(errs.len(), 1, "{needles:?}: {errs:?}");
        assert!(needles.iter().all(|n| errs[0].contains(n)), "{needles:?}: {}", errs[0]);
    };
    // CompressionInfo.db listed in TOC.txt but missing.
    let dir = tempfile::tempdir().unwrap();
    extract("nb", dir.path());
    std::fs::remove_file(component(dir.path(), "c_lz4", "-CompressionInfo.db")).unwrap();
    one(errors(dir.path()).await, &["c_lz4", "incomplete", "CompressionInfo.db is missing"]);
    // A Digest.crc32 that isn't a number.
    let dir = tempfile::tempdir().unwrap();
    extract("oa", dir.path());
    std::fs::write(component(dir.path(), "colls", "-Digest.crc32"), "not a crc").unwrap();
    one(errors(dir.path()).await, &["colls", "Digest.crc32"]);
    // A truncated Scylla.db.
    let dir = tempfile::tempdir().unwrap();
    extract("me", dir.path());
    std::fs::write(component(dir.path(), "statics", "-Scylla.db"), [0u8, 0]).unwrap();
    one(errors(dir.path()).await, &["statics", "Scylla.db"]);
    // Another partitioner than Murmur3 (ScyllaDB's `me` Statistics.db has no checksum).
    let dir = tempfile::tempdir().unwrap();
    extract("me", dir.path());
    let f = component(dir.path(), "ttls", "-Statistics.db");
    let mut raw = std::fs::read(&f).unwrap();
    let at = raw.windows(18).position(|w| w == b"Murmur3Partitioner").expect("the partitioner name");
    raw[at..at + 18].copy_from_slice(b"RandomXPartitioner");
    std::fs::write(&f, raw).unwrap();
    one(errors(dir.path()).await, &["ttls", "partitioner org.apache.cassandra.dht.RandomXPartitioner is not supported"]);
    // A version this importer doesn't read (a future one), and pre-3.0 names.
    let dir = tempfile::tempdir().unwrap();
    extract("nb", dir.path());
    let f = component(dir.path(), "wide", "-Data.db");
    let renamed = f.with_file_name(f.file_name().unwrap().to_string_lossy().replacen("nb-", "pa-", 1));
    std::fs::rename(&f, &renamed).unwrap();
    one(errors(dir.path()).await, &["pa-", "sstable version pa (big) is not supported"]);
    let dir = tempfile::tempdir().unwrap();
    extract("nb", dir.path());
    let f = component(dir.path(), "wide", "-Data.db");
    std::fs::write(f.with_file_name("imp_fx-wide-ka-1-Data.db"), b"").unwrap();
    one(errors(dir.path()).await, &["pre-3.0 sstables (ka/la) are not supported"]);
}
