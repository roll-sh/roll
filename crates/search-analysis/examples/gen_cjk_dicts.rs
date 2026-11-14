//! Packs the dictionaries of the `kuromoji`, `nori` and `smartcn` analysis components into the
//! compressed files the crate embeds (`src/morph/pack.rs` reads them).
//!
//! Input is a directory written by `tools/cjk_dicts.sh`: Lucene 10.5.1's dictionary files from
//! the analysis-kuromoji, -nori and -smartcn jars (Apache-2.0; built from IPADIC,
//! mecab-ko-dic and Lucene's own Chinese dictionaries), plus what `tools/DumpDicts.java` reads
//! out of them with Lucene itself: each `TokenInfoDictionary` FST's (surface, output) pairs and
//! smartcn's serialized tables. The FSTs are rebuilt with the `fst` crate (UTF-8 surfaces, same
//! outputs) and the connection costs are stored with duplicate rows and columns folded; every
//! other Lucene file is kept byte for byte.
//!
//! usage: cargo run -p search-analysis --example gen_cjk_dicts -- <dump dir> <crate src dir>

use std::path::Path;

fn read_vint(b: &[u8], p: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let x = b[*p];
        *p += 1;
        v |= ((x & 0x7f) as u64) << shift;
        if x & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

fn write_vint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn fst_from_dump(path: &Path) -> Vec<u8> {
    let b = std::fs::read(path).unwrap();
    let mut entries = Vec::new();
    let mut p = 0;
    while p < b.len() {
        let n = read_vint(&b, &mut p) as usize;
        let key = b[p..p + n].to_vec();
        p += n;
        let out = read_vint(&b, &mut p);
        entries.push((key, out));
    }
    entries.sort();
    let mut m = fst::MapBuilder::memory();
    for (k, v) in &entries {
        m.insert(k, *v).unwrap();
    }
    eprintln!("{}: {} surfaces", path.display(), entries.len());
    m.into_inner().unwrap()
}

/// Lucene's `ConnectionCosts.dat` with duplicate rows and columns folded: `forward`,
/// `backward`, the distinct row and column counts (u32 each), each backward id's row and each
/// forward id's column (u16 each), little-endian, then the distinct matrix row-major as zigzag
/// varint deltas (as Lucene stores it).
/// mecab-ko-dic's 3822 x 2693 matrix has only 1908 distinct rows and 1902 distinct columns.
fn costs(b: &[u8]) -> Vec<u8> {
    let mut p = 4;
    let n = b[p] as usize;
    p += 1 + n + 4;
    let forward = read_vint(b, &mut p) as usize;
    let backward = read_vint(b, &mut p) as usize;
    let mut accum = 0i32;
    let mut rows: Vec<Vec<i16>> = Vec::with_capacity(backward);
    for _ in 0..backward {
        let row = (0..forward)
            .map(|_| {
                let z = read_vint(b, &mut p) as u32;
                accum = accum.wrapping_add(((z >> 1) as i32) ^ -((z & 1) as i32));
                accum as i16
            })
            .collect();
        rows.push(row);
    }
    let mut row_ids = std::collections::HashMap::new();
    let mut row_of = Vec::with_capacity(backward);
    let mut distinct_rows: Vec<&Vec<i16>> = Vec::new();
    for r in &rows {
        let next = distinct_rows.len();
        let id = *row_ids.entry(r).or_insert(next);
        if id == next {
            distinct_rows.push(r);
        }
        row_of.push(id as u16);
    }
    let mut col_ids = std::collections::HashMap::new();
    let mut col_of = Vec::with_capacity(forward);
    let mut distinct_cols: Vec<usize> = Vec::new();
    for c in 0..forward {
        let col: Vec<i16> = distinct_rows.iter().map(|r| r[c]).collect();
        let next = distinct_cols.len();
        let id = *col_ids.entry(col).or_insert(next);
        if id == next {
            distinct_cols.push(c);
        }
        col_of.push(id as u16);
    }
    let mut out = Vec::new();
    for v in [forward, backward, distinct_rows.len(), distinct_cols.len()] {
        out.extend_from_slice(&(v as u32).to_le_bytes());
    }
    for v in row_of.iter().chain(&col_of) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let mut prev = 0i32;
    for r in &distinct_rows {
        for &c in &distinct_cols {
            let d = r[c] as i32 - prev;
            write_vint(&mut out, ((d << 1) ^ (d >> 31)) as u32 as u64);
            prev = r[c] as i32;
        }
    }
    eprintln!("  costs: {forward} x {backward} -> {} x {}", distinct_cols.len(), distinct_rows.len());
    out
}

/// smartcn's bigram table: (hash, frequency) pairs sorted by hash as a signed number, stored as
/// varint deltas.
fn smartcn_sections(path: &Path) -> (Vec<u8>, Vec<u8>) {
    let b = std::fs::read(path).unwrap();
    let mut p = 0;
    let i32_at = |p: &mut usize| {
        let v = i32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
        *p += 4;
        v
    };
    let nc = i32_at(&mut p) as usize;
    p += nc * 6;
    let nt = i32_at(&mut p) as usize;
    for _ in 0..nt {
        p += 4;
        let n = i32_at(&mut p) as usize;
        for _ in 0..n {
            let len = u16::from_le_bytes(b[p..p + 2].try_into().unwrap());
            p += 2;
            if len != 0xFFFF {
                p += len as usize * 2;
            }
            p += 4;
        }
    }
    let words = b[..p].to_vec();
    let nb = i32_at(&mut p) as usize;
    let mut bigrams = Vec::new();
    write_vint(&mut bigrams, nb as u64);
    let mut prev = i64::MIN;
    for i in 0..nb {
        let h = i64::from_le_bytes(b[p..p + 8].try_into().unwrap());
        let f = i32::from_le_bytes(b[p + 8..p + 12].try_into().unwrap());
        p += 12;
        assert!(i == 0 || h > prev);
        write_vint(&mut bigrams, h.wrapping_sub(prev) as u64);
        write_vint(&mut bigrams, f as u32 as u64);
        prev = h;
    }
    assert_eq!(p, b.len());
    (words, bigrams)
}

fn pack(out: &Path, sections: &[(&str, Vec<u8>)]) {
    let mut head = Vec::new();
    head.extend_from_slice(b"RCJK");
    head.extend_from_slice(&1u32.to_le_bytes());
    head.extend_from_slice(&(sections.len() as u32).to_le_bytes());
    let mut body = Vec::new();
    for (name, raw) in sections {
        let z = zstd::bulk::compress(raw, 19).unwrap();
        head.push(name.len() as u8);
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        head.extend_from_slice(&(z.len() as u32).to_le_bytes());
        eprintln!("  {name}: {} -> {}", raw.len(), z.len());
        body.extend_from_slice(&z);
    }
    head.extend_from_slice(&body);
    std::fs::write(out, &head).unwrap();
    eprintln!("{}: {} bytes", out.display(), head.len());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dump = Path::new(&args[1]);
    let src = Path::new(&args[2]);
    for (lang, module, name) in [("ja", "kuromoji", "ipadic"), ("ko", "nori", "mecab-ko-dic")] {
        let d = dump.join(lang);
        let file = |f: &str| std::fs::read(d.join(f)).unwrap();
        let sections = vec![
            ("fst", fst_from_dump(&dump.join(format!("{lang}-fst.bin")))),
            ("buffer", file("TokenInfoDictionary$buffer.dat")),
            ("targetMap", file("TokenInfoDictionary$targetMap.dat")),
            ("posDict", file("TokenInfoDictionary$posDict.dat")),
            ("costs", costs(&file("ConnectionCosts.dat"))),
            ("charDef", file("CharacterDefinition.dat")),
            ("unkBuffer", file("UnknownDictionary$buffer.dat")),
            ("unkTargetMap", file("UnknownDictionary$targetMap.dat")),
            ("unkPosDict", file("UnknownDictionary$posDict.dat")),
        ];
        pack(&src.join(module).join(format!("{name}.dict")), &sections);
    }
    let (words, bigrams) = smartcn_sections(&dump.join("smartcn.bin"));
    pack(
        &src.join("smartcn").join("hhmm.dict"),
        &[("words", words), ("bigrams", bigrams)],
    );
}
