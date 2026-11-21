//! Agreement with Elasticsearch 9.5.5's own E5 deployments, on the real weights. Opt-in: set
//! `ROLL_E5_WEIGHTS` to a directory with the pinned `model.safetensors` and `tokenizer.json`
//! (`search_embed::FILES`); without it the test passes without running. The expected vectors
//! are `tests/fixtures/e5-es-9.5.5.json`, from an ML node on Linux x86-64:
//!
//! - `embedding_f32`: Elasticsearch's float build (`.multilingual-e5-small`, what it deploys
//!   everywhere but Linux x86-64), against [`Precision::F32`], the build roll runs: every case
//!   within a cosine of 0.999. This covers prefixes, tokens, the 512-token cut, the encoder,
//!   pooling and normalization.
//! - `embedding`: the 8-bit build it deploys on Linux x86-64 (`_linux-x86_64`), against
//!   [`Precision::Int8`] (Elastic's recipe reproduced) and the float build, reported. The 8-bit
//!   build's dynamic quantization rounds each activation to 7 bits, so wherever a float
//!   operation before it rounds differently from Elasticsearch's (MKL's summation order) an
//!   activation lands on the other side of a step: some cases agree to six digits, others only
//!   as well as Elasticsearch's two builds agree with each other (printed). Until every case
//!   reaches 0.999 the node runs the float build; the 8-bit numbers are held where they are
//!   (mean 0.9989, lowest 0.993) so they don't regress unnoticed.

use search_embed::model::Precision;
use search_embed::{E5, PASSAGE, QUERY};

fn cosine(a: &[f32], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| f64::from(*x) * y).sum();
    dot / (a.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt() * b.iter().map(|y| y * y).sum::<f64>().sqrt())
}

/// A case's text: given, or generated from its `text_spec` (long inputs past the 96 KiB cut).
fn text_of(c: &serde_json::Value) -> String {
    if let Some(t) = c["text"].as_str() {
        return t.to_string();
    }
    let spec = &c["text_spec"];
    if let Some(p) = spec["paragraphs"].as_str() {
        let until = spec["until_bytes"].as_u64().unwrap() as usize;
        let mut words: Vec<String> = Vec::new();
        while words.join(" ").len() < until {
            words.push(format!("Paragraph {}: {p}", words.len()));
        }
        return words.join(" ");
    }
    format!("{}{}", spec["repeat"].as_str().unwrap().repeat(spec["times"].as_u64().unwrap() as usize), spec["then"].as_str().unwrap())
}

fn floats(v: &serde_json::Value) -> Vec<f64> {
    v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect()
}

/// `(mean, lowest, its case, identical to six digits)` of `precision` against the fixture's `key`.
fn agreement(m: &E5, cases: &[serde_json::Value], key: &str) -> (f64, f64, String, usize) {
    let (mut sum, mut low, mut low_case, mut same) = (0.0, 1.0f64, String::new(), 0);
    for c in cases {
        let prefix = match c["input_type"].as_str() {
            Some("ingest") => PASSAGE,
            Some("search") => QUERY,
            _ => "",
        };
        let input = format!("{prefix}{}", text_of(c));
        let cos = cosine(&m.embed(&[&input]).unwrap()[0], &floats(&c[key]));
        let name = format!("{} ({}, {} tokens)", c["name"].as_str().unwrap(), c["input_type"].as_str().unwrap_or("none"), m.tokenize(&input).unwrap().len());
        println!("{key} {cos:.6} {name}");
        sum += cos;
        same += usize::from(cos > 0.9999995);
        if cos < low {
            (low, low_case) = (cos, name);
        }
    }
    (sum / cases.len() as f64, low, low_case, same)
}

#[test]
fn embeddings_agree_with_elasticsearch() {
    let Some(dir) = std::env::var_os("ROLL_E5_WEIGHTS") else {
        eprintln!("ROLL_E5_WEIGHTS not set: skipped");
        return;
    };
    let dir = std::path::Path::new(&dir);
    let fixture: serde_json::Value = serde_json::from_str(include_str!("fixtures/e5-es-9.5.5.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    let builds: Vec<f64> = cases.iter().map(|c| {
        let (a, b) = (floats(&c["embedding"]), floats(&c["embedding_f32"]));
        cosine(&a.iter().map(|x| *x as f32).collect::<Vec<_>>(), &b)
    }).collect();
    println!(
        "Elasticsearch's 8-bit build against its float build: mean {:.6}, lowest {:.6}",
        builds.iter().sum::<f64>() / builds.len() as f64,
        builds.iter().cloned().fold(1.0, f64::min)
    );
    let started = std::time::Instant::now();
    let float = E5::load_with(dir, Precision::F32).unwrap();
    let f32 = agreement(&float, cases, "embedding_f32");
    let f32_x86 = agreement(&float, cases, "embedding");
    let int8 = agreement(&E5::load_with(dir, Precision::Int8).unwrap(), cases, "embedding");
    for (what, (mean, low, case, same)) in [("f32 vs ES float", &f32), ("f32 vs ES x86 8-bit", &f32_x86), ("int8 vs ES x86 8-bit", &int8)] {
        println!("{what}: {} cases, mean cosine {mean:.6}, lowest {low:.6} ({case}), {same} identical to six digits", cases.len());
    }
    println!("{:.1}s", started.elapsed().as_secs_f64());
    assert!(f32.1 >= 0.999, "float build: lowest cosine {:.6} on {}", f32.1, f32.2);
    assert!(int8.0 >= 0.9989 && int8.1 >= 0.993, "8-bit build regressed: mean {:.6}, lowest {:.6} on {}", int8.0, int8.1, int8.2);
}
