//! The BERT encoder of multilingual-e5-small (`BertModel` in `transformers`): token, position and
//! token type embeddings, then layers of self-attention and a GELU feed-forward block, each with
//! a residual connection and layer normalization. Shapes come from the weights; the head size is
//! the model's (32).
//!
//! One input at a time (no padding, so no attention mask), on plain `f32` slices: the products
//! through `gemm` with no parallelism, so everything runs on the calling thread, and the linear
//! layers in `f32` or dynamically quantized to 8 bits ([`Precision`], [`crate::quant`]). The
//! order of operations is the model's (Elastic's TorchScript): embeddings `(tokens + token
//! types) + positions`, attention scores divided by √32, layer normalization as
//! `(x · rstd − rstd · mean) · γ + β`.

use crate::Error;
use crate::ops;
use crate::quant::QLinear;
use std::collections::HashMap;

/// Width of one attention head (384 / 12).
pub const HEAD_SIZE: usize = 32;
/// `layer_norm_eps`.
const EPS: f32 = 1e-12;

/// How the linear layers compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Precision {
    /// As the `transformers` model: Elasticsearch's `.multilingual-e5-small`, which it deploys
    /// everywhere but on Linux x86-64.
    F32,
    /// Dynamically quantized as Elastic's `.multilingual-e5-small_linux-x86_64`, which
    /// Elasticsearch deploys on Linux x86-64 ([`crate::quant`], `ELASTIC_X86_FFN`). Not used by
    /// the node yet: its embeddings agree with that build's to a mean cosine of 0.9991 (lowest
    /// 0.994), short of the 0.999 every input must reach (`tests/agreement.rs`).
    Int8,
}

/// How one linear layer computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    F32,
    /// [`QLinear`] with one weight scale.
    PerTensor,
    /// [`QLinear`] with a weight scale per output row.
    PerChannel,
}

/// Elastic's `.multilingual-e5-small_linux-x86_64`, layer by layer (read from its TorchScript
/// code and constants): every attention projection (query, key, value, output) quantized per
/// tensor, and the feed-forward block's `(intermediate, output)` as below.
const ELASTIC_X86_FFN: [(Kind, Kind); 12] = {
    use Kind::*;
    [
        (PerChannel, F32),
        (PerChannel, PerChannel),
        (PerTensor, PerChannel),
        (PerTensor, PerChannel),
        (PerChannel, F32),
        (PerChannel, F32),
        (PerChannel, PerChannel),
        (PerChannel, PerChannel),
        (PerChannel, PerChannel),
        (PerChannel, PerChannel),
        (PerTensor, PerTensor),
        (PerTensor, PerTensor),
    ]
};

/// A tensor read from the weights: its shape and values.
pub struct Weights {
    pub shape: Vec<usize>,
    pub values: Vec<f32>,
}

type Tensors = HashMap<String, Weights>;

fn get<'a>(t: &'a Tensors, name: &str, dims: usize) -> Result<&'a Weights, Error> {
    let w = t.get(name).ok_or_else(|| Error::Model(format!("the weights have no tensor [{name}]")))?;
    if w.shape.len() != dims {
        return Err(Error::Model(format!("tensor [{name}] has shape {:?}", w.shape)));
    }
    Ok(w)
}

/// `c[m×n] = a[m×k] · b[k×n]`, each given by its row and column strides.
#[allow(clippy::too_many_arguments)]
fn matmul(m: usize, n: usize, k: usize, c: &mut [f32], c_rs: usize, a: &[f32], a_rs: usize, a_cs: usize, b: &[f32], b_rs: usize, b_cs: usize) {
    assert!(m == 0 || n == 0 || ((m - 1) * c_rs + n <= c.len()));
    assert!(m == 0 || k == 0 || ((m - 1) * a_rs + (k - 1) * a_cs < a.len()));
    assert!(k == 0 || n == 0 || ((k - 1) * b_rs + (n - 1) * b_cs < b.len()));
    // SAFETY: every element the strides reach is within its slice (asserted above), and `c`
    // doesn't overlap the inputs (distinct borrows).
    unsafe {
        gemm::gemm(
            m,
            n,
            k,
            c.as_mut_ptr(),
            1,
            c_rs as isize,
            false,
            a.as_ptr(),
            a_cs as isize,
            a_rs as isize,
            b.as_ptr(),
            b_cs as isize,
            b_rs as isize,
            0.0,
            1.0,
            false,
            false,
            false,
            gemm::Parallelism::None,
        )
    }
}

enum Linear {
    /// `[out, in]` as given, and the bias.
    F32 { weight: Vec<f32>, bias: Vec<f32>, k: usize, out: usize },
    Int8(QLinear),
}

impl Linear {
    fn load(t: &Tensors, name: &str, kind: Kind) -> Result<Linear, Error> {
        let w = get(t, &format!("{name}.weight"), 2)?;
        let bias = get(t, &format!("{name}.bias"), 1)?.values.clone();
        let (out, k) = (w.shape[0], w.shape[1]);
        if bias.len() != out {
            return Err(Error::Model(format!("[{name}] has {} biases for {out} outputs", bias.len())));
        }
        Ok(match kind {
            Kind::F32 => Linear::F32 { weight: w.values.clone(), bias, k, out },
            Kind::PerTensor | Kind::PerChannel => Linear::Int8(QLinear::new(&w.values, bias, out, k, kind == Kind::PerChannel)),
        })
    }

    /// `x`: `[rows, in]`; `[rows, out]`.
    fn forward(&self, x: &[f32], rows: usize) -> Vec<f32> {
        match self {
            Linear::F32 { weight, bias, k, out } => {
                let mut y = vec![0f32; rows * out];
                // `x · Wᵀ`: W's rows are the columns.
                matmul(rows, *out, *k, &mut y, *out, x, *k, 1, weight, 1, *k);
                for row in y.chunks_exact_mut(*out) {
                    for (v, b) in row.iter_mut().zip(bias) {
                        *v += b;
                    }
                }
                y
            }
            Linear::Int8(q) => q.forward(x, rows),
        }
    }

    fn bytes(&self) -> u64 {
        match self {
            Linear::F32 { weight, bias, .. } => 4 * (weight.len() + bias.len()) as u64,
            Linear::Int8(q) => q.bytes(),
        }
    }
}

struct Norm {
    weight: Vec<f32>,
    bias: Vec<f32>,
}

impl Norm {
    fn load(t: &Tensors, name: &str) -> Result<Norm, Error> {
        Ok(Norm { weight: get(t, &format!("{name}.weight"), 1)?.values.clone(), bias: get(t, &format!("{name}.bias"), 1)?.values.clone() })
    }

    fn forward(&self, x: &mut [f32]) {
        ops::layer_norm(x, &self.weight, &self.bias, EPS);
    }
}

struct Layer {
    query: Linear,
    key: Linear,
    value: Linear,
    attention_out: Linear,
    attention_norm: Norm,
    intermediate: Linear,
    out: Linear,
    out_norm: Norm,
}

impl Layer {
    fn load(t: &Tensors, i: usize, precision: Precision) -> Result<Layer, Error> {
        let p = format!("encoder.layer.{i}");
        let (attention, (intermediate, out)) = match precision {
            Precision::F32 => (Kind::F32, (Kind::F32, Kind::F32)),
            // A model with more layers than E5's quantizes the rest like its last.
            Precision::Int8 => (Kind::PerTensor, ELASTIC_X86_FFN[i.min(ELASTIC_X86_FFN.len() - 1)]),
        };
        let linear = |name: &str, kind: Kind| Linear::load(t, &format!("{p}.{name}"), kind);
        Ok(Layer {
            query: linear("attention.self.query", attention)?,
            key: linear("attention.self.key", attention)?,
            value: linear("attention.self.value", attention)?,
            attention_out: linear("attention.output.dense", attention)?,
            attention_norm: Norm::load(t, &format!("{p}.attention.output.LayerNorm"))?,
            intermediate: linear("intermediate.dense", intermediate)?,
            out: linear("output.dense", out)?,
            out_norm: Norm::load(t, &format!("{p}.output.LayerNorm"))?,
        })
    }

    /// `x`: `[seq, hidden]`.
    fn forward(&self, x: &[f32], seq: usize, hidden: usize) -> Vec<f32> {
        let heads = hidden / HEAD_SIZE;
        let q = self.query.forward(x, seq);
        let k = self.key.forward(x, seq);
        let v = self.value.forward(x, seq);
        let divisor = (HEAD_SIZE as f32).sqrt();
        let mut context = vec![0f32; seq * hidden];
        let mut scores = vec![0f32; seq * seq];
        for h in 0..heads {
            let o = h * HEAD_SIZE;
            // Q_h · K_hᵀ: rows of Q_h, and K_h's rows as columns.
            matmul(seq, seq, HEAD_SIZE, &mut scores, seq, &q[o..], hidden, 1, &k[o..], 1, hidden);
            ops::softmax_rows_scaled(&mut scores, seq, divisor);
            matmul(seq, HEAD_SIZE, seq, &mut context[o..], hidden, &scores, seq, 1, &v[o..], hidden, 1);
        }
        let mut attended = self.attention_out.forward(&context, seq);
        for (a, r) in attended.iter_mut().zip(x) {
            *a += r;
        }
        for row in attended.chunks_exact_mut(hidden) {
            self.attention_norm.forward(row);
        }
        let mut inner = self.intermediate.forward(&attended, seq);
        ops::gelu(&mut inner);
        let mut out = self.out.forward(&inner, seq);
        for (o, r) in out.iter_mut().zip(&attended) {
            *o += r;
        }
        for row in out.chunks_exact_mut(hidden) {
            self.out_norm.forward(row);
        }
        out
    }

    fn bytes(&self) -> u64 {
        [&self.query, &self.key, &self.value, &self.attention_out, &self.intermediate, &self.out].iter().map(|l| l.bytes()).sum::<u64>()
            + [&self.attention_norm, &self.out_norm].iter().map(|n| 4 * (n.weight.len() + n.bias.len()) as u64).sum::<u64>()
    }
}

/// The encoder.
pub struct Bert {
    words: Vec<f32>,
    vocab: usize,
    positions: Vec<f32>,
    max_positions: usize,
    token_type: Vec<f32>,
    norm: Norm,
    layers: Vec<Layer>,
    hidden: usize,
    intermediate: usize,
}

impl Bert {
    pub fn new(t: &Tensors, precision: Precision) -> Result<Bert, Error> {
        let words = get(t, "embeddings.word_embeddings.weight", 2)?;
        let (vocab, hidden) = (words.shape[0], words.shape[1]);
        if hidden % HEAD_SIZE != 0 {
            return Err(Error::Model(format!("hidden size [{hidden}] isn't a multiple of the head size [{HEAD_SIZE}]")));
        }
        let positions = get(t, "embeddings.position_embeddings.weight", 2)?;
        let token_type = get(t, "embeddings.token_type_embeddings.weight", 2)?;
        if positions.shape[1] != hidden || token_type.shape[1] != hidden {
            return Err(Error::Model("the embeddings' widths differ".into()));
        }
        let mut layers = Vec::new();
        while t.contains_key(&format!("encoder.layer.{}.attention.self.query.weight", layers.len())) {
            layers.push(Layer::load(t, layers.len(), precision)?);
        }
        if layers.is_empty() {
            return Err(Error::Model("the weights have no encoder layers".into()));
        }
        let intermediate = get(t, "encoder.layer.0.intermediate.dense.bias", 1)?.shape[0];
        Ok(Bert {
            words: words.values.clone(),
            vocab,
            positions: positions.values.clone(),
            max_positions: positions.shape[0],
            token_type: token_type.values[..hidden].to_vec(),
            norm: Norm::load(t, "embeddings.LayerNorm")?,
            layers,
            hidden,
            intermediate,
        })
    }

    /// Bytes the encoder's weights hold.
    pub fn bytes(&self) -> u64 {
        4 * (self.words.len() + self.positions.len() + self.token_type.len() + self.norm.weight.len() + self.norm.bias.len()) as u64
            + self.layers.iter().map(Layer::bytes).sum::<u64>()
    }

    /// Embedding width.
    pub fn hidden(&self) -> usize {
        self.hidden
    }

    /// Positions the model has embeddings for.
    pub fn max_positions(&self) -> usize {
        self.max_positions
    }

    /// Peak bytes of activations for an input of `seq` tokens: a few `[seq, hidden]` and
    /// `[seq, intermediate]` buffers and one head's `[seq, seq]` scores, alive together.
    pub fn activation_bytes(&self, seq: usize) -> u64 {
        4 * (8 * seq * self.hidden + 3 * seq * self.intermediate + seq * seq) as u64
    }

    /// The last hidden states, `[seq, hidden]`, of one input's token ids.
    pub fn forward(&self, ids: &[u32]) -> Result<Vec<f32>, Error> {
        let (seq, h) = (ids.len(), self.hidden);
        if seq > self.max_positions {
            return Err(Error::Model(format!("[{seq}] tokens is more than the model's [{}] positions", self.max_positions)));
        }
        let mut x = vec![0f32; seq * h];
        for (p, (&id, row)) in ids.iter().zip(x.chunks_exact_mut(h)).enumerate() {
            let id = id as usize;
            if id >= self.vocab {
                return Err(Error::Model(format!("token id [{id}] is outside the vocabulary")));
            }
            let (w, pos) = (&self.words[id * h..(id + 1) * h], &self.positions[p * h..(p + 1) * h]);
            for i in 0..h {
                row[i] = (w[i] + self.token_type[i]) + pos[i];
            }
            self.norm.forward(row);
        }
        for layer in &self.layers {
            x = layer.forward(&x, seq, h);
        }
        Ok(x)
    }
}
