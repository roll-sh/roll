//! Behaviour checked against outputs captured from Elasticsearch 9.5.5 (`tests/fixtures`).

use search_codec::automaton::dfa::DEFAULT_WORK_LIMIT;
use search_codec::automaton::regexp::{self, RegexpError};
use search_codec::vectors::similarity_score;
use search_codec::*;
use serde_json::Value;

fn load(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect()
}

#[test]
fn fuzzy_matches_elasticsearch() {
    let f = load("es-9.5.5-automata.json");
    let terms = strs(&f["terms"]);
    for case in f["fuzzy"].as_array().unwrap() {
        let value = case["value"].as_str().unwrap();
        let edits = case["edits"].as_u64().unwrap() as u32;
        let prefix = case["prefix"].as_u64().unwrap() as usize;
        let transpositions = case["transpositions"].as_bool().unwrap();
        let dfa = Dfa::levenshtein(value, edits, prefix, transpositions);
        let mut got: Vec<String> = terms.iter().filter(|t| dfa.matches(t.as_bytes())).cloned().collect();
        got.sort();
        assert_eq!(got, strs(&case["matches"]), "fuzzy {case}");
    }
}

#[test]
fn regexp_matches_elasticsearch() {
    let f = load("es-9.5.5-automata.json");
    let terms = strs(&f["terms"]);
    for case in f["regexp"].as_array().unwrap() {
        let pattern = case["pattern"].as_str().unwrap();
        let result = regexp::compile(pattern, regexp::ALL, false, DEFAULT_WORK_LIMIT);
        match (case.get("matches"), result) {
            (Some(want), Ok(dfa)) => {
                let mut got: Vec<String> = terms.iter().filter(|t| dfa.matches(t.as_bytes())).cloned().collect();
                got.sort();
                assert_eq!(got, strs(want), "regexp {pattern}");
            }
            (None, Err(RegexpError::Syntax(msg))) => {
                assert_eq!(case["error_type"], "illegal_argument_exception", "regexp {pattern}");
                assert_eq!(msg, case["error"].as_str().unwrap(), "regexp {pattern}");
            }
            (None, Err(RegexpError::TooComplex(t))) => {
                assert_eq!(case["error_type"], "too_complex_to_determinize_exception", "regexp {pattern}");
                assert!(t.to_string().starts_with("Determinizing automaton with "));
                assert!(t.to_string().ends_with(" would require more than 10000 effort."));
            }
            (want, got) => panic!("regexp {pattern}: expected {want:?}, got {got:?}"),
        }
    }
}

#[test]
fn vector_scores_match_elasticsearch() {
    let f = load("es-9.5.5-vectors.json");
    for case in f["cases"].as_array().unwrap() {
        let element = match case["element"].as_str().unwrap() {
            "float" => VectorElement::Float32,
            "byte" => VectorElement::Byte,
            _ => VectorElement::Bit,
        };
        let similarity = match case["similarity"].as_str().unwrap() {
            "cosine" => VectorSimilarity::Cosine,
            "l2_norm" => VectorSimilarity::L2Norm,
            "dot_product" => VectorSimilarity::DotProduct,
            _ => VectorSimilarity::MaxInnerProduct,
        };
        let spec = VectorSpec {
            dims: case["dims"].as_u64().unwrap() as u32,
            element,
            similarity,
            hnsw: None,
            quantization: VectorQuantization::None,
        };
        let enc = |v: &Value| -> Vec<u8> {
            v.as_array()
                .unwrap()
                .iter()
                .flat_map(|x| match element {
                    VectorElement::Float32 => (x.as_f64().unwrap() as f32).to_le_bytes().to_vec(),
                    _ => vec![x.as_i64().unwrap() as i8 as u8],
                })
                .collect()
        };
        let q = enc(&case["query"]);
        for (doc, want) in case["docs"].as_array().unwrap().iter().zip(case["scores"].as_array().unwrap()) {
            let got = similarity_score(&spec, &q, &enc(doc));
            let want = want.as_f64().unwrap() as f32;
            assert!((got - want).abs() <= want.abs() * 2e-6, "{case}: got {got}, want {want}");
        }
    }
}

#[test]
fn geo_encoding_matches_elasticsearch() {
    use search_codec::points::encode;
    let f = load("es-9.5.5-geo.json");
    for p in f["points"].as_array().unwrap() {
        let (lat, lon) = (p["lat"].as_f64().unwrap(), p["lon"].as_f64().unwrap());
        let dv = p["docvalue"].as_array().unwrap();
        let got = (
            encode::decode_latitude(encode::encode_latitude(lat)),
            encode::decode_longitude(encode::encode_longitude(lon)),
        );
        assert_eq!(got, (dv[0].as_f64().unwrap(), dv[1].as_f64().unwrap()), "{p}");
    }
}

#[test]
fn norms_decode_like_elasticsearch() {
    let f = load("es-9.5.5-norms.json");
    for c in f["cases"].as_array().unwrap() {
        let len = c["length"].as_u64().unwrap() as u32;
        let dl = c["dl"].as_u64().unwrap() as u32;
        assert_eq!(byte4_to_int(int_to_byte4(len)), dl, "length {len}");
    }
}

#[test]
fn half_floats_round_like_elasticsearch() {
    use search_codec::points::encode;
    let f = load("es-9.5.5-half-float.json");
    for c in f["cases"].as_array().unwrap() {
        let v = c["value"].as_f64().unwrap() as f32;
        let want = c["docvalue"].as_f64().unwrap();
        assert_eq!(encode::sortable_f16(encode::f16_sortable(v)) as f64, want, "{c}");
    }
}

#[test]
fn wildcard_matches_elasticsearch() {
    let f = load("es-9.5.5-wildcard.json");
    let terms = strs(&f["terms"]);
    for case in f["wildcard"].as_array().unwrap() {
        let pattern = case["pattern"].as_str().unwrap();
        if pattern.is_empty() {
            continue; // Elasticsearch answers an empty wildcard with no docs before building an automaton.
        }
        let ci = case["case_insensitive"].as_bool().unwrap();
        let dfa = Dfa::wildcard(pattern, ci, DEFAULT_WORK_LIMIT).unwrap();
        let mut got: Vec<String> = terms.iter().filter(|t| dfa.matches(t.as_bytes())).cloned().collect();
        got.sort();
        let mut want = strs(&case["matches"]);
        want.sort();
        assert_eq!(got, want, "wildcard {pattern:?} case_insensitive={ci}");
    }
}

/// Pseudo-random terms and queries (`gen_automata_random.py`): every fuzzy, regexp, wildcard and
/// prefix automaton must accept exactly the terms Elasticsearch matched, and invalid regexps
/// must fail with Lucene's message.
#[test]
fn random_automata_match_elasticsearch() {
    let f = load("es-9.5.5-automata-random.json");
    let terms = strs(&f["terms"]);
    let want_of = |case: &Value| -> Vec<usize> {
        case["matches"].as_array().unwrap().iter().map(|i| i.as_u64().unwrap() as usize).collect()
    };
    let matching = |aut: &dyn Automaton| -> Vec<usize> {
        terms.iter().enumerate().filter(|(_, t)| aut.matches(t.as_bytes())).map(|(i, _)| i).collect()
    };
    let show = |idx: &[usize]| idx.iter().map(|&i| terms[i].as_str()).collect::<Vec<_>>();
    let mut failures = Vec::new();
    for case in f["fuzzy"].as_array().unwrap() {
        let value = case["value"].as_str().unwrap();
        let dfa = Dfa::levenshtein(
            value,
            case["edits"].as_u64().unwrap() as u32,
            case["prefix"].as_u64().unwrap() as usize,
            case["transpositions"].as_bool().unwrap(),
        );
        let (got, want) = (matching(&dfa), want_of(case));
        if got != want {
            failures.push(format!("fuzzy {case}: got {:?} want {:?}", show(&got), show(&want)));
        }
    }
    for case in f["regexp"].as_array().unwrap() {
        let pattern = case["pattern"].as_str().unwrap();
        let ci = case["case_insensitive"].as_bool().unwrap();
        let collapsed = regexp::collapse_consecutive_quantifiers(pattern);
        match (case.get("matches"), regexp::compile(&collapsed, regexp::ALL, ci, DEFAULT_WORK_LIMIT)) {
            (Some(_), Ok(dfa)) => {
                let (got, want) = (matching(&dfa), want_of(case));
                if got != want {
                    failures.push(format!("regexp {pattern:?} ci={ci}: got {:?} want {:?}", show(&got), show(&want)));
                }
            }
            (None, Err(RegexpError::Syntax(msg))) if case["error"].as_str() == Some(msg.as_str()) => {}
            (None, Err(RegexpError::TooComplex(_))) if case["error_type"] == "too_complex_to_determinize_exception" => {}
            (want, got) => failures.push(format!("regexp {pattern:?}: expected {want:?} / {}, got {got:?}", case["error"])),
        }
    }
    for case in f["wildcard"].as_array().unwrap() {
        let pattern = case["pattern"].as_str().unwrap();
        let ci = case["case_insensitive"].as_bool().unwrap();
        let dfa = Dfa::wildcard(pattern, ci, DEFAULT_WORK_LIMIT).unwrap();
        let (got, want) = (matching(&dfa), want_of(case));
        if got != want {
            failures.push(format!("wildcard {pattern:?} ci={ci}: got {:?} want {:?}", show(&got), show(&want)));
        }
    }
    for case in f["prefix"].as_array().unwrap() {
        let prefix = case["prefix"].as_str().unwrap();
        let ci = case["case_insensitive"].as_bool().unwrap();
        let got = if ci {
            matching(&Dfa::prefix_case_insensitive(prefix, DEFAULT_WORK_LIMIT).unwrap())
        } else {
            matching(&Prefix::new(prefix))
        };
        let want = want_of(case);
        if got != want {
            failures.push(format!("prefix {prefix:?} ci={ci}: got {:?} want {:?}", show(&got), show(&want)));
        }
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}
