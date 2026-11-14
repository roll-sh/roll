//! Merge benchmark over real segment files (ignored by default):
//!
//! ```sh
//! ROLL_BENCH_SEGMENTS=<dir with .seg files> cargo test -p search-codec --release \
//!     --test merge_bench -- --ignored --nocapture
//! ```
//!
//! Merges every segment in the directory the way a shard does (BP reordering, then writing the
//! remapped source), several times, and prints the median time of each phase: BP's doc map,
//! writing the reordered segment, and writing the plain merge for comparison. A `search-dev`
//! data directory keeps its segment files under `cache/seg/`.

use std::time::{Duration, Instant};

use search_codec::{BpOptions, MergeInput, MergeSource, RemappedSource, SegmentReader, Slice, WriteOptions, bp_reorder, write_segment_with};

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
#[ignore]
fn bench_merge_files() {
    let Some(dir) = std::env::var_os("ROLL_BENCH_SEGMENTS") else {
        eprintln!("ROLL_BENCH_SEGMENTS is not set");
        return;
    };
    let reps: usize = std::env::var("ROLL_BENCH_REPS").ok().and_then(|r| r.parse().ok()).unwrap_or(5);
    let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "seg")).collect();
    paths.sort();
    let readers: Vec<SegmentReader> = paths.iter().map(|p| SegmentReader::open(Slice::map_file(p).unwrap()).unwrap()).collect();
    let docs: u64 = readers.iter().map(|r| r.max_doc() as u64).sum();
    let source = || MergeSource::new(readers.iter().map(|r| MergeInput { reader: r.clone(), live: None }).collect(), [7; 16]).unwrap();
    let opts = WriteOptions::default();
    let (mut bp, mut remapped, mut plain) = (Vec::new(), Vec::new(), Vec::new());
    let mut bytes = (0, 0);
    for _ in 0..reps {
        let ms = source();
        let t = Instant::now();
        let map = bp_reorder(&ms, &BpOptions::default()).unwrap().expect("BP found nothing to reorder");
        bp.push(t.elapsed());
        let t = Instant::now();
        let r = RemappedSource::new(&ms, map, None).unwrap();
        let mut out = Vec::new();
        write_segment_with(&r, &mut out, &opts).unwrap();
        remapped.push(t.elapsed());
        let t = Instant::now();
        let mut plain_out = Vec::new();
        write_segment_with(&source(), &mut plain_out, &opts).unwrap();
        plain.push(t.elapsed());
        bytes = (out.len(), plain_out.len());
    }
    eprintln!(
        "{} segments, {docs} docs: bp {:?}, write reordered {:?} ({} bytes), plain merge {:?} ({} bytes)",
        readers.len(),
        median(bp),
        median(remapped),
        bytes.0,
        median(plain),
        bytes.1
    );
}
