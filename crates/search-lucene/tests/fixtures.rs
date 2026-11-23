//! The readers against Lucene directories Elasticsearch wrote (rebuilt from snapshots of
//! generated data, `docs/search/compatibility.md` "Restoring Elasticsearch's snapshots"): a
//! 9.5.5 shard (Lucene 10.5.1, LZ4 stored fields, soft deletes from updates and deletes,
//! nested documents, routing, vectors left out of `_source`) and an 8.19.7 one (Lucene 9.12.2,
//! `best_compression`'s zstd). Every live document must come back with Elasticsearch's
//! sequence number, version, routing and `_source`.

use search_lucene::segment::{self, Files, Limits};
use search_lucene::{Error, Result, segments};
use std::path::{Path, PathBuf};

struct Dir {
    root: PathBuf,
    held: i64,
    peak: i64,
}

impl Files for Dir {
    fn len(&mut self, name: &str) -> Result<Option<u64>> {
        Ok(std::fs::metadata(self.root.join(name)).ok().map(|m| m.len()))
    }
    fn read(&mut self, name: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let all = std::fs::read(self.root.join(name)).map_err(|e| Error::Corrupt(e.to_string()))?;
        all.get(offset as usize..offset as usize + len).map(<[u8]>::to_vec).ok_or_else(|| Error::Corrupt(format!("{name} is too short")))
    }
    fn charge(&mut self, delta: i64) -> Result<()> {
        self.held += delta;
        self.peak = self.peak.max(self.held);
        assert!(self.held >= 0, "released more than was charged");
        Ok(())
    }
    fn check(&mut self) -> Result<()> {
        Ok(())
    }
}

const LIMITS: Limits = Limits { small_file: 1 << 20, chunk: 8 << 20, kept: 64 << 20, window: 4096 };

struct Read {
    seq_no: i64,
    version: i64,
    primary_term: i64,
    routing: Option<String>,
    source: serde_json::Value,
    raw: Vec<u8>,
}

fn read_shard(dir: &Path, vectors: &[String]) -> (Vec<Read>, segment::Stats) {
    let mut d = Dir { root: dir.to_path_buf(), held: 0, peak: 0 };
    let commit_name = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()?.file_name().into_string().ok()).find(|n| n.starts_with("segments_")).unwrap();
    let commit = segments::read_commit(&std::fs::read(dir.join(&commit_name)).unwrap(), segments::generation_of(&commit_name).unwrap()).unwrap();
    let mut out = Vec::new();
    let mut total = segment::Stats::default();
    for s in &commit.segments {
        let si = segments::read_segment_info(&std::fs::read(dir.join(format!("{}.si", s.name))).unwrap(), &s.id).unwrap();
        let stats = segment::read(&mut d, s, &si, vectors, &LIMITS, &mut |doc| {
            let mut source: serde_json::Value = serde_json::from_slice(doc.source.unwrap()).unwrap();
            for (field, v) in &doc.vectors {
                source[*field] = serde_json::Value::Array(v.iter().map(|x| serde_json::from_str(&x.to_string()).unwrap()).collect());
            }
            out.push(Read {
                seq_no: doc.seq_no.unwrap(),
                version: doc.version.unwrap(),
                primary_term: doc.primary_term.unwrap(),
                routing: doc.routing.map(str::to_string),
                source,
                raw: doc.source.unwrap().to_vec(),
            });
            Ok(true)
        })
        .unwrap();
        total.docs += stats.docs;
        total.nested += stats.nested;
        total.deleted += stats.deleted;
    }
    assert_eq!(d.held, 0, "everything charged is released");
    assert!(d.peak > 0 && d.peak < 1 << 20, "peak {}", d.peak);
    (out, total)
}

fn expected(name: &str) -> Vec<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()).unwrap()
}

fn check(dir: &str, vectors: &[String]) -> Vec<Read> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(dir);
    let (docs, stats) = read_shard(&root, vectors);
    let want = expected(&format!("{dir}.expected.json"));
    assert_eq!(docs.len(), want.len(), "{stats:?}");
    assert!(stats.deleted > 0 && stats.nested > 0, "{stats:?}");
    for w in &want {
        let seq = w["_seq_no"].as_i64().unwrap();
        let got = docs.iter().find(|d| d.seq_no == seq).unwrap_or_else(|| panic!("no document at seq_no {seq}"));
        assert_eq!(got.version, w["_version"].as_i64().unwrap());
        assert_eq!(got.primary_term, w["_primary_term"].as_i64().unwrap());
        assert_eq!(got.routing.as_deref(), w["_routing"].as_str());
        assert_eq!(got.source, w["_source"], "seq_no {seq}: {}", String::from_utf8_lossy(&got.raw));
        // The bytes Elasticsearch stored, as its `GET _source` gives them.
        assert_eq!(String::from_utf8_lossy(&got.raw), w["source_raw"].as_str().unwrap(), "seq_no {seq}");
    }
    docs
}

#[test]
fn elasticsearch_9_5_5_lucene_10() {
    let docs = check("lucene-10.5.1-mixed-0", &["vec".to_string()]);
    assert!(docs.iter().any(|d| d.version > 1), "updated documents keep their versions");
}

#[test]
fn elasticsearch_8_19_7_zstd() {
    let docs = check("lucene-9.12.2-zstd-0", &[]);
    // 8.x keeps the source as it was sent, vectors and all.
    assert!(docs.iter().all(|d| d.raw.starts_with(b"{\"title\": ") && d.raw.windows(6).any(|w| w == b"\"vec\":")));
}

/// [`Dir`] with bytes changed or cut, as a bucket someone else writes can hold them.
struct Damaged {
    dir: Dir,
    patch: Option<(String, u64, Vec<u8>)>,
    truncate: Option<(String, u64)>,
}

impl Files for Damaged {
    fn len(&mut self, name: &str) -> Result<Option<u64>> {
        let len = self.dir.len(name)?;
        Ok(match &self.truncate {
            Some((n, l)) if n == name => len.map(|_| *l),
            _ => len,
        })
    }
    fn read(&mut self, name: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut out = self.dir.read(name, offset, len)?;
        if let Some((n, at, bytes)) = &self.patch
            && n == name
        {
            for (i, b) in bytes.iter().enumerate() {
                let p = at + i as u64;
                if p >= offset && p < offset + len as u64 {
                    out[(p - offset) as usize] = *b;
                }
            }
        }
        Ok(out)
    }
    fn charge(&mut self, delta: i64) -> Result<()> {
        self.dir.charge(delta)
    }
    fn check(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Where segment `_0`'s stored fields start in its compound file.
fn fdt_offset(root: &Path, s: &segments::SegmentCommit) -> u64 {
    let cfe = std::fs::read(root.join("_0.cfe")).unwrap();
    let cfs_len = std::fs::metadata(root.join("_0.cfs")).unwrap().len();
    search_lucene::compound::read_entries(&cfe, &s.id, cfs_len).unwrap()[".fdt"].offset
}

#[test]
fn damaged_files_fail_cleanly_without_allocating() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lucene-10.5.1-mixed-0");
    let name = std::fs::read_dir(&root).unwrap().filter_map(|e| e.ok()?.file_name().into_string().ok()).find(|n| n.starts_with("segments_")).unwrap();
    let commit = segments::read_commit(&std::fs::read(root.join(&name)).unwrap(), segments::generation_of(&name).unwrap()).unwrap();
    let s = commit.segments.iter().find(|s| s.name == "_0").unwrap();
    let si = segments::read_segment_info(&std::fs::read(root.join("_0.si")).unwrap(), &s.id).unwrap();
    let fdt = fdt_offset(&root, s);
    let header = 4 + 1 + "Lucene90StoredFieldsFastData".len() as u64 + 4 + 16 + 1;
    let cases: Vec<(&str, Damaged)> = vec![
        ("truncated", Damaged { dir: Dir { root: root.clone(), held: 0, peak: 0 }, patch: None, truncate: Some(("_0.cfs".into(), 9000)) }),
        // One byte of a document changed: the checksum catches what decompression doesn't.
        ("checksum", Damaged { dir: Dir { root: root.clone(), held: 0, peak: 0 }, patch: Some(("_0.cfs".into(), fdt + 300, vec![0x55])), truncate: None }),
        // A chunk declaring one document of 2 GiB.
        ("huge", Damaged { dir: Dir { root: root.clone(), held: 0, peak: 0 }, patch: Some(("_0.cfs".into(), fdt + header, vec![0x00, 0x04, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x07])), truncate: None }),
    ];
    for (what, mut d) in cases {
        let mut docs = 0;
        let e = segment::read(&mut d, s, &si, &["vec".to_string()], &LIMITS, &mut |_| {
            docs += 1;
            Ok(true)
        })
        .expect_err(what);
        assert!(d.dir.peak < 1 << 20, "{what}: peak {}", d.dir.peak);
        match what {
            "truncated" => assert!(matches!(&e, Error::Corrupt(m) if m.contains("outside") || m.contains("length should be")), "{e:?}"),
            "huge" => assert!(matches!(&e, Error::Unsupported(m) if m.contains("more than")), "{e:?}"),
            _ => assert!(matches!(&e, Error::Corrupt(_)), "{e:?}"),
        }
    }
}
