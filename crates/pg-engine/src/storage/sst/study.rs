//! Offline measurements of the stored block format (agent 231, HANDOFF "Storage format v2:
//! design"): bytes per block by component, codecs, decode and search costs, over a store's
//! files as a build wrote them.
//!
//! ```text
//! FMT_DIR=<store dir: MANIFEST + .sst> [FMT_ONLY=codecs,cols,seek] \
//!   cargo test --release --lib storage::sst::study -- --ignored --nocapture
//! ```

use super::*;
use crate::storage::batch::RowBatch;
use crate::storage::{cols, row};
use crate::types::{DataType, Value};
use std::collections::BTreeMap;
use std::time::Instant;

/// Block `i`'s entries in the store's file form (a linked run's relinked: as compaction would
/// write them), and its stored length.
fn raw_of(r: &SstReader, i: usize) -> (Vec<u8>, usize) {
    let (off, len) = r.index.at[i];
    let mut buf = vec![0u8; len as usize];
    r.file.read_exact_at(&mut buf, off).unwrap();
    let crc_pos = buf.len() - 4;
    let raw = match buf[0] {
        0 => buf[1..crc_pos].to_vec(),
        _ => {
            let n = u32::from_le_bytes(buf[1..5].try_into().unwrap()) as usize;
            zstd::bulk::decompress(&buf[5..crc_pos], n).unwrap()
        }
    };
    match r.meta.commit_ts {
        Some(ts) => (relink(&raw, ts).unwrap().to_vec(), len as usize),
        None => (raw, len as usize),
    }
}

fn group_of(k: &[u8]) -> (u32, u32) {
    if k.len() < 8 {
        return (0, 0);
    }
    (u32::from_be_bytes(k[0..4].try_into().unwrap()), u32::from_be_bytes(k[4..8].try_into().unwrap()))
}

/// Where the stored row's values start (its count first), past its header.
fn values_at(v: &[u8]) -> Option<usize> {
    let skip = match *v.first()? {
        row::ROW_HEADER | row::ROW_HEADER_EXT => 1,
        row::ROW_HEADER_TID | row::ROW_HEADER_TID_EXT => 3,
        _ => return None,
    };
    let mut p = 1;
    for _ in 0..skip {
        get_varint(v, &mut p).ok()?;
    }
    Some(p)
}

/// The stored row's column count (None: not a row).
fn ncols(v: &[u8]) -> Option<usize> {
    let mut p = values_at(v)?;
    get_varint(v, &mut p).ok().map(|n| n as usize)
}

#[derive(Clone, Copy)]
enum Codec {
    Zstd(i32),
    Lz4,
    Snap,
}

impl Codec {
    fn name(self) -> String {
        match self {
            Codec::Zstd(l) => format!("zstd{l}"),
            Codec::Lz4 => "lz4".into(),
            Codec::Snap => "snappy".into(),
        }
    }
}

const CODECS: [Codec; 8] =
    [Codec::Zstd(1), Codec::Zstd(-1), Codec::Zstd(-2), Codec::Zstd(-3), Codec::Zstd(-5), Codec::Zstd(3), Codec::Lz4, Codec::Snap];

struct Coders {
    zc: Vec<zstd::bulk::Compressor<'static>>,
    zd: zstd::bulk::Decompressor<'static>,
    sc: snap::raw::Encoder,
    sd: snap::raw::Decoder,
}

impl Coders {
    fn new() -> Coders {
        Coders {
            zc: CODECS
                .iter()
                .map(|c| match c {
                    Codec::Zstd(l) => zstd::bulk::Compressor::new(*l).unwrap(),
                    _ => zstd::bulk::Compressor::new(1).unwrap(),
                })
                .collect(),
            zd: zstd::bulk::Decompressor::new().unwrap(),
            sc: snap::raw::Encoder::new(),
            sd: snap::raw::Decoder::new(),
        }
    }

    /// (compressed bytes, compress ns, decompress ns)
    fn run(&mut self, ci: usize, raw: &[u8]) -> (usize, u64, u64) {
        let t0 = Instant::now();
        let c = match CODECS[ci] {
            Codec::Zstd(_) => self.zc[ci].compress(raw).unwrap(),
            Codec::Lz4 => lz4_flex::block::compress(raw),
            Codec::Snap => self.sc.compress_vec(raw).unwrap(),
        };
        let t1 = Instant::now();
        let d = match CODECS[ci] {
            Codec::Zstd(_) => self.zd.decompress(&c, raw.len()).unwrap(),
            Codec::Lz4 => lz4_flex::block::decompress(&c, raw.len()).unwrap(),
            Codec::Snap => self.sd.decompress_vec(&c).unwrap(),
        };
        let t2 = Instant::now();
        assert_eq!(d.len(), raw.len());
        std::hint::black_box(&d);
        (c.len(), (t1 - t0).as_nanos() as u64, (t2 - t1).as_nanos() as u64)
    }
}

#[derive(Default)]
struct G {
    files: usize,
    blocks: usize,
    entries: u64,
    tomb: u64,
    stored: u64,
    raw: u64,
    hdr: u64,
    key: u64,
    ts_stored: u64,
    val: u64,
    derivable: u64,
    compact: u64,
    compact_data: u64,
    ncols: usize,
    uniform_klen_blocks: usize,
    decode_ns: u64,
    /// Per codec: (bytes, compress ns, decompress ns) of the file form and of the compact form.
    file_c: Vec<(u64, u64, u64)>,
    compact_c: Vec<(u64, u64, u64)>,
}

fn label(g: (u32, u32), ncols: usize) -> String {
    match g {
        (0, _) => "system".into(),
        (t, 0) => format!("t{t:x} rows({ncols})"),
        (t, 0xffff_fffe) => format!("t{t:x} toast"),
        (t, i) => format!("t{t:x} ix{i:x}"),
    }
}

fn mb(b: u64) -> f64 {
    b as f64 / 1048576.0
}

fn manifest(dir: &Path) -> super::super::Manifest {
    serde_json::from_slice(&std::fs::read(dir.join("MANIFEST")).unwrap()).unwrap()
}

fn only(what: &str) -> bool {
    std::env::var("FMT_ONLY").map_or(true, |o| o.split(',').any(|x| x == what))
}

#[test]
#[ignore]
fn study() {
    let Ok(dir) = std::env::var("FMT_DIR") else { return };
    let dir = PathBuf::from(dir);
    let m = manifest(&dir);
    let cache = BlockCache::new(1 << 20);
    let readers: Vec<Arc<SstReader>> = m
        .l1
        .iter()
        .chain(&m.l0)
        .map(|meta| Arc::new(SstReader::open(&dir, meta.clone(), cache.clone()).unwrap()))
        .collect();
    println!("{} files ({} L0)", readers.len(), m.l0.len());
    if only("codecs") {
        codecs(&readers);
    }
    if only("cols") {
        columns(&readers);
    }
    if only("seek") {
        seeks(&readers);
    }
}

fn codecs(readers: &[Arc<SstReader>]) {
    let mut groups: BTreeMap<(u32, u32), G> = BTreeMap::new();
    let mut coders = Coders::new();
    let sample = std::env::var("FMT_SAMPLE").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
    for r in readers {
        let gk = group_of(&r.meta.first_key);
        groups.entry(gk).or_default().files += 1;
        for i in 0..r.block_count() {
            let (raw, stored) = raw_of(r, i);
            // (By the block's first key: outputs end where tables and indexes do.)
            let mut pos = 0;
            let mut key: Vec<u8> = Vec::new();
            let first = {
                let (sh, us, _) = entry_head(&raw, &mut pos).unwrap();
                assert_eq!(sh, 0);
                raw[pos..pos + us].to_vec()
            };
            let bg = group_of(&first);
            let g = groups.entry(bg).or_default();
            g.blocks += 1;
            g.stored += stored as u64;
            g.raw += raw.len() as u64;
            pos = 0;
            let mut klen = None;
            let mut uniform = true;
            while pos < raw.len() {
                let p0 = pos;
                let (shared, unshared, vl) = entry_head(&raw, &mut pos).unwrap();
                g.hdr += (pos - p0) as u64;
                key.truncate(shared);
                key.extend_from_slice(&raw[pos..pos + unshared]);
                pos += unshared;
                g.key += unshared as u64;
                let kl = key.len();
                g.ts_stored += (kl - shared.max(kl.saturating_sub(8))) as u64;
                match klen {
                    None => klen = Some(kl),
                    Some(l) if l != kl => uniform = false,
                    _ => {}
                }
                g.entries += 1;
                if vl == 0 {
                    g.tomb += 1;
                } else {
                    let v = &raw[pos..pos + vl - 1];
                    pos += vl - 1;
                    g.val += v.len() as u64;
                    if derived_tail(&key, v).is_some() {
                        g.derivable += v.len() as u64;
                    }
                    if g.ncols == 0 && bg.1 == 0 {
                        g.ncols = ncols(v).unwrap_or(0);
                    }
                }
            }
            g.uniform_klen_blocks += uniform as usize;
            let t0 = Instant::now();
            let b = Block::decode(Bytes::from(raw.clone())).unwrap();
            g.decode_ns += t0.elapsed().as_nanos() as u64;
            let compact = b.data.len() + b.restarts.len() * 4 + b.rvals.len() * 4 + 16;
            g.compact += compact as u64;
            g.compact_data += b.data.len() as u64;
            if i % sample != 0 {
                continue;
            }
            if g.file_c.is_empty() {
                g.file_c = vec![(0, 0, 0); CODECS.len()];
                g.compact_c = vec![(0, 0, 0); CODECS.len()];
            }
            for ci in 0..CODECS.len() {
                let (n, c, d) = coders.run(ci, &raw);
                let e = &mut g.file_c[ci];
                *e = (e.0 + n as u64, e.1 + c, e.2 + d);
                let (n, c, d) = coders.run(ci, &b.data);
                let e = &mut g.compact_c[ci];
                *e = (e.0 + n as u64, e.1 + c, e.2 + d);
            }
        }
    }
    let mut tot = G::default();
    println!("\n## bytes by component (file form, decompressed), per relation");
    println!("| relation | files | blocks | entries | tomb | raw MB | stored MB | hdr % | key % | of it ts % | val % | dup row key % | compact MB (-%) | uniform key len blocks % | decode ns/block |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for (gk, g) in &groups {
        if g.blocks == 0 {
            continue;
        }
        let p = |x: u64| 100.0 * x as f64 / g.raw as f64;
        println!(
            "| {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} ({:.0}) | {:.0} | {:.0} |",
            label(*gk, g.ncols), g.files, g.blocks, g.entries, g.tomb, mb(g.raw), mb(g.stored), p(g.hdr), p(g.key), p(g.ts_stored), p(g.val), p(g.derivable),
            mb(g.compact), 100.0 - p(g.compact), 100.0 * g.uniform_klen_blocks as f64 / g.blocks as f64, g.decode_ns as f64 / g.blocks as f64
        );
        tot.blocks += g.blocks;
        tot.raw += g.raw;
        tot.stored += g.stored;
        tot.compact += g.compact;
        tot.compact_data += g.compact_data;
        tot.decode_ns += g.decode_ns;
        if tot.file_c.is_empty() {
            tot.file_c = vec![(0, 0, 0); CODECS.len()];
            tot.compact_c = vec![(0, 0, 0); CODECS.len()];
        }
        for ci in 0..g.file_c.len() {
            for (t, s) in [(&mut tot.file_c[ci], g.file_c[ci]), (&mut tot.compact_c[ci], g.compact_c[ci])] {
                *t = (t.0 + s.0, t.1 + s.1, t.2 + s.2);
            }
        }
    }
    println!(
        "| all | | {} | | | {:.1} | {:.1} | | | | | | {:.1} | | {:.0} |",
        tot.blocks, mb(tot.raw), mb(tot.stored), mb(tot.compact), tot.decode_ns as f64 / tot.blocks as f64
    );
    println!("\n## codecs (sampled 1 in {sample} blocks): MB out (ratio), compress / decompress MB/s of raw input");
    println!("| relation | form | {} |", CODECS.iter().map(|c| c.name()).collect::<Vec<_>>().join(" | "));
    println!("|---|---|{}", "---|".repeat(CODECS.len()));
    let show = |name: String, g: &G| {
        if g.file_c.is_empty() {
            return;
        }
        let sampled_raw = g.raw as f64 / sample as f64;
        let sampled_compact = g.compact_data as f64 / sample as f64;
        for (form, c, input) in [("file", &g.file_c, sampled_raw), ("compact", &g.compact_c, sampled_compact)] {
            let cells: Vec<String> = c
                .iter()
                .map(|(n, cn, dn)| {
                    format!(
                        "{:.1} ({:.2}x) {:.0}/{:.0}",
                        *n as f64 * sample as f64 / 1048576.0,
                        input / *n as f64,
                        input / 1048576.0 / (*cn as f64 / 1e9),
                        input / 1048576.0 / (*dn as f64 / 1e9)
                    )
                })
                .collect();
            println!("| {name} | {form} | {} |", cells.join(" | "));
        }
    };
    for (gk, g) in &groups {
        show(label(*gk, g.ncols), g);
    }
    show("all".into(), &tot);
}

/// A column's values as a PAX block would hold them (`pax`): typed, fixed width, or the
/// encoded values with offsets.
enum PaxCol {
    Fixed { width: usize, data: Vec<u8>, nulls: Vec<bool> },
    Num { scale: u32, data: Vec<i64>, nulls: Vec<bool> },
    Var { offs: Vec<u32>, data: Vec<u8>, nulls: Vec<bool> },
    Raw { offs: Vec<u32>, data: Vec<u8> },
}

fn num_width(v: i64) -> usize {
    ((64 - (v ^ (v >> 63)).leading_zeros() + 1).div_ceil(8) as usize).max(1)
}

impl PaxCol {
    fn bytes(&self) -> usize {
        let nb = |n: &Vec<bool>| if n.iter().any(|x| *x) { n.len().div_ceil(8) } else { 0 };
        match self {
            PaxCol::Fixed { data, nulls, .. } => data.len() + nb(nulls) + 2,
            PaxCol::Num { data, nulls, .. } => {
                // (The narrowest width all values fit.)
                let w = data.iter().map(|v| num_width(*v)).max().unwrap_or(1);
                data.len() * w + nb(nulls) + 3
            }
            PaxCol::Var { offs, data, nulls } => data.len() + offs.len() * 2 + nb(nulls) + 2,
            PaxCol::Raw { offs, data } => data.len() + offs.len() * 2 + 2,
        }
    }
}

struct Pax {
    xmin: Vec<u64>,
    cols: Vec<PaxCol>,
}

/// The rows of `vals` (stored rows, all `ROW_HEADER` with `n` values) as columns, None when
/// a row doesn't fit (another header, another count).
fn pax(vals: &[&[u8]], n: usize) -> Option<Pax> {
    let mut xmin = Vec::with_capacity(vals.len());
    let mut per: Vec<Vec<(u8, &[u8])>> = vec![Vec::with_capacity(vals.len()); n];
    for v in vals {
        // (A row with a heap TID: its xmin as the block's xmin column, the TID left out here;
        // a PAX layout would keep it as two more columns.)
        if !matches!(v[0], row::ROW_HEADER | row::ROW_HEADER_TID) {
            return None;
        }
        let mut p = 1;
        xmin.push(get_varint(v, &mut p).ok()?);
        p = values_at(v)?;
        if get_varint(v, &mut p).ok()? as usize != n {
            return None;
        }
        for col in per.iter_mut() {
            let s = p;
            row::skip_value(v, &mut p).ok()?;
            col.push((v[s], &v[s..p]));
        }
    }
    let cols = per
        .into_iter()
        .map(|vs| {
            let tag = vs.iter().map(|x| x.0).find(|t| *t != row::tag::NULL);
            let same = vs.iter().all(|x| x.0 == row::tag::NULL || Some(x.0) == tag);
            let nulls: Vec<bool> = vs.iter().map(|x| x.0 == row::tag::NULL).collect();
            let fixed = |w: usize| {
                let mut data = Vec::with_capacity(vs.len() * w);
                for (t, b) in &vs {
                    if *t == row::tag::NULL {
                        data.extend(std::iter::repeat_n(0, w));
                    } else {
                        data.extend_from_slice(&b[1..1 + w]);
                    }
                }
                PaxCol::Fixed { width: w, data, nulls: nulls.clone() }
            };
            let raw = || {
                let mut offs = Vec::new();
                let mut data = Vec::new();
                for (_, b) in &vs {
                    offs.push(data.len() as u32);
                    data.extend_from_slice(b);
                }
                PaxCol::Raw { offs, data }
            };
            match (same, tag) {
                (true, Some(row::tag::INT2)) => fixed(2),
                (true, Some(row::tag::INT4 | row::tag::DATE | row::tag::FLOAT4)) => fixed(4),
                (true, Some(row::tag::INT8 | row::tag::FLOAT8 | row::tag::TIMESTAMP | row::tag::TIMESTAMPTZ)) => fixed(8),
                (true, Some(row::tag::NUMERIC)) => {
                    let mut scale = None;
                    let mut data = Vec::new();
                    for (t, b) in &vs {
                        if *t == row::tag::NULL {
                            data.push(0);
                            continue;
                        }
                        let Some(Some((x, s))) = row::peek_num(b, 0) else { return raw() };
                        if *scale.get_or_insert(s) != s || x > i64::MAX as i128 || x < i64::MIN as i128 {
                            return raw();
                        }
                        data.push(x as i64);
                    }
                    PaxCol::Num { scale: scale.unwrap_or(0), data, nulls }
                }
                (true, Some(row::tag::TEXT)) => {
                    let mut offs = Vec::new();
                    let mut data = Vec::new();
                    for (t, b) in &vs {
                        offs.push(data.len() as u32);
                        if *t != row::tag::NULL {
                            let mut p = 1;
                            let len = get_varint(b, &mut p).unwrap() as usize;
                            data.extend_from_slice(&b[p..p + len]);
                        }
                    }
                    offs.push(data.len() as u32);
                    PaxCol::Var { offs, data, nulls }
                }
                _ => raw(),
            }
        })
        .collect();
    Some(Pax { xmin, cols })
}

/// Row `r` of `p` encoded as the row format stores it (a point read's whole row).
fn pax_row(p: &Pax, r: usize, out: &mut Vec<u8>) {
    out.clear();
    out.push(row::ROW_HEADER);
    put_varint(out, p.xmin[r]);
    put_varint(out, p.cols.len() as u64);
    for c in &p.cols {
        match c {
            PaxCol::Fixed { width, data, nulls } => {
                if nulls[r] {
                    out.push(row::tag::NULL);
                } else {
                    // (The tag isn't kept here: any of the width's.)
                    out.push(match width {
                        2 => row::tag::INT2,
                        4 => row::tag::INT4,
                        _ => row::tag::INT8,
                    });
                    out.extend_from_slice(&data[r * width..(r + 1) * width]);
                }
            }
            PaxCol::Num { scale, data, nulls } => {
                if nulls[r] {
                    out.push(row::tag::NULL);
                } else {
                    out.push(row::tag::NUMERIC);
                    put_varint(out, *scale as u64);
                    let v = data[r];
                    let n = num_width(v);
                    put_varint(out, n as u64);
                    out.extend_from_slice(&v.to_le_bytes()[..n]);
                }
            }
            PaxCol::Var { offs, data, nulls } => {
                if nulls[r] {
                    out.push(row::tag::NULL);
                } else {
                    out.push(row::tag::TEXT);
                    let s = &data[offs[r] as usize..offs[r + 1] as usize];
                    put_varint(out, s.len() as u64);
                    out.extend_from_slice(s);
                }
            }
            PaxCol::Raw { offs, data } => {
                let e = offs.get(r + 1).map_or(data.len(), |x| *x as usize);
                out.extend_from_slice(&data[offs[r] as usize..e]);
            }
        }
    }
}

fn type_of(v: &Value) -> DataType {
    match v {
        Value::Int2(_) => DataType::Int2,
        Value::Int4(_) => DataType::Int4,
        Value::Int8(_) => DataType::Int8,
        Value::Float4(_) => DataType::Float4,
        Value::Float8(_) => DataType::Float8,
        Value::Numeric(_) => DataType::Numeric(-1),
        Value::Bool(_) => DataType::Bool,
        Value::Date(_) => DataType::Date,
        _ => DataType::Text,
    }
}

fn columns(readers: &[Arc<SstReader>]) {
    // The row files of wide tables (5 columns or more), by table.
    let mut tables: BTreeMap<u32, (usize, Vec<Arc<SstReader>>)> = BTreeMap::new();
    for r in readers {
        let (t, i) = group_of(&r.meta.first_key);
        if t == 0 || i != 0 || r.block_count() == 0 {
            continue;
        }
        let b = r.block_with(0, false).unwrap();
        let mut pos = BlockPos::default();
        b.at(0, &mut pos);
        let Some(n) = b.value_ref(&pos).and_then(ncols) else { continue };
        if n < 4 {
            continue;
        }
        tables.entry(t).or_insert((n, Vec::new())).1.push(r.clone());
    }
    let max_blocks: usize = std::env::var("FMT_COL_BLOCKS").ok().and_then(|s| s.parse().ok()).unwrap_or(3000);
    println!("\n## column decode: decode_cols (row form) against a PAX block's typed columns copied out; ns per row (per value)");
    println!("| table | rows | mask | decode_cols | PAX columns | PAX bytes / row-form value bytes | whole row rebuilt from PAX, ns |");
    println!("|---|---|---|---|---|---|---|");
    for (t, (n, files)) in &tables {
        let total: usize = files.iter().map(|f| f.block_count()).sum();
        let step = total.div_ceil(max_blocks).max(1);
        let mut blocks: Vec<Arc<Block>> = Vec::new();
        let mut k = 0;
        for f in files {
            for i in 0..f.block_count() {
                if k % step == 0 {
                    blocks.push(f.block_with(i, false).unwrap());
                }
                k += 1;
            }
        }
        let mut batches: Vec<RowBatch> = Vec::new();
        let mut paxes: Vec<Option<Pax>> = Vec::new();
        let (mut vbytes, mut pbytes) = (0usize, 0usize);
        for b in &blocks {
            let mut batch = RowBatch::default();
            let mut pos = BlockPos::default();
            b.at(0, &mut pos);
            while pos.index() < b.len() {
                if b.value_ref(&pos).is_some() {
                    let k = pos.key().to_vec();
                    b.push_value(&pos, user_key(&k), &mut batch);
                }
                b.step(&mut pos);
            }
            let vals: Vec<&[u8]> = (0..batch.len()).map(|i| batch.get(i).1).collect();
            let p = pax(&vals, *n);
            if let Some(p) = &p {
                vbytes += vals.iter().map(|v| v.len()).sum::<usize>();
                pbytes += p.cols.iter().map(PaxCol::bytes).sum::<usize>() + 8;
            }
            paxes.push(p);
            batches.push(batch);
        }
        let rows: usize = batches.iter().map(|b| b.len()).sum();
        let Some(first) = batches.iter().find(|b| !b.is_empty()) else { continue };
        let v0 = first.get(0).1;
        let Some(p0) = values_at(v0) else { continue };
        let Ok(vals0) = row::decode_row(&v0[p0..]) else { continue };
        let types: Vec<DataType> = vals0.iter().map(type_of).collect();
        let missing = vec![Value::Null; *n];
        let masks: Vec<(&str, Vec<usize>)> = if *n == 16 {
            vec![("q01 (4-10)", (4..=10).collect()), ("q06 (4,5,6,10)", vec![4, 5, 6, 10]), ("all", (0..16).collect())]
        } else {
            vec![("first 2", vec![0, 1]), ("last", vec![n - 1]), ("all", (0..*n).collect())]
        };
        // Whole rows rebuilt from PAX columns (what a point read or a whole-row scan of a PAX
        // block would pay).
        let mut rowbuf = Vec::new();
        let t0 = Instant::now();
        let mut nrow = 0;
        for p in paxes.iter().flatten() {
            for r in 0..p.xmin.len() {
                pax_row(p, r, &mut rowbuf);
                std::hint::black_box(&rowbuf);
                nrow += 1;
            }
        }
        let rec = t0.elapsed().as_nanos() as f64 / nrow.max(1) as f64;
        for (name, cols_) in &masks {
            let need: Vec<bool> = (0..*n).map(|c| cols_.contains(&c)).collect();
            let mut out = cols::ColBatch::default();
            let mut best = u64::MAX;
            let mut failed = false;
            for _ in 0..3 {
                let t0 = Instant::now();
                for b in &batches {
                    failed |= cols::decode_cols(b, 0..b.len(), &need, &types, &missing, &mut out).is_err();
                    std::hint::black_box(&out);
                }
                best = best.min(t0.elapsed().as_nanos() as u64);
            }
            if failed {
                println!("| t{t:x} | | {name} | decode failed | | | |");
                continue;
            }
            // PAX: the needed columns copied out as typed vectors.
            let mut i64s: Vec<i64> = Vec::new();
            let mut bytes: Vec<u8> = Vec::new();
            let mut pbest = u64::MAX;
            for _ in 0..3 {
                let t0 = Instant::now();
                for p in paxes.iter().flatten() {
                    for &c in cols_ {
                        match &p.cols[c] {
                            PaxCol::Num { data, .. } => {
                                i64s.clear();
                                i64s.extend_from_slice(data);
                            }
                            PaxCol::Fixed { data, .. } | PaxCol::Var { data, .. } | PaxCol::Raw { data, .. } => {
                                bytes.clear();
                                bytes.extend_from_slice(data);
                            }
                        }
                        std::hint::black_box((&i64s, &bytes));
                    }
                }
                pbest = pbest.min(t0.elapsed().as_nanos() as u64);
            }
            let nv = cols_.len() as f64;
            let prows: usize = paxes.iter().flatten().map(|p| p.xmin.len()).sum();
            println!(
                "| t{t:x} ({n} cols) | {rows} | {name} | {:.1} ({:.1}) | {:.1} ({:.1}) | {:.2} | {:.0} |",
                best as f64 / rows as f64,
                best as f64 / rows as f64 / nv,
                pbest as f64 / prows.max(1) as f64,
                pbest as f64 / prows.max(1) as f64 / nv,
                pbytes as f64 / vbytes.max(1) as f64,
                rec
            );
        }
        let typed: Vec<String> = paxes
            .iter()
            .flatten()
            .next()
            .map(|p| {
                p.cols
                    .iter()
                    .map(|c| match c {
                        PaxCol::Fixed { width, .. } => format!("f{width}"),
                        PaxCol::Num { .. } => "num".into(),
                        PaxCol::Var { .. } => "var".into(),
                        PaxCol::Raw { .. } => "raw".into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        println!(
            "  t{t:x}: {} of {} blocks fit a PAX layout; columns {}",
            paxes.iter().filter(|p| p.is_some()).count(),
            paxes.len(),
            typed.join(" ")
        );
    }
}

/// Random numbers (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn seeks(readers: &[Arc<SstReader>]) {
    // The biggest relations: every group of 256 blocks or more.
    let mut groups: BTreeMap<(u32, u32), Vec<Arc<SstReader>>> = BTreeMap::new();
    for r in readers {
        groups.entry(group_of(&r.meta.first_key)).or_default().push(r.clone());
    }
    let max_blocks: usize = std::env::var("FMT_SEEK_BLOCKS").ok().and_then(|s| s.parse().ok()).unwrap_or(16384);
    println!("\n## in-block search: ns per seek of a key the block holds (cold: random blocks of a {max_blocks}-block working set), by restart interval");
    println!("| relation | restart | blocks | entries/block | cache bytes/entry | warm seek | cold seek | cold first_restart | cold find_block | cold seek + value |");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for (gk, files) in &groups {
        let total: usize = files.iter().map(|f| f.block_count()).sum();
        if total < 256 {
            continue;
        }
        let step = total.div_ceil(max_blocks).max(1);
        let mut picked: Vec<(usize, usize)> = Vec::new();
        let mut k = 0;
        for (fi, f) in files.iter().enumerate() {
            for i in 0..f.block_count() {
                if k % step == 0 {
                    picked.push((fi, i));
                }
                k += 1;
            }
        }
        let raws: Vec<Vec<u8>> = picked.iter().map(|&(fi, i)| raw_of(&files[fi], i).0).collect();
        for rlog in [4u8, 3, 2] {
            let blocks: Vec<Block> = raws.iter().map(|r| Block::decode_with(r, None, rlog).unwrap()).collect();
            let mem: usize = blocks.iter().map(|b| b.mem_bytes()).sum();
            // 8 keys of each block.
            let mut keys: Vec<(usize, Vec<u8>)> = Vec::new();
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            let mut ents = 0;
            for (bi, b) in blocks.iter().enumerate() {
                ents += b.len();
                let mut pos = BlockPos::default();
                for _ in 0..8 {
                    b.at(rng.next() as usize % b.len(), &mut pos);
                    keys.push((bi, pos.key().to_vec()));
                }
            }
            let n = keys.len() as f64;
            let mut pos = BlockPos::default();
            // Warm: a block's keys one after another, the block in the CPU's cache.
            let t0 = Instant::now();
            for _ in 0..2 {
                for (bi, k) in &keys {
                    blocks[*bi].seek(k, &mut pos);
                    std::hint::black_box(pos.index());
                }
            }
            let warm = t0.elapsed().as_nanos() as f64 / n / 2.0;
            // Cold: in random order.
            let mut order: Vec<usize> = (0..keys.len()).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, rng.next() as usize % (i + 1));
            }
            let t0 = Instant::now();
            for &j in &order {
                let (bi, k) = &keys[j];
                blocks[*bi].seek(k, &mut pos);
                assert_eq!(pos.key(), k.as_slice());
            }
            let cold = t0.elapsed().as_nanos() as f64 / n;
            let t0 = Instant::now();
            for &j in &order {
                let (bi, k) = &keys[j];
                std::hint::black_box(blocks[*bi].first_restart(k));
            }
            let cold_fr = t0.elapsed().as_nanos() as f64 / n;
            let t0 = Instant::now();
            for &j in &order {
                let (bi, k) = &keys[j];
                std::hint::black_box(files[picked[*bi].0].find_block(k));
            }
            let cold_fb = t0.elapsed().as_nanos() as f64 / n;
            let t0 = Instant::now();
            for &j in &order {
                let (bi, k) = &keys[j];
                let b = &blocks[*bi];
                b.seek(k, &mut pos);
                std::hint::black_box(b.value(&pos));
            }
            let cold_v = t0.elapsed().as_nanos() as f64 / n;
            println!(
                "| {} | {} | {} | {:.0} | {:.1} | {warm:.0} | {cold:.0} | {cold_fr:.0} | {cold_fb:.0} | {cold_v:.0} |",
                label(*gk, 0),
                1 << rlog,
                blocks.len(),
                ents as f64 / blocks.len() as f64,
                mem as f64 / ents as f64
            );
        }
    }
}
