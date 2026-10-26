//! CSV and JSON output records.

use crate::error::{Result, SelectError};
use crate::eval::MAX_RECORD_BYTES;
use crate::request::{Output as OutputSpec, QuoteFields};
use crate::value::{Value, write_json, write_json_str};

/// One output column's value: raw CSV field text, or a value.
pub(crate) enum Cell<'a> {
    Text(&'a str),
    Value(&'a Value),
}

enum Format {
    Csv { fd: String, rd: String, quote: String, esc: String, always: bool },
    Json { rd: String },
}

pub(crate) struct Output {
    pub buf: Vec<u8>,
    /// Bytes of records produced so far.
    pub produced: u64,
    format: Format,
    scratch: String,
}

impl Output {
    pub fn new(spec: &OutputSpec) -> Output {
        let format = match spec {
            OutputSpec::Csv(c) => Format::Csv {
                fd: c.field_delimiter.clone(),
                rd: c.record_delimiter.clone(),
                quote: c.quote_character.clone(),
                esc: c.quote_escape_character.clone(),
                always: c.quote_fields == QuoteFields::Always,
            },
            OutputSpec::Json(j) => Format::Json { rd: j.record_delimiter.clone() },
        };
        Output { buf: Vec::new(), produced: 0, format, scratch: String::new() }
    }

    /// Write one record. A record over 1 MiB is OverMaxRecordSize (and nothing is written).
    pub fn record(&mut self, cells: &[(&str, Cell<'_>)]) -> Result<()> {
        let s = &mut self.scratch;
        s.clear();
        match &self.format {
            Format::Csv { fd, rd, quote, esc, always } => {
                for (i, (_, cell)) in cells.iter().enumerate() {
                    if i > 0 {
                        s.push_str(fd);
                    }
                    let owned;
                    let text: Option<&str> = match cell {
                        Cell::Text(t) => Some(t),
                        Cell::Value(Value::Str(t)) => Some(t),
                        Cell::Value(v) => {
                            owned = v.text();
                            owned.as_deref()
                        }
                    };
                    let text = text.unwrap_or("");
                    let needs = *always
                        || text.contains(fd.as_str())
                        || text.contains(quote.as_str())
                        || text.contains(rd.as_str())
                        || text.contains(['\n', '\r']);
                    if needs {
                        s.push_str(quote);
                        for c in text.chars() {
                            let mut b = [0u8; 4];
                            let cs: &str = c.encode_utf8(&mut b);
                            if cs == quote.as_str() || (esc != quote && cs == esc.as_str()) {
                                s.push_str(esc);
                            }
                            s.push(c);
                        }
                        s.push_str(quote);
                    } else {
                        s.push_str(text);
                    }
                    if s.len() > MAX_RECORD_BYTES {
                        return Err(SelectError::over_max_record());
                    }
                }
                s.push_str(rd);
            }
            Format::Json { rd } => {
                s.push('{');
                let mut first = true;
                for (name, cell) in cells {
                    if matches!(cell, Cell::Value(Value::Missing)) {
                        continue;
                    }
                    if !first {
                        s.push(',');
                    }
                    first = false;
                    write_json_str(s, name);
                    s.push(':');
                    match cell {
                        Cell::Text(t) => write_json_str(s, t),
                        Cell::Value(v) => write_json(s, v),
                    }
                    if s.len() > MAX_RECORD_BYTES {
                        return Err(SelectError::over_max_record());
                    }
                }
                s.push('}');
                s.push_str(rd);
            }
        }
        if s.len() > MAX_RECORD_BYTES + 8 {
            return Err(SelectError::over_max_record());
        }
        self.buf.extend_from_slice(s.as_bytes());
        self.produced += s.len() as u64;
        if s.capacity() > 64 * 1024 {
            *s = String::new();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{CsvOutput, JsonOutput};

    fn csv(spec: CsvOutput, cells: &[(&str, Cell<'_>)]) -> String {
        let mut o = Output::new(&OutputSpec::Csv(spec));
        o.record(cells).unwrap();
        String::from_utf8(o.buf).unwrap()
    }

    #[test]
    fn csv_quoting() {
        let v = [Value::Int(1), Value::Float(2.5), Value::Null, Value::Bool(true), Value::Str("a,b".into()), Value::Str("say \"hi\"".into())];
        let cells: Vec<(&str, Cell)> = v.iter().map(|x| ("", Cell::Value(x))).chain([("", Cell::Text("x\ny"))]).collect();
        assert_eq!(csv(CsvOutput::default(), &cells), "1,2.5,,true,\"a,b\",\"say \"\"hi\"\"\",\"x\ny\"\n");
        let always = CsvOutput { quote_fields: QuoteFields::Always, ..CsvOutput::default() };
        assert_eq!(csv(always, &cells[..3]), "\"1\",\"2.5\",\"\"\n");
        let custom = CsvOutput {
            quote_fields: QuoteFields::AsNeeded,
            field_delimiter: "|".into(),
            record_delimiter: "\r\n".into(),
            quote_character: "'".into(),
            quote_escape_character: "\\".into(),
        };
        assert_eq!(csv(custom, &[("", Cell::Text("it's")), ("", Cell::Text("a|b")), ("", Cell::Text("c\\d")), ("", Cell::Text("e,f"))]), "'it\\'s'|'a|b'|c\\d|e,f\r\n");
    }

    #[test]
    fn json_records() {
        let mut o = Output::new(&OutputSpec::Json(JsonOutput::default()));
        let list = Value::List(vec![Value::Int(1), Value::Struct(vec![("k".into(), Value::Str("v".into()))])]);
        o.record(&[("a", Cell::Value(&Value::Int(1))), ("gone", Cell::Value(&Value::Missing)), ("n", Cell::Value(&Value::Null)), ("l", Cell::Value(&list)), ("t", Cell::Text("x\"y"))]).unwrap();
        o.record(&[("a", Cell::Value(&Value::Missing))]).unwrap();
        assert_eq!(String::from_utf8(o.buf).unwrap(), "{\"a\":1,\"n\":null,\"l\":[1,{\"k\":\"v\"}],\"t\":\"x\\\"y\"}\n{}\n");
    }

    #[test]
    fn records_over_1mib_fail() {
        let mut o = Output::new(&OutputSpec::Json(JsonOutput::default()));
        let big = Value::Str("x".repeat(MAX_RECORD_BYTES));
        assert_eq!(o.record(&[("a", Cell::Value(&big))]).unwrap_err().code, "OverMaxRecordSize");
        assert!(o.buf.is_empty());
        let mut o = Output::new(&OutputSpec::Csv(CsvOutput::default()));
        let half = Value::Str("x".repeat(MAX_RECORD_BYTES / 2 + 1));
        assert_eq!(o.record(&[("a", Cell::Value(&half)), ("b", Cell::Value(&half))]).unwrap_err().code, "OverMaxRecordSize");
    }
}
