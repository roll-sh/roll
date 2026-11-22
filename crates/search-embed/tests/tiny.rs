//! The model's code paths on the tiny made-up model (`search_embed::tiny`).

use search_embed::model::Precision;
use search_embed::{E5, MAX_TOKENS};

fn model() -> (tempfile::TempDir, E5) {
    let dir = tempfile::tempdir().unwrap();
    search_embed::tiny::write(dir.path()).unwrap();
    let m = E5::load(dir.path()).unwrap();
    (dir, m)
}

fn close(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
}

#[test]
fn embeddings_are_unit_length_and_independent_of_their_batch() {
    let (dir, m) = model();
    let texts = ["a", "hello world", "the quick brown fox jumps over the lazy dog", ""];
    let alone: Vec<Vec<f32>> = texts.iter().map(|t| m.embed(&[t]).unwrap().remove(0)).collect();
    let together = m.embed(&texts).unwrap();
    for (a, b) in alone.iter().zip(&together) {
        assert_eq!(a.len(), search_embed::tiny::HIDDEN);
        assert!(close(a, b), "an input's embedding depends on the others in its call");
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm {norm}");
    }
    assert!(!close(&together[0], &together[1]));
    // Both precisions run, and the 8-bit layers stay close to the float ones.
    let f = E5::load_with(dir.path(), Precision::F32).unwrap();
    let i = E5::load_with(dir.path(), Precision::Int8).unwrap();
    for t in texts {
        let (a, b) = (f.embed(&[t]).unwrap().remove(0), i.embed(&[t]).unwrap().remove(0));
        let cos: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!(cos > 0.99, "{t:?}: {cos}");
    }
}

#[test]
fn long_inputs_are_cut_to_512_tokens() {
    let (_d, m) = model();
    // Each `▁x` word is one token: `<s>`, 510 words, `</s>`.
    let words: Vec<String> = (0..2000).map(|i| ((b'a' + (i % 26) as u8) as char).to_string()).collect();
    let long = words.join(" ");
    let ids = m.tokenize(&long).unwrap();
    assert_eq!(ids.len(), MAX_TOKENS);
    assert_eq!((ids[0], ids[MAX_TOKENS - 1]), (0, 2));
    assert_eq!(ids, m.tokenize(&words[..510].join(" ")).unwrap());
    assert!(close(&m.embed(&[&long]).unwrap()[0], &m.embed(&[&words[..510].join(" ")]).unwrap()[0]));
    // Only a bounded start is tokenized, with the same tokens: repeated spaces collapse, so a
    // long run of them still counts as one; a run without spaces is cut by bytes.
    let spaced = format!("a{}b {}", " ".repeat(40_000), words.join(" "));
    assert_eq!(m.tokenize(&spaced).unwrap()[..3], [0, m.tokenize("a").unwrap()[1], m.tokenize("b").unwrap()[1]]);
    let unspaced = "a".repeat(30_000);
    let full = m.tokenize(&unspaced).unwrap();
    assert_eq!(full.len(), MAX_TOKENS);
    assert!(m.cut(&unspaced).len() < unspaced.len());
    assert_eq!(m.tokenize(&"a".repeat(2 << 20)).unwrap(), full);
    // Long inputs of every shape give the tokens of the whole text: words throughout, and long
    // runs of what the normaliser deletes or turns into spaces before the words.
    let article = words[..600].join(" ");
    for text in [words.join(" ").repeat(10), format!("{}{article}", "\u{200b}".repeat(33_000)), format!("{}{article}", "\u{1}".repeat(40_000))] {
        assert!(text.len() > m.cut_bytes());
        assert_eq!(m.tokenize(&text).unwrap(), m.tokenize_uncut(&text).unwrap(), "{:?}…", text.chars().take(8).collect::<String>());
    }
    // The prefix goes before the cut text, and costs no copy of the text.
    assert_eq!(m.tokenize_prefixed("query: ", "a b").unwrap(), m.tokenize("query: a b").unwrap());
}
