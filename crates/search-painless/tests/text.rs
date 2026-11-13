//! Every locale of the JDK formats like Elasticsearch's (`tests/fixtures/locale_formats.json`,
//! captured with `NumberFormat.get{Number,Currency,Percent}Instance(locale)`): the number
//! `-1234567.891`, the same amount as currency, and `-0.256` as a percentage.

use std::path::Path;

use search_painless::text::{number_format, Style};
use serde_json::Value as Json;

#[test]
fn locale_number_formats() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/locale_formats.json");
    let rows: Json = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut failures = Vec::new();
    for r in rows.as_array().unwrap() {
        let r: Vec<&str> = r.as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
        let (locale, number, currency, percent) = (r[0], r[1], r[2], r[3]);
        let got = [
            number_format(locale, Style::Number).format_double(-1234567.891).unwrap(),
            number_format(locale, Style::Currency).format_double(-1234567.891).unwrap(),
            number_format(locale, Style::Percent).format_double(-0.256).unwrap(),
        ];
        if got != [number, currency, percent] {
            failures.push(format!("{locale:?}: want {:?}, got {got:?}", [number, currency, percent]));
        }
    }
    for f in failures.iter().take(20) {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "{} of {} locales format differently", failures.len(), rows.as_array().unwrap().len());
}
