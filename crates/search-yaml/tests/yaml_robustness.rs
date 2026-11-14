//! The YAML parser and the suite loader must never panic, whatever the input: a malformed test
//! file is a reported parse failure, not a crashed run. The corpus is the vendored
//! elasticsearch-clients-tests suite, mutated deterministically (truncations, deleted and
//! duplicated bytes, swapped indentation).

mod common;

use std::path::{Path, PathBuf};

use search_yaml::suite::Suite;
use search_yaml::yaml::load_documents;

fn corpus() -> Vec<(PathBuf, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("suites/clients-tests/tests");
    let mut files = search_yaml::runner::discover(&root).expect("vendored suite");
    files.sort();
    files.into_iter().map(|f| (f.clone(), std::fs::read_to_string(&f).expect("utf-8 test file"))).collect()
}

/// A tiny deterministic generator (xorshift), so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn char_floor(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[test]
fn every_vendored_file_parses() {
    let files = corpus();
    assert!(files.len() > 100, "the vendored suite is missing");
    for (path, text) in &files {
        let suite = Suite::parse(text, path.clone(), String::new(), String::new());
        assert!(suite.is_ok(), "{}: {}", path.display(), suite.unwrap_err());
    }
}

#[test]
fn mutated_files_never_panic() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    const SPECIALS: &[&str] = &["{", "}", "[", "]", "\"", "'", ":", "- ", "|", ">", "#", "\n", "  ", "&a", "*a", "!!str ", "---\n", "\\", "\t"];
    for (path, text) in corpus() {
        for round in 0..24 {
            let mut s = text.clone();
            match round % 4 {
                0 => {
                    let cut = char_floor(&s, rng.below(s.len()));
                    s.truncate(cut);
                }
                1 => {
                    for _ in 0..3 {
                        if s.is_empty() {
                            break;
                        }
                        let at = char_floor(&s, rng.below(s.len()));
                        let end = char_floor(&s, (at + 1 + rng.below(8)).min(s.len()));
                        s.replace_range(at..end, "");
                    }
                }
                2 => {
                    for _ in 0..3 {
                        let at = char_floor(&s, rng.below(s.len() + 1).min(s.len()));
                        s.insert_str(at, SPECIALS[rng.below(SPECIALS.len())]);
                    }
                }
                _ => {
                    // Shift the indentation of one line.
                    let lines: Vec<&str> = s.lines().collect();
                    let target = rng.below(lines.len());
                    let shifted: Vec<String> = lines
                        .iter()
                        .enumerate()
                        .map(|(i, l)| if i == target { format!("   {l}") } else { (*l).to_owned() })
                        .collect();
                    s = shifted.join("\n");
                }
            }
            let outcome = std::panic::catch_unwind(|| {
                let _ = load_documents(&s);
                let _ = Suite::parse(&s, PathBuf::from("x.yml"), String::new(), String::new());
            });
            assert!(outcome.is_ok(), "{} (round {round}) panicked on:\n{s}", path.display());
        }
    }
}

#[test]
fn pathological_inputs() {
    let deep_flow = format!("a: {}{}\n", "[".repeat(5000), "]".repeat(5000));
    let deep_block: String = (0..2000).map(|i| format!("{}- \n", " ".repeat(i))).collect();
    let cases = [
        String::new(),
        "\u{feff}".into(),
        "---".into(),
        "...".into(),
        ":".into(),
        "- - - -".into(),
        "{".into(),
        "\"\\".into(),
        "'".into(),
        "|".into(),
        "a: |9\n  x\n".into(),
        "a: !!int x\n".into(),
        "*nope\n".into(),
        "&a [*a]\n".into(),
        "a:\n\t- b\n".into(),
        deep_flow,
        deep_block,
        "\"\\uD800\"".into(),
        "\"\\U0011FFFF\"".into(),
        "? complex\n: key\n".into(),
        "a: x\0é\n- \0".into(),
        "!<tag:yaml.org,2002:str".into(),
        "\"\\é\"".into(),
    ];
    for s in &cases {
        let outcome = std::panic::catch_unwind(|| load_documents(s).map(|d| d.len()));
        assert!(outcome.is_ok(), "panicked on {:?}", s.chars().take(80).collect::<String>());
    }
    // Nesting is bounded with an error, not a stack overflow.
    assert!(load_documents(&cases[15]).unwrap_err().message.contains("nesting"));
    assert!(load_documents(&cases[16]).unwrap_err().message.contains("nesting"));
    let fine = format!("a: {}1{}\n", "[".repeat(100), "]".repeat(100));
    assert!(load_documents(&fine).is_ok());
}
