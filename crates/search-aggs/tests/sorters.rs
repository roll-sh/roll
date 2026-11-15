//! `util::intro_sort` against Lucene's `IntroSorter` itself (`fixtures/aggs-sorters.json`,
//! from `fixtures/oracle/Sorters.java`): arrays full of ties must end in the same permutation.

use search_aggs::Json;
use search_aggs::util::{IntroSortable, intro_sort};

struct Tagged {
    v: Vec<f64>,
    tag: Vec<usize>,
    pivot: f64,
}

impl IntroSortable for Tagged {
    fn swap(&mut self, i: usize, j: usize) {
        self.v.swap(i, j);
        self.tag.swap(i, j);
    }
    fn set_pivot(&mut self, i: usize) {
        self.pivot = self.v[i];
    }
    fn compare_pivot(&self, j: usize) -> std::cmp::Ordering {
        self.pivot.total_cmp(&self.v[j])
    }
}

#[test]
fn intro_sort_permutes_like_lucene() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aggs-sorters.json");
    let cases = Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cases = cases.as_array().unwrap();
    assert!(cases.len() >= 100);
    for (c, case) in cases.iter().enumerate() {
        let v: Vec<f64> = case.get("values").unwrap().as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        let want: Vec<usize> = case.get("order").unwrap().as_array().unwrap().iter().map(|x| x.as_i64().unwrap() as usize).collect();
        let n = v.len();
        let mut s = Tagged { v, tag: (0..n).collect(), pivot: 0.0 };
        intro_sort(&mut s, 0, n);
        assert_eq!(s.tag, want, "case {c} (n = {n})");
    }
}
