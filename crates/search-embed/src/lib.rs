//! roll search's built-in embedding model: `intfloat/multilingual-e5-small` (MIT), run on the CPU
//! as Elasticsearch 9.5.5 runs its `.multilingual-e5-small` deployment, so the preconfigured
//! `.multilingual-e5-small-elasticsearch` inference endpoint works without a provider key.
//!
//! What one embedding is, checked against Elasticsearch's own deployment
//! (`tests/agreement.rs`, `tests/fixtures/e5-es-9.5.5.json`):
//!
//! - the text, after the prefix the caller chose (`passage: ` at ingest, `query: ` at search:
//!   the model's `prefix_strings`; that choice is the inference service's, not this crate's);
//! - XLM-RoBERTa's tokenizer (`tokenizer.json`: the SentencePiece unigram model with its
//!   precompiled normalizer; trailing whitespace dropped), `<s> … </s>`, cut to 512 tokens from
//!   the end (`truncate: first`);
//! - the BERT encoder (12 layers, 384 wide, 12 heads, exact GELU) in `f32`, as Elasticsearch's
//!   `.multilingual-e5-small` build (the 8-bit `_linux-x86_64` build is reproduced too, [`quant`],
//!   but isn't the default until it agrees closely enough: [`model::Precision`]);
//! - each input on its own, never padded into a batch with others (an Elasticsearch call with
//!   one input: the quantization ranges are the input's own);
//! - the mean of the last hidden states over the tokens, `<s>` and `</s>` included, then L2
//!   normalized: 384 floats of length 1.
//!
//! This crate reads two files from a directory ([`WEIGHTS`], [`TOKENIZER`]) and does no other
//! I/O: fetching and checking them is the node's (`search_server::embed`). Everything runs on the
//! calling thread (no thread pool), so a caller's thread CPU clock measures an embedding exactly.

pub mod model;
pub mod ops;
pub mod quant;
pub mod tiny;

use std::path::Path;

/// The model's weights in a directory ([`E5::load`]).
pub const WEIGHTS: &str = "model.safetensors";
/// The model's tokenizer in a directory.
pub const TOKENIZER: &str = "tokenizer.json";
/// Dimensions of an embedding.
pub const DIMS: usize = 384;
/// Tokens an input is cut to, `<s>` and `</s>` included (`max_sequence_length`).
pub const MAX_TOKENS: usize = 512;

/// What Elasticsearch's deployment puts before a document's text (`prefix_strings.ingest`).
pub const PASSAGE: &str = "passage: ";
/// …and before a query's (`prefix_strings.search`).
pub const QUERY: &str = "query: ";

/// Where the weights come from by default: the Hugging Face repository at a pinned revision.
pub const REPOSITORY: &str = "https://huggingface.co/intfloat/multilingual-e5-small/resolve";
/// The pinned revision (2026-04-02).
pub const REVISION: &str = "614241f622f53c4eeff9890bdc4f31cfecc418b3";
/// The files of the pinned revision with their SHA-256 digests and sizes.
pub const FILES: [(&str, &str, u64); 2] = [
    (WEIGHTS, "1a55775f53449dac10a2bcbc312469fac40b96d53198c407081a831f81c98477", 470_641_600),
    (TOKENIZER, "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39", 17_082_730),
];

/// A run of whitespace longer than this is shortened to it before tokenizing: the normalizer
/// collapses any run to one space, so the tokens are the same.
const WHITESPACE_RUN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("model files: {0}")]
    Files(String),
    #[error("tokenizer: {0}")]
    Tokenizer(String),
    #[error("model: {0}")]
    Model(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The loaded model.
pub struct E5 {
    tokenizer: tokenizers::Tokenizer,
    model: model::Bert,
    /// Bytes the weights and the tokenizer take in memory.
    bytes: u64,
    /// Bytes of input that can hold [`MAX_TOKENS`] tokens: four times 512 times the longest
    /// vocabulary piece ([`E5::cut`]).
    cut_bytes: usize,
    /// What the tokenizer's normaliser does to single characters, for the cut.
    classes: Classes,
}

/// The characters of the Basic Multilingual Plane the tokenizer's normaliser deletes (C0 and C1
/// controls in `nmt_nfkc`), and those it turns into a space (zero-width and other spaces, which
/// then collapse with their neighbours), read from the normaliser itself at load. Outside the
/// BMP nothing maps to either in the shipped normaliser (checked), so those are left as they are.
#[derive(Clone)]
pub struct Classes {
    deleted: Box<[u64; 1024]>,
    spaces: Box<[u64; 1024]>,
}

impl Classes {
    /// No character deleted, only Unicode whitespace as space (no normaliser).
    pub fn none() -> Classes {
        Classes { deleted: Box::new([0; 1024]), spaces: Box::new([0; 1024]) }
    }

    /// The classes `normalize` gives each BMP character.
    pub fn from_normalizer(normalize: impl Fn(&str) -> Option<String>) -> Classes {
        let mut c = Classes::none();
        for cp in 0u32..0x10000 {
            let Some(ch) = char::from_u32(cp) else { continue };
            let s = ch.to_string();
            match normalize(&s) {
                Some(n) if n.is_empty() => c.deleted[cp as usize / 64] |= 1 << (cp % 64),
                Some(n) if n != s && n.chars().all(|x| x == ' ') => c.spaces[cp as usize / 64] |= 1 << (cp % 64),
                _ => {}
            }
        }
        c
    }

    fn deleted(&self, c: char) -> bool {
        let cp = c as usize;
        cp < 0x10000 && self.deleted[cp / 64] & (1 << (cp % 64)) != 0
    }

    /// Whitespace, or a character the normaliser turns into a space.
    fn space(&self, c: char) -> bool {
        let cp = c as usize;
        c.is_whitespace() || (cp < 0x10000 && self.spaces[cp / 64] & (1 << (cp % 64)) != 0)
    }
}

impl std::fmt::Debug for E5 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("E5").field("bytes", &self.bytes).finish()
    }
}

impl E5 {
    /// Loads [`WEIGHTS`] and [`TOKENIZER`] from `dir`. The configuration is the model's
    /// (`config.json` of the pinned revision): only the shapes come from the weights, so a model of
    /// the same architecture and other sizes ([`tiny`]) loads too.
    pub fn load(dir: &Path) -> Result<E5> {
        E5::load_with(dir, model::Precision::F32)
    }

    /// [`E5::load`] with the linear layers' precision chosen.
    pub fn load_with(dir: &Path, precision: model::Precision) -> Result<E5> {
        let tokenizer_file = dir.join(TOKENIZER);
        let tokenizer_size = std::fs::metadata(&tokenizer_file).map_err(|e| Error::Files(format!("{}: {e}", tokenizer_file.display())))?.len();
        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_file).map_err(|e| Error::Tokenizer(e.to_string()))?;
        tokenizer.with_padding(None);
        tokenizer.with_truncation(None).map_err(|e| Error::Tokenizer(e.to_string()))?;
        let path = dir.join(WEIGHTS);
        let file = std::fs::File::open(&path).map_err(|e| Error::Files(format!("{}: {e}", path.display())))?;
        // SAFETY: the file is written once under a temporary name and renamed into place
        // (`search_server::embed`), and nothing writes it afterwards; the values are copied out
        // of the mapping before it is dropped.
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| Error::Files(format!("{}: {e}", path.display())))?;
        let tensors = read_tensors(&map).map_err(|e| Error::Files(format!("{}: {e}", path.display())))?;
        drop(map);
        let model = model::Bert::new(tensors, precision)?;
        // The vocabulary twice (piece to id and back) and the lattice tables: about twice the
        // JSON it was read from (an estimate; the tokenizer doesn't report its size).
        let bytes = model.bytes() + 2 * tokenizer_size;
        let longest = tokenizer.get_vocab(true).keys().map(|p| p.trim_start_matches('\u{2581}').len()).max().unwrap_or(1).max(1);
        let cut_bytes = (4 * MAX_TOKENS * longest).max(4096);
        let classes = match tokenizer.get_normalizer() {
            Some(n) => Classes::from_normalizer(|s| {
                let mut ns = tokenizers::NormalizedString::from(s);
                tokenizers::Normalizer::normalize(n, &mut ns).ok().map(|_| ns.get().to_string())
            }),
            None => Classes::none(),
        };
        Ok(E5 { tokenizer, model, bytes, cut_bytes, classes })
    }

    /// Bytes the loaded model holds (weights, and an estimate of the tokenizer's tables).
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The start of `text` its first [`MAX_TOKENS`] tokens come from, trailing whitespace
    /// dropped: at most [`E5::cut_bytes`] bytes, whitespace runs shortened. Tokenizing the cut
    /// gives the same 512 tokens as tokenizing the whole text, in bounded time and memory,
    /// whatever the input (a megabyte without a space included).
    pub fn cut<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        cut_text(text, self.cut_bytes, &self.classes)
    }

    /// Bytes [`E5::cut`] keeps at most.
    pub fn cut_bytes(&self) -> usize {
        self.cut_bytes
    }

    /// Memory embedding an input of the longest length takes besides the weights (the hidden
    /// states, one head's attention scores, the feed-forward activations of one layer).
    pub fn batch_bytes_max(&self) -> u64 {
        self.model.activation_bytes(MAX_TOKENS.min(self.model.max_positions()).max(1))
    }

    /// `text`'s token ids as the model sees them: `<s>`, at most 510 tokens, `</s>`.
    pub fn tokenize(&self, text: &str) -> Result<Vec<u32>> {
        self.tokenize_prefixed("", text)
    }

    /// The token ids of `prefix` then `text` ([`E5::cut`] first), without building the
    /// prefixed copy of a long text. Trailing whitespace is dropped, as SentencePiece's
    /// `remove_extra_whitespaces` (and Elasticsearch's tokenizer) drops it; `tokenizer.json`
    /// would give it a token of its own.
    pub fn tokenize_prefixed(&self, prefix: &str, text: &str) -> Result<Vec<u32>> {
        let cut = self.cut(text);
        let input = if prefix.is_empty() { cut } else { std::borrow::Cow::Owned(format!("{prefix}{cut}")) };
        Ok(truncate(self.encode(input.trim_end())?))
    }

    /// The token ids of the whole of `text`, uncut (trailing whitespace dropped): what
    /// [`E5::tokenize`] must equal, for checks. Its cost grows with the text.
    pub fn tokenize_uncut(&self, text: &str) -> Result<Vec<u32>> {
        let classes = &self.classes;
        Ok(truncate(self.encode(text.trim_end_matches(|c| classes.space(c) || classes.deleted(c)))?))
    }

    fn encode(&self, text: &str) -> Result<Vec<u32>> {
        let enc = self.tokenizer.encode(text, true).map_err(|e| Error::Tokenizer(e.to_string()))?;
        Ok(enc.get_ids().to_vec())
    }

    /// Embeddings of `texts`, in order.
    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed_one(&self.tokenize(t)?)).collect()
    }

    /// Embeddings of `prefix` before each of `texts`, in order, one input at a time.
    pub fn embed_prefixed(&self, prefix: &str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed_one(&self.tokenize_prefixed(prefix, t)?)).collect()
    }

    /// Embeddings of tokenized inputs ([`E5::tokenize`]), in order, each run on its own.
    pub fn embed_tokens(&self, ids: &[Vec<u32>]) -> Result<Vec<Vec<f32>>> {
        ids.iter().map(|row| self.embed_one(row)).collect()
    }

    /// The mean of the last hidden states over the tokens, then unit length (`clamp_min(1e-12)`
    /// on the norm, as the model's normalization).
    fn embed_one(&self, ids: &[u32]) -> Result<Vec<f32>> {
        let h = self.model.hidden();
        let hidden = self.model.forward(ids)?;
        let rows = ids.len().max(1) as f32;
        let mut mean = vec![0f32; h];
        for row in hidden.chunks_exact(h) {
            for (m, v) in mean.iter_mut().zip(row) {
                *m += v;
            }
        }
        for m in mean.iter_mut() {
            *m /= rows;
        }
        let norm = mean.iter().map(|v| f64::from(*v) * f64::from(*v)).sum::<f64>().sqrt().max(1e-12) as f32;
        Ok(mean.into_iter().map(|v| v / norm).collect())
    }
}

/// The tensors of a `.safetensors` file as `f32` (`F32`, `F16` and `BF16` are read).
fn read_tensors(bytes: &[u8]) -> std::result::Result<std::collections::HashMap<String, model::Weights>, String> {
    let st = safetensors::SafeTensors::deserialize(bytes).map_err(|e| e.to_string())?;
    let mut out = std::collections::HashMap::new();
    for (name, view) in st.tensors() {
        let data = view.data();
        let values: Vec<f32> = match view.dtype() {
            safetensors::Dtype::F32 => data.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect(),
            safetensors::Dtype::F16 => data.as_chunks::<2>().0.iter().map(|b| f16_to_f32(u16::from_le_bytes(*b))).collect(),
            safetensors::Dtype::BF16 => data.as_chunks::<2>().0.iter().map(|b| f32::from_bits(u32::from(u16::from_le_bytes(*b)) << 16)).collect(),
            // Integer buffers (`position_ids`) aren't weights.
            _ => continue,
        };
        out.insert(name, model::Weights { shape: view.shape().to_vec(), values });
    }
    Ok(out)
}

/// IEEE half to single precision.
fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let man = u32::from(h & 0x3ff);
    let bits = match (exp, man) {
        (0, 0) => sign,
        (0, m) => {
            // Subnormal: normalize.
            let shift = m.leading_zeros() - 21;
            sign | ((113 - shift) << 23) | (((m << shift) & 0x3ff) << 13)
        }
        (31, m) => sign | 0x7f80_0000 | (m << 13),
        (e, m) => sign | ((e + 112) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

/// The start of `text` its first [`MAX_TOKENS`] tokens come from: characters the normaliser
/// deletes skipped (they'd vanish anyway), runs of whitespace and of characters it turns into
/// spaces shortened to [`WHITESPACE_RUN`] (they'd collapse to one space anyway), trailing ones
/// dropped, then at most `limit` bytes, cut at a character boundary. Each byte kept then
/// normalises to text that holds at least a quarter of a token's bytes, so `limit` (four times
/// 512 times the longest piece) holds the 512 tokens. Borrowed when nothing was left out.
pub fn cut_text<'a>(text: &'a str, limit: usize, classes: &Classes) -> std::borrow::Cow<'a, str> {
    let text = text.trim_end_matches(|c| classes.space(c) || classes.deleted(c));
    let mut out: Option<String> = None;
    let (mut run, mut kept) = (0usize, 0usize);
    for (i, c) in text.char_indices() {
        let skip = if classes.deleted(c) {
            true
        } else {
            run = if classes.space(c) { run + 1 } else { 0 };
            run > WHITESPACE_RUN
        };
        if skip {
            out.get_or_insert_with(|| text[..i].to_string());
            continue;
        }
        let n = c.len_utf8();
        if kept + n > limit {
            return match out {
                Some(o) => std::borrow::Cow::Owned(o),
                None => std::borrow::Cow::Borrowed(&text[..i]),
            };
        }
        kept += n;
        if let Some(o) = out.as_mut() {
            o.push(c);
        }
    }
    match out {
        Some(o) => std::borrow::Cow::Owned(o),
        None => std::borrow::Cow::Borrowed(text),
    }
}

fn truncate(mut ids: Vec<u32>) -> Vec<u32> {
    if ids.len() > MAX_TOKENS {
        let last = ids[ids.len() - 1];
        ids.truncate(MAX_TOKENS - 1);
        ids.push(last);
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_the_closing_token() {
        let ids: Vec<u32> = std::iter::once(0).chain(5..600).chain(std::iter::once(2)).collect();
        let t = truncate(ids);
        assert_eq!(t.len(), MAX_TOKENS);
        assert_eq!((t[0], t[510], t[511]), (0, 514, 2));
        assert_eq!(truncate(vec![0, 7, 2]), [0, 7, 2]);
    }

    #[test]
    fn cuts_are_bounded_and_borrow_when_they_can() {
        let none = Classes::none();
        assert!(matches!(cut_text("short text  ", 100, &none), std::borrow::Cow::Borrowed("short text")));
        assert_eq!(cut_text("abcdef", 4, &none), "abcd");
        // A character is never split.
        assert_eq!(cut_text("ééé", 5, &none), "éé");
        let spaced = format!("a{}b", " ".repeat(1000));
        assert_eq!(cut_text(&spaced, 100, &none), format!("a{}b", " ".repeat(WHITESPACE_RUN)));
        assert_eq!(cut_text(&"x".repeat(1 << 20), 4096, &none).len(), 4096);
        // What the normaliser deletes costs nothing; what it turns into spaces runs like spaces.
        let nfkc = Classes::from_normalizer(|s| {
            Some(match s {
                "\u{1}" => String::new(),
                "\u{200b}" => " ".to_string(),
                other => other.to_string(),
            })
        });
        let deleted = format!("{}article", "\u{1}".repeat(40_000));
        assert_eq!(cut_text(&deleted, 100, &nfkc), "article");
        let zero_width = format!("{}article\u{200b}", "\u{200b}".repeat(33_000));
        assert_eq!(cut_text(&zero_width, 100, &nfkc), format!("{}article", "\u{200b}".repeat(WHITESPACE_RUN)));
    }

    #[test]
    fn halves_widen_exactly() {
        for (h, want) in [(0x3c00u16, 1.0f32), (0xc000, -2.0), (0x0001, 5.960_464_5e-8), (0x7bff, 65504.0), (0x0000, 0.0)] {
            assert_eq!(f16_to_f32(h), want, "{h:#x}");
        }
    }
}
