//! A tiny model of the same architecture with made-up weights, for tests: every code path of
//! the real model (tokenizer with `<s>`/`</s>` and the 512-token cut, the encoder, pooling,
//! normalization) runs on it in milliseconds, and no test downloads anything. Its embeddings mean
//! nothing; their shape and invariants are the real model's.

use std::path::Path;

/// Hidden width of the tiny model: E5's 384 (twelve heads), so its embeddings fit where E5's go.
pub const HIDDEN: usize = 12 * super::model::HEAD_SIZE;
const INTERMEDIATE: usize = 64;
const LAYERS: usize = 1;

/// Pieces of the tiny tokenizer: XLM-RoBERTa's four special tokens first (`<s>` 0, `<pad>` 1,
/// `</s>` 2, `<unk>` 3), then `▁`, letters and digits alone and after `▁`.
fn vocab() -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = ["<s>", "<pad>", "</s>", "<unk>"].iter().map(|s| (s.to_string(), 0.0)).collect();
    v.push(("▁".into(), -2.0));
    for c in ('a'..='z').chain('0'..='9').chain(":.,!?".chars()) {
        v.push((c.to_string(), -4.0));
        v.push((format!("▁{c}"), -3.0));
    }
    v
}

fn tokenizer_json() -> serde_json::Value {
    let special = |id: &str, n: u32| serde_json::json!({ "id": id, "ids": [n], "tokens": [id] });
    let added: Vec<serde_json::Value> = ["<s>", "<pad>", "</s>", "<unk>"]
        .iter()
        .enumerate()
        .map(|(i, t)| serde_json::json!({ "id": i, "content": t, "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true }))
        .collect();
    let pieces: Vec<serde_json::Value> = vocab().into_iter().map(|(p, s)| serde_json::json!([p, s])).collect();
    serde_json::json!({
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": added,
        "normalizer": { "type": "Sequence", "normalizers": [
            { "type": "Lowercase" },
            { "type": "Replace", "pattern": { "Regex": " {2,}" }, "content": " " }
        ] },
        "pre_tokenizer": { "type": "Metaspace", "replacement": "▁", "add_prefix_space": true },
        "post_processor": {
            "type": "TemplateProcessing",
            "single": [{ "SpecialToken": { "id": "<s>", "type_id": 0 } }, { "Sequence": { "id": "A", "type_id": 0 } }, { "SpecialToken": { "id": "</s>", "type_id": 0 } }],
            "pair": [{ "SpecialToken": { "id": "<s>", "type_id": 0 } }, { "Sequence": { "id": "A", "type_id": 0 } }, { "SpecialToken": { "id": "</s>", "type_id": 0 } },
                     { "SpecialToken": { "id": "</s>", "type_id": 0 } }, { "Sequence": { "id": "B", "type_id": 0 } }, { "SpecialToken": { "id": "</s>", "type_id": 0 } }],
            "special_tokens": { "<s>": special("<s>", 0), "</s>": special("</s>", 2) }
        },
        "decoder": { "type": "Metaspace", "replacement": "▁", "add_prefix_space": true },
        "model": { "type": "Unigram", "unk_id": 3, "vocab": pieces }
    })
}

/// Deterministic values in `[-scale, scale)` (a linear congruential generator from `seed`).
fn values(n: usize, seed: u64, scale: f32) -> Vec<f32> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((x >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * scale
        })
        .collect()
}

/// Writes the tiny model's [`super::WEIGHTS`] and [`super::TOKENIZER`] into `dir`.
pub fn write(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(super::TOKENIZER), serde_json::to_vec(&tokenizer_json()).map_err(std::io::Error::other)?)?;
    // Name, shape and little-endian bytes of each tensor.
    let mut t: Vec<(String, Vec<usize>, Vec<u8>)> = Vec::new();
    let mut seed = 1;
    let mut put = |name: String, shape: &[usize], scale: f32, base: f32| -> std::io::Result<()> {
        seed += 1;
        let n = shape.iter().product();
        let bytes: Vec<u8> = values(n, seed, scale).into_iter().flat_map(|x| (x + base).to_le_bytes()).collect();
        t.push((name, shape.to_vec(), bytes));
        Ok(())
    };
    let words = vocab().len();
    put("embeddings.word_embeddings.weight".into(), &[words, HIDDEN], 1.0, 0.0)?;
    put("embeddings.position_embeddings.weight".into(), &[super::MAX_TOKENS, HIDDEN], 0.1, 0.0)?;
    put("embeddings.token_type_embeddings.weight".into(), &[2, HIDDEN], 0.1, 0.0)?;
    put("embeddings.LayerNorm.weight".into(), &[HIDDEN], 0.1, 1.0)?;
    put("embeddings.LayerNorm.bias".into(), &[HIDDEN], 0.1, 0.0)?;
    for l in 0..LAYERS {
        let p = format!("encoder.layer.{l}");
        for (name, out, inp) in [
            ("attention.self.query", HIDDEN, HIDDEN),
            ("attention.self.key", HIDDEN, HIDDEN),
            ("attention.self.value", HIDDEN, HIDDEN),
            ("attention.output.dense", HIDDEN, HIDDEN),
            ("intermediate.dense", INTERMEDIATE, HIDDEN),
            ("output.dense", HIDDEN, INTERMEDIATE),
        ] {
            put(format!("{p}.{name}.weight"), &[out, inp], 0.2, 0.0)?;
            put(format!("{p}.{name}.bias"), &[out], 0.05, 0.0)?;
        }
        for norm in ["attention.output.LayerNorm", "output.LayerNorm"] {
            put(format!("{p}.{norm}.weight"), &[HIDDEN], 0.1, 1.0)?;
            put(format!("{p}.{norm}.bias"), &[HIDDEN], 0.1, 0.0)?;
        }
    }
    let views = t
        .iter()
        .map(|(name, shape, bytes)| Ok((name.clone(), safetensors::tensor::TensorView::new(safetensors::Dtype::F32, shape.clone(), bytes).map_err(std::io::Error::other)?)))
        .collect::<std::io::Result<Vec<_>>>()?;
    std::fs::write(dir.join(super::WEIGHTS), safetensors::serialize(views, None).map_err(std::io::Error::other)?)
}
