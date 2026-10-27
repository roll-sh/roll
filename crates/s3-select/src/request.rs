//! The SelectObjectContent request body (`<SelectObjectContentRequest>`).

use quick_xml::events::Event;

use crate::error::{Result, SelectError};

/// A SelectObjectContent request body, parsed and validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectRequest {
    pub expression: String,
    pub input: Input,
    pub compression: Compression,
    pub output: Output,
    /// `<RequestProgress><Enabled>true</Enabled>`
    pub progress: bool,
    pub scan_range: Option<ScanRange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Csv(CsvInput),
    Json(JsonInput),
    Parquet,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileHeaderInfo {
    Use,
    Ignore,
    #[default]
    None,
}

/// CSV input. Delimiters are 1–2 characters; the quote, escape and comment characters are one
/// ASCII character each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvInput {
    pub file_header_info: FileHeaderInfo,
    pub field_delimiter: String,
    pub record_delimiter: String,
    pub quote_character: String,
    pub quote_escape_character: String,
    /// Lines starting with this are skipped (AWS's default is `#`; an empty `<Comments/>` turns
    /// comments off).
    pub comments: Option<String>,
    pub allow_quoted_record_delimiter: bool,
}

impl Default for CsvInput {
    fn default() -> Self {
        CsvInput {
            file_header_info: FileHeaderInfo::None,
            field_delimiter: ",".into(),
            record_delimiter: "\n".into(),
            quote_character: "\"".into(),
            quote_escape_character: "\"".into(),
            comments: Some("#".into()),
            allow_quoted_record_delimiter: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JsonType {
    #[default]
    Document,
    Lines,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JsonInput {
    pub kind: JsonType,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    #[default]
    None,
    Gzip,
    Bzip2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Csv(CsvOutput),
    Json(JsonOutput),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuoteFields {
    Always,
    #[default]
    AsNeeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvOutput {
    pub quote_fields: QuoteFields,
    pub field_delimiter: String,
    pub record_delimiter: String,
    pub quote_character: String,
    pub quote_escape_character: String,
}

impl Default for CsvOutput {
    fn default() -> Self {
        CsvOutput {
            quote_fields: QuoteFields::AsNeeded,
            field_delimiter: ",".into(),
            record_delimiter: "\n".into(),
            quote_character: "\"".into(),
            quote_escape_character: "\"".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonOutput {
    /// Default `"\n"`.
    pub record_delimiter: String,
}

impl Default for JsonOutput {
    fn default() -> Self {
        JsonOutput { record_delimiter: "\n".into() }
    }
}

/// `<ScanRange>`: inclusive byte offsets. Only `end` is "the last `end` bytes".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanRange {
    pub start: Option<u64>,
    pub end: Option<u64>,
}

/// Limit on the XML body (the expression itself is limited to 256 KiB).
pub const MAX_REQUEST_BYTES: usize = 1 << 20;
const MAX_XML_DEPTH: usize = 16;
const MAX_XML_NODES: usize = 256;

#[derive(Debug, Default)]
struct Node {
    name: String,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

fn malformed() -> SelectError {
    SelectError::new(
        "MalformedXML",
        "The XML provided was not well formed or did not validate against our published schema. Check the service documentation and try again.",
    )
}

fn parse_xml(body: &[u8]) -> Result<Node> {
    if body.len() > MAX_REQUEST_BYTES {
        return Err(SelectError::new("MalformedXML", "The request body is too large."));
    }
    let text = std::str::from_utf8(body).map_err(|_| malformed())?;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut nodes = 0usize;
    let name_of = |n: &[u8]| std::str::from_utf8(n).map(str::to_string).map_err(|_| malformed());
    loop {
        match reader.read_event().map_err(|_| malformed())? {
            Event::Start(_) | Event::Empty(_) if root.is_some() => return Err(malformed()),
            Event::Start(e) => {
                nodes += 1;
                if stack.len() >= MAX_XML_DEPTH || nodes > MAX_XML_NODES {
                    return Err(malformed());
                }
                stack.push(Node { name: name_of(e.local_name().as_ref())?, ..Node::default() });
            }
            Event::Empty(e) => {
                nodes += 1;
                if nodes > MAX_XML_NODES {
                    return Err(malformed());
                }
                let n = Node { name: name_of(e.local_name().as_ref())?, ..Node::default() };
                match stack.last_mut() {
                    Some(p) => p.children.push(n),
                    None => root = Some(n),
                }
            }
            Event::End(e) => {
                let n = stack.pop().ok_or_else(malformed)?;
                if e.local_name().as_ref() != n.name.as_bytes() {
                    return Err(malformed());
                }
                match stack.last_mut() {
                    Some(p) => p.children.push(n),
                    None => root = Some(n),
                }
            }
            Event::Text(t) => {
                let s = t.decode().map_err(|_| malformed())?;
                match stack.last_mut() {
                    Some(n) => n.text.push_str(&s),
                    None if !s.trim().is_empty() => return Err(malformed()),
                    None => {}
                }
            }
            Event::CData(t) => {
                let s = t.decode().map_err(|_| malformed())?;
                if let Some(n) = stack.last_mut() {
                    n.text.push_str(&s);
                }
            }
            Event::GeneralRef(r) => {
                let s = if r.is_char_ref() {
                    r.resolve_char_ref().map_err(|_| malformed())?.ok_or_else(malformed)?.to_string()
                } else {
                    match r.decode().map_err(|_| malformed())?.as_ref() {
                        "amp" => "&".into(),
                        "lt" => "<".into(),
                        "gt" => ">".into(),
                        "quot" => "\"".into(),
                        "apos" => "'".into(),
                        _ => return Err(malformed()),
                    }
                };
                if let Some(n) = stack.last_mut() {
                    n.text.push_str(&s);
                }
            }
            Event::DocType(_) => return Err(malformed()),
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) => {}
            Event::Eof => break,
        }
    }
    if !stack.is_empty() {
        return Err(malformed());
    }
    root.ok_or_else(malformed)
}

fn missing(what: &str) -> SelectError {
    SelectError::new(
        "MissingRequiredParameter",
        format!("The SelectRequest entity is missing a required parameter ({what}). Check the service documentation and try again."),
    )
}

fn invalid_param(what: impl std::fmt::Display) -> SelectError {
    SelectError::new("InvalidRequestParameter", format!("The value of a parameter in the SelectRequest element is not valid: {what}."))
}

fn bool_of(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" | "" => Ok(false),
        _ => Err(malformed()),
    }
}

/// A delimiter or quote: the default when absent or empty.
fn chars_of(node: &Node, name: &str, default: &str) -> String {
    match node.text_of(name) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => default.to_string(),
    }
}

fn check_len(what: &str, s: &str, max: usize) -> Result<()> {
    let n = s.chars().count();
    if n == 0 || n > max {
        return Err(invalid_param(format_args!("{what} must be {} character{}", if max == 1 { "one" } else { "one or two" }, if max == 1 { "" } else { "s" })));
    }
    Ok(())
}

fn check_ascii_char(what: &str, s: &str) -> Result<()> {
    if s.len() != 1 || !s.is_ascii() {
        return Err(invalid_param(format_args!("{what} must be a single ASCII character")));
    }
    Ok(())
}

fn csv_input(n: &Node) -> Result<CsvInput> {
    let d = CsvInput::default();
    let file_header_info = match n.text_of("FileHeaderInfo").map(|s| s.trim().to_ascii_uppercase()) {
        None => FileHeaderInfo::None,
        Some(s) => match s.as_str() {
            "" | "NONE" => FileHeaderInfo::None,
            "USE" => FileHeaderInfo::Use,
            "IGNORE" => FileHeaderInfo::Ignore,
            _ => {
                return Err(SelectError::new("InvalidFileHeaderInfo", "The FileHeaderInfo is invalid. Only NONE, USE, and IGNORE are supported."));
            }
        },
    };
    let c = CsvInput {
        file_header_info,
        field_delimiter: chars_of(n, "FieldDelimiter", &d.field_delimiter),
        record_delimiter: chars_of(n, "RecordDelimiter", &d.record_delimiter),
        quote_character: chars_of(n, "QuoteCharacter", &d.quote_character),
        quote_escape_character: chars_of(n, "QuoteEscapeCharacter", &d.quote_escape_character),
        comments: match n.text_of("Comments") {
            None => d.comments,
            Some("") => None,
            Some(s) => Some(s.to_string()),
        },
        allow_quoted_record_delimiter: n.text_of("AllowQuotedRecordDelimiter").map(bool_of).transpose()?.unwrap_or(false),
    };
    validate_csv_input(&c)?;
    Ok(c)
}

pub(crate) fn validate_csv_input(c: &CsvInput) -> Result<()> {
    check_len("FieldDelimiter", &c.field_delimiter, 2)?;
    check_len("RecordDelimiter", &c.record_delimiter, 2)?;
    check_ascii_char("QuoteCharacter", &c.quote_character)?;
    check_ascii_char("QuoteEscapeCharacter", &c.quote_escape_character)?;
    if let Some(cm) = &c.comments {
        check_ascii_char("Comments", cm)?;
    }
    let (fd, rd) = (&c.field_delimiter, &c.record_delimiter);
    if fd.contains(rd.as_str()) || rd.contains(fd.as_str()) || fd.contains(&c.quote_character) || rd.contains(&c.quote_character) {
        return Err(invalid_param("the field delimiter, record delimiter and quote character must differ"));
    }
    Ok(())
}

pub(crate) fn validate_output(o: &Output) -> Result<()> {
    match o {
        Output::Csv(c) => {
            check_len("FieldDelimiter", &c.field_delimiter, 2)?;
            check_len("RecordDelimiter", &c.record_delimiter, 2)?;
            check_len("QuoteCharacter", &c.quote_character, 1)?;
            check_len("QuoteEscapeCharacter", &c.quote_escape_character, 1)?;
        }
        Output::Json(j) => check_len("RecordDelimiter", &j.record_delimiter, 2)?,
    }
    Ok(())
}

/// ScanRange works on uncompressed CSV (without quoted record delimiters) and JSON LINES.
pub(crate) fn validate_scan_range(input: &Input, compression: Compression, sr: &ScanRange) -> Result<()> {
    if let (Some(s), Some(e)) = (sr.start, sr.end)
        && s > e
    {
        return Err(SelectError::new("InvalidScanRange", "The provided scan range is not valid: Start is after End."));
    }
    let ok = compression == Compression::None
        && match input {
            Input::Csv(c) => !c.allow_quoted_record_delimiter,
            Input::Json(j) => j.kind == JsonType::Lines,
            Input::Parquet => true,
        };
    if !ok {
        return Err(SelectError::new(
            "UnsupportedScanRangeInput",
            "Scan range queries are supported only on uncompressed CSV (without quoted record delimiters) and JSON LINES objects.",
        ));
    }
    Ok(())
}

/// Parse the XML body (`<SelectObjectContentRequest>`, or `<SelectRequest>` as in AWS's examples),
/// validating it as S3 does.
pub fn parse_request(xml: &[u8]) -> Result<SelectRequest> {
    if xml.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(SelectError::new("EmptyRequestBody", "Request body cannot be empty."));
    }
    let root = parse_xml(xml)?;
    if root.name != "SelectObjectContentRequest" && root.name != "SelectRequest" {
        return Err(malformed());
    }
    let expression = root.text_of("Expression").ok_or_else(|| missing("Expression"))?.to_string();
    let et = root.text_of("ExpressionType").ok_or_else(|| missing("ExpressionType"))?;
    if !et.trim().eq_ignore_ascii_case("sql") {
        return Err(SelectError::new("InvalidExpressionType", "The ExpressionType is invalid. Only SQL expressions are supported."));
    }
    let is = root.child("InputSerialization").ok_or_else(|| missing("InputSerialization"))?;
    let os = root.child("OutputSerialization").ok_or_else(|| missing("OutputSerialization"))?;
    let compression = match is.text_of("CompressionType").map(|s| s.trim().to_ascii_uppercase()) {
        None => Compression::None,
        Some(s) => match s.as_str() {
            "" | "NONE" => Compression::None,
            "GZIP" => Compression::Gzip,
            "BZIP2" => Compression::Bzip2,
            _ => {
                return Err(SelectError::new(
                    "InvalidCompressionFormat",
                    "The file is not in a supported compression format. Only GZIP and BZIP2 are supported.",
                ));
            }
        },
    };
    let formats: Vec<&Node> = is.children.iter().filter(|c| matches!(c.name.as_str(), "CSV" | "JSON" | "Parquet")).collect();
    let input = match formats.as_slice() {
        [] => return Err(missing("InputSerialization needs CSV, JSON or Parquet")),
        [one] => match one.name.as_str() {
            "CSV" => Input::Csv(csv_input(one)?),
            "JSON" => Input::Json(JsonInput {
                kind: match one.text_of("Type").map(|s| s.trim().to_ascii_uppercase()) {
                    None => JsonType::Document,
                    Some(s) => match s.as_str() {
                        "" | "DOCUMENT" => JsonType::Document,
                        "LINES" => JsonType::Lines,
                        _ => return Err(SelectError::new("InvalidJsonType", "The JsonType is invalid. Only DOCUMENT and LINES are supported.")),
                    },
                },
            }),
            _ => Input::Parquet,
        },
        _ => return Err(conflict()),
    };
    if input == Input::Parquet && compression != Compression::None {
        return Err(SelectError::new(
            "InvalidCompressionFormat",
            "Parquet objects can't be compressed as a whole (only their columns, with GZIP or Snappy).",
        ));
    }
    let outs: Vec<&Node> = os.children.iter().filter(|c| matches!(c.name.as_str(), "CSV" | "JSON" | "Parquet")).collect();
    let output = match outs.as_slice() {
        [] => return Err(missing("OutputSerialization needs CSV or JSON")),
        [one] => match one.name.as_str() {
            "CSV" => {
                let d = CsvOutput::default();
                Output::Csv(CsvOutput {
                    quote_fields: match one.text_of("QuoteFields").map(|s| s.trim().to_ascii_uppercase()) {
                        None => QuoteFields::AsNeeded,
                        Some(s) => match s.as_str() {
                            "" | "ASNEEDED" => QuoteFields::AsNeeded,
                            "ALWAYS" => QuoteFields::Always,
                            _ => {
                                return Err(SelectError::new("InvalidQuoteFields", "The QuoteFields is invalid. Only ALWAYS and ASNEEDED are supported."));
                            }
                        },
                    },
                    field_delimiter: chars_of(one, "FieldDelimiter", &d.field_delimiter),
                    record_delimiter: chars_of(one, "RecordDelimiter", &d.record_delimiter),
                    quote_character: chars_of(one, "QuoteCharacter", &d.quote_character),
                    quote_escape_character: chars_of(one, "QuoteEscapeCharacter", &d.quote_escape_character),
                })
            }
            "JSON" => Output::Json(JsonOutput { record_delimiter: chars_of(one, "RecordDelimiter", "\n") }),
            _ => return Err(invalid_param("Parquet output is not supported; use CSV or JSON")),
        },
        _ => return Err(conflict()),
    };
    validate_output(&output)?;
    let progress = match root.child("RequestProgress") {
        Some(p) => p.text_of("Enabled").map(bool_of).transpose()?.unwrap_or(false),
        None => false,
    };
    let scan_range = match root.child("ScanRange") {
        None => None,
        Some(sr) => {
            let num = |name: &str| -> Result<Option<u64>> {
                match sr.text_of(name).map(str::trim) {
                    None => Ok(None),
                    Some(s) => s.parse::<u64>().map(Some).map_err(|_| {
                        SelectError::new("InvalidScanRange", format!("The provided scan range is not valid: {name} must be a non-negative integer."))
                    }),
                }
            };
            let r = ScanRange { start: num("Start")?, end: num("End")? };
            if r.start.is_none() && r.end.is_none() {
                return Err(SelectError::new("InvalidScanRange", "The provided scan range is not valid: it is empty."));
            }
            validate_scan_range(&input, compression, &r)?;
            Some(r)
        }
    };
    Ok(SelectRequest { expression, input, compression, output, progress, scan_range })
}

fn conflict() -> SelectError {
    SelectError::new(
        "ObjectSerializationConflict",
        "InputSerialization specifies more than one format (CSV, JSON, or Parquet), or OutputSerialization specifies more than one format (CSV or JSON). For InputSerialization and OutputSerialization, you can specify only one format for each.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body as boto3 sends it for `select_object_content(...)`.
    const BOTO3: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SelectObjectContentRequest xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Expression>select count(0) from s3object where _1 &lt; 5 &amp;&amp; 1;</Expression><ExpressionType>SQL</ExpressionType><RequestProgress><Enabled>false</Enabled></RequestProgress><InputSerialization><CSV><FileHeaderInfo>NONE</FileHeaderInfo><QuoteEscapeCharacter>\</QuoteEscapeCharacter><RecordDelimiter>&#10;</RecordDelimiter><FieldDelimiter>,</FieldDelimiter><QuoteCharacter>"</QuoteCharacter></CSV><CompressionType>NONE</CompressionType></InputSerialization><OutputSerialization><CSV /></OutputSerialization></SelectObjectContentRequest>"#;

    #[test]
    fn boto3_body() {
        let r = parse_request(BOTO3.as_bytes()).unwrap();
        assert_eq!(r.expression, "select count(0) from s3object where _1 < 5 && 1;");
        assert_eq!(
            r.input,
            Input::Csv(CsvInput {
                file_header_info: FileHeaderInfo::None,
                field_delimiter: ",".into(),
                record_delimiter: "\n".into(),
                quote_character: "\"".into(),
                quote_escape_character: "\\".into(),
                comments: Some("#".into()),
                allow_quoted_record_delimiter: false,
            })
        );
        assert_eq!(r.compression, Compression::None);
        assert_eq!(r.output, Output::Csv(CsvOutput::default()));
        assert!(!r.progress);
        assert_eq!(r.scan_range, None);
    }

    /// AWS's documented example (pretty-printed, `<SelectRequest>` root, a literal newline).
    #[test]
    fn aws_example_body() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<SelectRequest>
    <Expression>Select * from S3Object</Expression>
    <ExpressionType>SQL</ExpressionType>
    <InputSerialization>
        <CompressionType>GZIP</CompressionType>
        <CSV>
            <FileHeaderInfo>IGNORE</FileHeaderInfo>
            <RecordDelimiter>\n</RecordDelimiter>
            <FieldDelimiter>\t</FieldDelimiter>
            <QuoteCharacter>'</QuoteCharacter>
            <QuoteEscapeCharacter>\"</QuoteEscapeCharacter>
            <Comments>#</Comments>
            <AllowQuotedRecordDelimiter>TRUE</AllowQuotedRecordDelimiter>
        </CSV>
    </InputSerialization>
    <OutputSerialization>
        <JSON><RecordDelimiter>;</RecordDelimiter></JSON>
    </OutputSerialization>
    <RequestProgress><Enabled>TRUE</Enabled></RequestProgress>
</SelectRequest>";
        let r = parse_request(xml.as_bytes()).unwrap();
        let Input::Csv(c) = &r.input else { panic!() };
        assert_eq!(c.file_header_info, FileHeaderInfo::Ignore);
        assert_eq!(c.record_delimiter, "\n");
        assert_eq!(c.field_delimiter, "\t");
        assert_eq!(c.quote_character, "'");
        assert!(c.allow_quoted_record_delimiter);
        assert_eq!(r.compression, Compression::Gzip);
        assert_eq!(r.output, Output::Json(JsonOutput { record_delimiter: ";".into() }));
        assert!(r.progress);
    }

    fn body(input: &str, output: &str, extra: &str) -> String {
        format!(
            "<SelectObjectContentRequest><Expression>select * from s3object</Expression><ExpressionType>SQL</ExpressionType><InputSerialization>{input}</InputSerialization><OutputSerialization>{output}</OutputSerialization>{extra}</SelectObjectContentRequest>"
        )
    }

    fn code(xml: &str) -> &'static str {
        parse_request(xml.as_bytes()).unwrap_err().code
    }

    #[test]
    fn json_and_scan_range() {
        let r = parse_request(body("<JSON><Type>LINES</Type></JSON>", "<JSON/>", "<ScanRange><Start>10</Start><End>20</End></ScanRange>").as_bytes()).unwrap();
        assert_eq!(r.input, Input::Json(JsonInput { kind: JsonType::Lines }));
        assert_eq!(r.output, Output::Json(JsonOutput::default()));
        assert_eq!(r.scan_range, Some(ScanRange { start: Some(10), end: Some(20) }));
        let r = parse_request(body("<JSON><Type>document</Type></JSON>", "<CSV><QuoteFields>ALWAYS</QuoteFields></CSV>", "").as_bytes()).unwrap();
        assert_eq!(r.input, Input::Json(JsonInput { kind: JsonType::Document }));
        assert!(matches!(r.output, Output::Csv(CsvOutput { quote_fields: QuoteFields::Always, .. })));
        let r = parse_request(body("<CSV><Comments></Comments></CSV>", "<CSV/>", "<ScanRange><End>5</End></ScanRange>").as_bytes()).unwrap();
        assert!(matches!(&r.input, Input::Csv(c) if c.comments.is_none()));
        assert_eq!(r.scan_range, Some(ScanRange { start: None, end: Some(5) }));
        assert_eq!(parse_request(body("<Parquet/>", "<CSV/>", "").as_bytes()).unwrap().input, Input::Parquet);
    }

    #[test]
    fn validation_errors() {
        assert_eq!(code(""), "EmptyRequestBody");
        assert_eq!(code("<Select"), "MalformedXML");
        assert_eq!(code("<Other/>"), "MalformedXML");
        assert_eq!(code("<!DOCTYPE x><SelectRequest/>"), "MalformedXML");
        assert_eq!(code("<SelectRequest><ExpressionType>SQL</ExpressionType></SelectRequest>"), "MissingRequiredParameter");
        assert_eq!(code(&body("<CSV/>", "<CSV/>", "").replace("<ExpressionType>SQL</ExpressionType>", "")), "MissingRequiredParameter");
        assert_eq!(code(&body("<CSV/>", "<CSV/>", "").replace(">SQL<", ">XPATH<")), "InvalidExpressionType");
        assert_eq!(code(&body("", "<CSV/>", "")), "MissingRequiredParameter");
        assert_eq!(code(&body("<CSV/>", "", "")), "MissingRequiredParameter");
        assert_eq!(code(&body("<CSV/><JSON/>", "<CSV/>", "")), "ObjectSerializationConflict");
        assert_eq!(code(&body("<CSV/>", "<CSV/><JSON/>", "")), "ObjectSerializationConflict");
        assert_eq!(code(&body("<CSV><FileHeaderInfo>MAYBE</FileHeaderInfo></CSV>", "<CSV/>", "")), "InvalidFileHeaderInfo");
        assert_eq!(code(&body("<JSON><Type>XML</Type></JSON>", "<CSV/>", "")), "InvalidJsonType");
        assert_eq!(code(&body("<CSV/>", "<CSV><QuoteFields>SOMETIMES</QuoteFields></CSV>", "")), "InvalidQuoteFields");
        assert_eq!(code(&body("<CSV/><CompressionType>ZSTD</CompressionType>", "<CSV/>", "")), "InvalidCompressionFormat");
        assert_eq!(code(&body("<Parquet/><CompressionType>GZIP</CompressionType>", "<CSV/>", "")), "InvalidCompressionFormat");
        assert_eq!(code(&body("<CSV/>", "<CSV/>", "<ScanRange><Start>9</Start><End>3</End></ScanRange>")), "InvalidScanRange");
        assert_eq!(code(&body("<CSV/>", "<CSV/>", "<ScanRange><Start>-1</Start></ScanRange>")), "InvalidScanRange");
        assert_eq!(code(&body("<CSV/>", "<CSV/>", "<ScanRange></ScanRange>")), "InvalidScanRange");
        assert_eq!(code(&body("<JSON/>", "<CSV/>", "<ScanRange><Start>1</Start></ScanRange>")), "UnsupportedScanRangeInput");
        assert_eq!(code(&body("<CSV/><CompressionType>GZIP</CompressionType>", "<CSV/>", "<ScanRange><Start>1</Start></ScanRange>")), "UnsupportedScanRangeInput");
        assert_eq!(
            code(&body("<CSV><AllowQuotedRecordDelimiter>true</AllowQuotedRecordDelimiter></CSV>", "<CSV/>", "<ScanRange><Start>1</Start></ScanRange>")),
            "UnsupportedScanRangeInput"
        );
        assert_eq!(code(&body("<CSV><FieldDelimiter>abc</FieldDelimiter></CSV>", "<CSV/>", "")), "InvalidRequestParameter");
        assert_eq!(code(&body("<CSV><QuoteCharacter>«</QuoteCharacter></CSV>", "<CSV/>", "")), "InvalidRequestParameter");
        assert_eq!(code(&body("<CSV><FieldDelimiter>\"</FieldDelimiter></CSV>", "<CSV/>", "")), "InvalidRequestParameter");
        assert_eq!(code(&body("<CSV/>", "<Parquet/>", "")), "InvalidRequestParameter");
        assert_eq!(code(&body("<CSV><AllowQuotedRecordDelimiter>yes</AllowQuotedRecordDelimiter></CSV>", "<CSV/>", "")), "MalformedXML");
    }
}
