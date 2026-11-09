//! End-to-end tests through the public API.

use std::io::Write as _;

use crate::*;

fn req(sql: &str, input: Input, output: Output) -> SelectRequest {
    SelectRequest { expression: sql.into(), input, compression: Compression::None, output, progress: false, scan_range: None }
}

fn csv_in() -> Input {
    Input::Csv(CsvInput::default())
}

fn csv_out() -> Output {
    Output::Csv(CsvOutput::default())
}

fn json_out() -> Output {
    Output::Json(JsonOutput::default())
}

fn doc() -> Input {
    Input::Json(JsonInput { kind: JsonType::Document })
}

fn lines() -> Input {
    Input::Json(JsonInput { kind: JsonType::Lines })
}

/// Run a request over `data` fed `chunk` bytes at a time, the way the server should: draining a
/// backlog before feeding more, stopping when done.
fn run_chunked(r: &SelectRequest, data: &[u8], chunk: usize) -> Result<String, SelectError> {
    let mut q = Query::new(r)?;
    let range = q.input_range(data.len() as u64);
    let mut out = Vec::new();
    if let Some(range) = range {
        for piece in data[range.start as usize..range.end as usize].chunks(chunk.max(1)) {
            q.feed(piece)?;
            while q.backlogged() {
                out.extend(q.take_output());
                q.feed(&[])?;
            }
            assert!(q.pending_output() <= OUTPUT_HIGH_WATER + 2 * MAX_RECORD_BYTES);
            if q.done() {
                break;
            }
        }
    }
    q.finish()?;
    out.extend(q.take_output());
    let s = q.stats();
    assert_eq!(s.bytes_returned, out.len() as u64);
    Ok(String::from_utf8(out).unwrap())
}

fn run(r: &SelectRequest, data: &[u8]) -> Result<String, SelectError> {
    run_chunked(r, data, 1 << 16)
}

fn csv(sql: &str, data: &str) -> String {
    run(&req(sql, csv_in(), csv_out()), data.as_bytes()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn csv_err(sql: &str, data: &str) -> &'static str {
    run(&req(sql, csv_in(), csv_out()), data.as_bytes()).unwrap_err().code
}

fn csv_with(sql: &str, input: CsvInput, data: &str) -> String {
    run(&req(sql, Input::Csv(input), csv_out()), data.as_bytes()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn json(sql: &str, input: Input, data: &str) -> String {
    run(&req(sql, input, json_out()), data.as_bytes()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Run on a thread with a 2 MiB stack (tokio's default for workers).
fn small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new().stack_size(2 << 20).spawn(f).unwrap().join().unwrap()
}

/// A small deterministic PRNG for generated objects.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Like s3-tests' create_random_csv_object: `rows` lines of `cols` numbers 0..=1000, each followed
/// by the delimiter.
fn random_csv(rng: &mut Rng, rows: usize, cols: usize, fd: &str, rd: &str) -> String {
    let mut s = String::new();
    for _ in 0..rows {
        for _ in 0..cols {
            s += &format!("{}{fd}", rng.below(1001));
        }
        s += rd;
    }
    s
}

#[test]
fn query_is_send() {
    fn send<T: Send + 'static>() {}
    send::<Query>();
    send::<SelectRequest>();
    send::<SelectError>();
}

#[test]
fn deepest_queries_fit_a_small_stack() {
    small_stack(|| {
        let n = parser::MAX_NESTING - 2;
        let sql = format!("select {}_1{} from s3object", "(".repeat(n), ")".repeat(n));
        assert_eq!(csv(&sql, "7\n"), "7\n");
        let sql = format!("select {}1{} from s3object", "lower(cast(".repeat(n / 3), " as string))".repeat(n / 3));
        assert_eq!(csv(&sql, "7\n"), "1\n");
        let sql = format!("select 1{} from s3object", " + 1".repeat(parser::MAX_DEPTH - 5));
        assert_eq!(csv(&sql, "7\n"), format!("{}\n", parser::MAX_DEPTH - 4));
        let sql = format!("select _1 from s3object where {}", vec!["_1 = '7'"; parser::MAX_DEPTH / 2 - 5].join(" or "));
        assert_eq!(csv(&sql, "7\n"), "7\n");
        let sql = format!("select {}1 from s3object", "- ".repeat(n - 1));
        assert!(run(&req(&sql, csv_in(), csv_out()), b"7\n").is_ok());
        let sql = format!(
            "select int(_1) as a0{} from s3object where a40 > 0",
            (1..=40).map(|i| format!(", a{} + 1 as a{i}", i - 1)).collect::<String>()
        );
        assert!(run(&req(&sql, csv_in(), csv_out()), b"7\n").is_ok());
        let sql = format!("select {}1 from s3object", "not ".repeat(n));
        assert!(run(&req(&sql, csv_in(), csv_out()), b"7\n").is_err());
    });
}

#[test]
fn csv_projection_filter_limit() {
    let data = "1,alice,30\n2,bob,25\n3,carol,35\n";
    assert_eq!(csv("select * from s3object", data), data);
    assert_eq!(csv("SELECT s._2, s._3 FROM S3Object s WHERE s._3 > 26", data), "alice,30\ncarol,35\n");
    assert_eq!(csv("select _2 from s3object where _3 > 26 limit 1", data), "alice\n");
    assert_eq!(csv("select _2 from s3object limit 0", data), "");
    assert_eq!(csv("select _2 || '!' as x, cast(_3 as int) + 1 from s3object where _2 like '%o%'", data), "bob!,26\ncarol!,36\n");
    assert_eq!(csv("select _1, _4 from s3object where _1 = 1", data), "1,\n");
    assert_eq!(csv("select count(0) from s3object;", data), "3\n");
    assert_eq!(csv("select upper(_2) from S3OBJECT where _2 in ('bob', 'carol')", data), "BOB\nCAROL\n");
    // JSON output names the columns.
    let r = req("select s._2, _3 as age, cast(_1 as int) + 1 from s3object s", csv_in(), json_out());
    assert_eq!(
        run(&r, data.as_bytes()).unwrap(),
        "{\"_2\":\"alice\",\"age\":\"30\",\"_3\":2}\n{\"_2\":\"bob\",\"age\":\"25\",\"_3\":3}\n{\"_2\":\"carol\",\"age\":\"35\",\"_3\":4}\n"
    );
    let r = req("select * from s3object limit 1", csv_in(), json_out());
    assert_eq!(run(&r, data.as_bytes()).unwrap(), "{\"_1\":\"1\",\"_2\":\"alice\",\"_3\":\"30\"}\n");
}

#[test]
fn csv_header_modes() {
    let data = "Id,Name,Age\n1,alice,30\n2,bob,25\n";
    let use_ = CsvInput { file_header_info: FileHeaderInfo::Use, ..CsvInput::default() };
    let ignore = CsvInput { file_header_info: FileHeaderInfo::Ignore, ..CsvInput::default() };
    assert_eq!(csv_with("select name, s.\"Age\" from s3object s where id = '2'", use_.clone(), data), "bob,25\n");
    assert_eq!(csv_with("select _2 from s3object", use_.clone(), data), "alice\nbob\n");
    assert_eq!(csv_with("select * from s3object", use_.clone(), data), "1,alice,30\n2,bob,25\n");
    let r = req("select * from s3object", Input::Csv(use_.clone()), json_out());
    assert_eq!(
        run(&r, data.as_bytes()).unwrap(),
        "{\"Id\":\"1\",\"Name\":\"alice\",\"Age\":\"30\"}\n{\"Id\":\"2\",\"Name\":\"bob\",\"Age\":\"25\"}\n"
    );
    let r = req("select \"name\" from s3object", Input::Csv(use_.clone()), csv_out());
    assert_eq!(run(&r, data.as_bytes()).unwrap_err().code, "MissingHeaders");
    assert_eq!(csv_with("select _2 from s3object", ignore, data), "alice\nbob\n");
    assert_eq!(csv("select _2 from s3object", data), "Name\nalice\nbob\n");
    // A header-only or empty object: aggregates still answer.
    assert_eq!(csv_with("select count(*) from s3object", use_.clone(), "a,b\n"), "0\n");
    let r = req("select count(*), max(a) from s3object", Input::Csv(use_.clone()), csv_out());
    assert_eq!(run(&r, b"").unwrap_err().code, "MissingHeaders");
    assert_eq!(csv_with("select a from s3object", use_.clone(), ""), "");
    // Comments come before the header too.
    assert_eq!(csv_with("select b from s3object", use_, "# note\na,b\n1,2\n#3,4\n5,6\n"), "2\n6\n");
}

#[test]
fn csv_dialects() {
    // Quotes, doubled quotes, escapes, delimiters in quotes, \r\n records, comments.
    let data = "\"a,1\",\"say \"\"hi\"\"\",\"x\\\"y\"\r\n# comment\r\nplain,\"\",end\r\n";
    let input = CsvInput { quote_escape_character: "\\".into(), ..CsvInput::default() };
    let r = req("select * from s3object", Input::Csv(input), json_out());
    assert_eq!(
        run(&r, data.as_bytes()).unwrap(),
        "{\"_1\":\"a,1\",\"_2\":\"say \\\"hi\\\"\",\"_3\":\"x\\\"y\"}\n{\"_1\":\"plain\",\"_2\":\"\",\"_3\":\"end\"}\n"
    );
    // Custom delimiters (s3-tests' "|" and "\t"), a 2-character record delimiter, other quotes.
    let input = CsvInput { field_delimiter: "|".into(), record_delimiter: "\t".into(), ..CsvInput::default() };
    assert_eq!(csv_with("select _2, _1 from s3object", input, "1|2|\t3|4|\t"), "2,1\n4,3\n");
    let input = CsvInput {
        field_delimiter: ";".into(),
        record_delimiter: "\r\n".into(),
        quote_character: "'".into(),
        quote_escape_character: "'".into(),
        ..CsvInput::default()
    };
    assert_eq!(csv_with("select _2 from s3object", input, "1;'a;b'\r\n2;'it''s'\r\n"), "a;b\nit's\n");
    // Comments off: '#' is data. Another comment character.
    let input = CsvInput { comments: None, ..CsvInput::default() };
    assert_eq!(csv_with("select _1 from s3object", input, "#a\nb\n"), "#a\nb\n");
    let input = CsvInput { comments: Some("%".into()), ..CsvInput::default() };
    assert_eq!(csv_with("select _1 from s3object", input, "#a\n%b\nc\n"), "#a\nc\n");
    // Quoted record delimiters: kept inside quotes only when allowed.
    let data = "1,\"two\nlines\"\n2,x\n";
    let allow = CsvInput { allow_quoted_record_delimiter: true, ..CsvInput::default() };
    let r = req("select _2 from s3object", Input::Csv(allow), json_out());
    assert_eq!(run(&r, data.as_bytes()).unwrap(), "{\"_2\":\"two\\nlines\"}\n{\"_2\":\"x\"}\n");
    assert_eq!(csv("select _1 from s3object", data), "1\n\"lines\"\"\"\n2\n");
    // No trailing newline, blank lines, a BOM.
    assert_eq!(csv("select _1 from s3object", "\u{feff}a\n\nb"), "a\nb\n");
    // s3-tests' test_csv_parser line (S3 Select reads quotes only at the start of a field).
    let line = r#",first,,,second,third="c31,c32,c33",forth="1,2,3,4",fifth=my_string=\"any_value\" \, my_other_string=\"aaaa\,bbb\" ,"#
        .to_string()
        + "\n";
    let input = CsvInput { quote_escape_character: "\\".into(), ..CsvInput::default() };
    assert_eq!(csv_with("select _2, _6 from s3object", input.clone(), &line), "first,\"third=\"\"c31\"\n");
    assert_eq!(csv_with("select _1, _3, _4 from s3object", input, &line), ",,\n");
    // Invalid UTF-8.
    assert_eq!(run(&req("select _1 from s3object", csv_in(), csv_out()), b"\xff\xfe\n").unwrap_err().code, "InvalidTextEncoding");
}

#[test]
fn csv_output_options() {
    let data = "a,\"b,c\",\"d\"\"e\"\n";
    let always = CsvOutput { quote_fields: QuoteFields::Always, ..CsvOutput::default() };
    assert_eq!(run(&req("select * from s3object", csv_in(), Output::Csv(always)), data.as_bytes()).unwrap(), "\"a\",\"b,c\",\"d\"\"e\"\n");
    let custom = CsvOutput {
        quote_fields: QuoteFields::AsNeeded,
        field_delimiter: "|".into(),
        record_delimiter: "\r\n".into(),
        quote_character: "'".into(),
        quote_escape_character: "\\".into(),
    };
    assert_eq!(run(&req("select _1, _2, 'it''s' from s3object", csv_in(), Output::Csv(custom)), data.as_bytes()).unwrap(), "a|b,c|'it\\'s'\r\n");
    let jr = Output::Json(JsonOutput { record_delimiter: ";".into() });
    assert_eq!(run(&req("select _1 as x from s3object", csv_in(), jr), b"1\n2\n").unwrap(), "{\"x\":\"1\"};{\"x\":\"2\"};");
}

#[test]
fn aggregates() {
    let data = "1,2.5,x\n2,,y\n3,4,\n";
    assert_eq!(
        csv("select count(*), count(_2), sum(cast(_1 as int)), avg(cast(_1 as int)), min(_1), max(_1) from s3object", data),
        "3,3,6,2.0,1,3\n"
    );
    assert_eq!(csv("select sum(cast(_2 as float)), max(cast(_2 as float)), min(_3), max(_3) from s3object", data), "6.5,4.0,,y\n");
    assert_eq!(csv("select count(*), sum(_1) from s3object where _1 > 5", data), "0,\n");
    assert_eq!(csv("select sum(_1) + 1, count(*) * 2 from s3object", data), "7,6\n");
    assert_eq!(csv("select max(int(_1)) as m from s3object", data), "3\n");
    assert_eq!(csv_err("select _1, count(*) from s3object", data), "UnsupportedSqlStructure");
    assert_eq!(csv_err("select sum(_3) from s3object", data), "InvalidDataType");
    assert_eq!(csv_err("select max(_1) from s3object", "1\nabc\n"), "InvalidDataType");
    assert_eq!(csv_err("select sum(cast(_1 as int)) from s3object", "9223372036854775807\n1\n"), "IntegerOverflow");
    let r = req("select count(*) as n, avg(s.a) from s3object[*] s", lines(), json_out());
    assert_eq!(run(&r, b"{\"a\":1}\n{\"a\":2}\n{\"b\":3}\n").unwrap(), "{\"n\":3,\"_2\":1.5}\n");
    // An aggregate query returns one record, whatever LIMIT says.
    assert_eq!(csv("select count(*) from s3object limit 1", data), "3\n");
}

#[test]
fn json_document_and_lines() {
    let docs = r#"{ "Rules": [ {"id": "1"}, {"expr": "y > x"}, {"id": "2", "expr": "z = DEBUG"} ]}
{ "created": "June 27", "modified": "July 6" }"#;
    assert_eq!(json("SELECT id FROM S3Object[*].Rules[*].id", doc(), docs), "{\"id\":\"1\"}\n{}\n{\"id\":\"2\"}\n{}\n");
    assert_eq!(json("SELECT id FROM S3Object[*].Rules[*].id WHERE id IS NOT MISSING", doc(), docs), "{\"id\":\"1\"}\n{\"id\":\"2\"}\n");
    let dirs = r#"{ "created": "936864000", "dir_name": "important_docs", "files": [ { "name": "." }, { "name": ".." }, { "name": ".aws" }, { "name": "downloads" } ], "owner": "Amazon S3" }
{ "created": "936864000", "dir_name": "other_docs", "files": [ { "name": "." }, { "name": ".." }, { "name": "my stuff" }, { "name": "backup" } ], "owner": "User" }"#;
    assert_eq!(
        json("SELECT d.dir_name, d.files FROM S3Object[*] d", doc(), dirs),
        "{\"dir_name\":\"important_docs\",\"files\":[{\"name\":\".\"},{\"name\":\"..\"},{\"name\":\".aws\"},{\"name\":\"downloads\"}]}\n{\"dir_name\":\"other_docs\",\"files\":[{\"name\":\".\"},{\"name\":\"..\"},{\"name\":\"my stuff\"},{\"name\":\"backup\"}]}\n"
    );
    assert_eq!(
        json("SELECT _1.dir_name, _1.owner FROM S3Object[*]", doc(), dirs),
        "{\"dir_name\":\"important_docs\",\"owner\":\"Amazon S3\"}\n{\"dir_name\":\"other_docs\",\"owner\":\"User\"}\n"
    );
    assert_eq!(
        json("SELECT f.name FROM S3Object[*].files[*] f WHERE f.name LIKE '.%'", doc(), dirs),
        "{\"name\":\".\"}\n{\"name\":\"..\"}\n{\"name\":\".aws\"}\n{\"name\":\".\"}\n{\"name\":\"..\"}\n"
    );
    let susan = r#"{"name": "Susan Smith", "org": "engineering", "projects": [ {"project_name":"project1", "completed":false}, {"project_name":"project2", "completed":true} ] }"#;
    assert_eq!(json("Select s.name from S3Object s", doc(), susan), "{\"name\":\"Susan Smith\"}\n");
    assert_eq!(json("Select s.projects[0].project_name from S3Object s", doc(), susan), "{\"project_name\":\"project1\"}\n");
    assert_eq!(json("Select s.NAME, s.\"name\", s.\"NAME\" from S3Object s", doc(), susan), "{\"NAME\":\"Susan Smith\",\"name\":\"Susan Smith\"}\n");
    assert_eq!(json("select count(*) from s3object[*].projects[*] p where p.completed = true", doc(), susan), "{\"_1\":1}\n");
    // SELECT * on JSON keeps document order; CSV output flattens the top level.
    let r = req("select * from s3object", lines(), csv_out());
    assert_eq!(run(&r, b"{\"b\":1,\"a\":[1,2],\"c\":null}\n\n{\"b\":\"x,y\"}\r\n").unwrap(), "1,\"[1,2]\",\n\"x,y\"\n");
    assert_eq!(json("select * from s3object[*].a[*]", lines(), "{\"a\":[1,{\"k\":2}]}\n"), "{\"_1\":1}\n{\"k\":2}\n");
    assert_eq!(json("select s.a from s3object s", lines(), "{\"a\":1.5e3, \"b\":\"\\u00e9\"}\n[1]\n"), "{\"a\":1500.0}\n{}\n");
    // s3-tests' documents with s3object[*].root.
    let root = "{\"root\" : [\n{\"c1\": 5,\"c2\": 7},\n{\"c1\": 2,\"c2\": 2}\n]}";
    assert_eq!(json("select count(0) from s3object[*];", doc(), root), "{\"_1\":1}\n");
    assert_eq!(json("select count(0) from s3object[*].root;", doc(), root), "{\"_1\":1}\n");
    assert_eq!(json("select count(0) from s3object[*].root[*];", doc(), root), "{\"_1\":2}\n");
    assert_eq!(json("select sum(_1.c1), min(_1.c2) from s3object[*].root[*];", doc(), root), "{\"_1\":7,\"_2\":2}\n");
    assert_eq!(json("select count(0) from s3object[*].root[*] where nullif(_1.c1,_1.c2) is null;", doc(), root), "{\"_1\":1}\n");
    assert_eq!(json("select (_1.c1 = _1.c2) from s3object[*].root;", doc(), root), "{}\n");
    // Errors.
    assert_eq!(run(&req("select * from s3object", lines(), json_out()), b"{\"a\":\n").unwrap_err().code, "JSONParsingError");
    assert_eq!(run(&req("select * from s3object", doc(), json_out()), b"{\"a\":1").unwrap_err().code, "JSONParsingError");
    assert_eq!(run(&req("select * from s3object[*].a", csv_in(), json_out()), b"").unwrap_err().code, "InvalidKeyPath");
}

#[test]
fn feeding_in_any_chunk_size_gives_the_same_output() {
    let mut rng = Rng(7);
    let data = random_csv(&mut rng, 300, 5, ",", "\r\n");
    let jdoc = format!("{{\"root\": [{}]}}", (0..300).map(|i| format!("{{\"c1\": {i}, \"s\": \"v{i}\\\"\"}}")).collect::<Vec<_>>().join(", "));
    let jlines: String = (0..300).map(|i| format!("{{\"c1\": {i}, \"s\": \"x{}\"}}\n", i % 7)).collect();
    let cases = [
        (req("select _2, _1 from s3object where cast(_3 as int) > 500", csv_in(), json_out()), data.as_bytes()),
        (req("select count(*), sum(cast(_1 as int)) from s3object", csv_in(), csv_out()), data.as_bytes()),
        (req("select r.s from s3object[*].root[*] r where r.c1 % 3 = 0", doc(), csv_out()), jdoc.as_bytes()),
        (req("select * from s3object s where s.s = 'x3'", lines(), json_out()), jlines.as_bytes()),
    ];
    for (r, input) in &cases {
        let want = run(r, input).unwrap();
        assert!(!want.is_empty());
        for chunk in [1, 2, 3, 5, 17, 100, 4096] {
            assert_eq!(run_chunked(r, input, chunk).unwrap(), want, "{} chunk {chunk}", r.expression);
        }
    }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn bzip2(data: &[u8]) -> Vec<u8> {
    let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

#[test]
fn compressed_input() {
    let mut rng = Rng(3);
    let data = random_csv(&mut rng, 2000, 4, ",", "\n");
    let mut plain = req("select _1, _3 from s3object where _2 > 900", csv_in(), csv_out());
    let want = run(&plain, data.as_bytes()).unwrap();
    for (c, bytes) in [(Compression::Gzip, gzip(data.as_bytes())), (Compression::Bzip2, bzip2(data.as_bytes()))] {
        plain.compression = c;
        for chunk in [1, 7, 1000, bytes.len()] {
            assert_eq!(run_chunked(&plain, &bytes, chunk).unwrap(), want, "{c:?} chunk {chunk}");
        }
        let mut q = Query::new(&plain).unwrap();
        q.feed(&bytes).unwrap();
        q.finish().unwrap();
        let s = q.stats();
        assert_eq!(s.bytes_scanned, bytes.len() as u64);
        assert_eq!(s.bytes_processed, data.len() as u64);
        // Truncated input fails.
        let e = run(&plain, &bytes[..bytes.len() / 2]).unwrap_err();
        assert_eq!(e.code, "TruncatedInput");
    }
    // Concatenated gzip members (as `cat a.gz b.gz` makes).
    let mut two = gzip(b"1\n2\n");
    two.extend(gzip(b"3\n"));
    plain.compression = Compression::Gzip;
    plain.expression = "select count(*) from s3object".into();
    assert_eq!(run_chunked(&plain, &two, 1).unwrap(), "3\n");
    // JSON too.
    let r = SelectRequest { compression: Compression::Bzip2, ..req("select s.a from s3object s", doc(), csv_out()) };
    assert_eq!(run(&r, &bzip2(b"{\"a\":1} {\"a\":2}")).unwrap(), "1\n2\n");
}

/// A decompression bomb fed in one call stays bounded: decompressed data is processed 64 KiB at
/// a time, and output pauses at the high-water mark.
#[test]
fn decompression_bomb_stays_bounded() {
    let line = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n";
    let total = 16 << 20;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let block = line.repeat(10_000);
    for _ in 0..total / block.len() {
        e.write_all(&block).unwrap();
    }
    let bomb = e.finish().unwrap();
    assert!(bomb.len() < total / 100);
    let rows = (total / block.len() * 10_000) as u64;

    // An aggregate: no output to speak of.
    let r = SelectRequest { compression: Compression::Gzip, ..req("select count(*) from s3object", csv_in(), csv_out()) };
    let mut q = Query::new(&r).unwrap();
    q.feed(&bomb).unwrap();
    assert_eq!(q.pending_output(), 0);
    assert!(!q.backlogged());
    q.finish().unwrap();
    assert_eq!(String::from_utf8(q.take_output()).unwrap(), format!("{rows}\n"));

    // SELECT *: one feed produces at most the high-water mark; the rest waits.
    let r = SelectRequest { compression: Compression::Gzip, ..req("select * from s3object", csv_in(), csv_out()) };
    let mut q = Query::new(&r).unwrap();
    q.feed(&bomb).unwrap();
    assert!(q.pending_output() <= OUTPUT_HIGH_WATER + line.len());
    assert!(q.backlogged());
    let mut out = 0usize;
    let mut rounds = 0;
    while q.backlogged() {
        out += q.take_output().len();
        q.feed(&[]).unwrap();
        assert!(q.pending_output() <= OUTPUT_HIGH_WATER + line.len());
        rounds += 1;
    }
    q.finish().unwrap();
    out += q.take_output().len();
    assert_eq!(out as u64, rows * line.len() as u64);
    assert!(rounds + 1 >= out / OUTPUT_HIGH_WATER);

    // BZIP2 too.
    let small = line.repeat((4 << 20) / line.len());
    let r = SelectRequest { compression: Compression::Bzip2, ..req("select * from s3object", csv_in(), csv_out()) };
    let mut q = Query::new(&r).unwrap();
    q.feed(&bzip2(&small)).unwrap();
    assert!(q.pending_output() <= OUTPUT_HIGH_WATER + line.len());
    let mut out = 0;
    while q.backlogged() {
        out += q.take_output().len();
        q.feed(&[]).unwrap();
    }
    q.finish().unwrap();
    out += q.take_output().len();
    assert_eq!(out, small.len());
}

/// Output amplification without compression (`||`) is bounded the same way.
#[test]
fn output_amplification_pauses() {
    let data = "x\n".repeat(20_000);
    let sql = format!("select {} from s3object", vec!["_1"; 200].join(" || "));
    let r = req(&sql, csv_in(), csv_out());
    let mut q = Query::new(&r).unwrap();
    q.feed(data.as_bytes()).unwrap();
    assert!(q.pending_output() <= OUTPUT_HIGH_WATER + 300);
    assert!(q.backlogged());
    // Feeding more without draining still works (the input is held, not dropped).
    q.feed(b"y\n").unwrap();
    let mut out = q.take_output();
    q.finish().unwrap();
    out.extend(q.take_output());
    assert_eq!(out.len(), 20_001 * 201);
    assert!(out.ends_with(format!("{}\n", "y".repeat(200)).as_bytes()));
}

#[test]
fn records_over_1mib_fail() {
    let big = "x".repeat(MAX_RECORD_BYTES + 1);
    assert_eq!(csv_err("select _1 from s3object", &format!("{big}\n")), "OverMaxRecordSize");
    assert_eq!(csv_err("select count(*) from s3object", &big), "OverMaxRecordSize");
    let half = "x".repeat(MAX_RECORD_BYTES / 2 + 10);
    assert_eq!(csv_err("select _1 || _1 from s3object", &format!("{half}\n")), "OverMaxRecordSize");
    assert_eq!(csv_err("select _1, _1 from s3object", &format!("{half}\n")), "OverMaxRecordSize");
    let ok = "x".repeat(MAX_RECORD_BYTES - 10);
    assert_eq!(csv("select char_length(_1) from s3object", &format!("{ok}\n")), format!("{}\n", ok.len()));
    let r = req("select * from s3object", lines(), json_out());
    assert_eq!(run(&r, format!("{{\"a\":\"{big}\"}}\n").as_bytes()).unwrap_err().code, "OverMaxRecordSize");
}

#[test]
fn errors_end_the_query() {
    let r = req("select cast(_1 as int) from s3object", csv_in(), csv_out());
    let mut q = Query::new(&r).unwrap();
    q.feed(b"1\n2\n").unwrap();
    assert_eq!(q.take_output(), b"1\n2\n");
    let e = q.feed(b"x\n3\n").unwrap_err();
    assert_eq!(e.code, "CastFailed");
    assert_eq!(q.feed(b"4\n").unwrap_err().code, "CastFailed");
    assert_eq!(q.finish().unwrap_err().code, "CastFailed");
    // Parse and bind errors come from Query::new.
    assert_eq!(Query::new(&req("select from s3object", csv_in(), csv_out())).err().unwrap().code, "ParseEmptySelect");
    assert_eq!(Query::new(&req("select nope from s3object", csv_in(), csv_out())).err().unwrap().code, "EvaluatorBindingDoesNotExist");
    let pq = Query::new(&req("select * from s3object", Input::Parquet, csv_out())).err().unwrap();
    assert_eq!((pq.code, pq.status), ("NotImplemented", 501));
    assert!(pq.message.contains("Parquet"));
}

#[test]
fn limit_marks_done() {
    let r = req("select _1 from s3object limit 2", csv_in(), csv_out());
    let mut q = Query::new(&r).unwrap();
    q.feed(b"1\n").unwrap();
    assert!(!q.done());
    q.feed(b"2\n3").unwrap();
    assert!(q.done());
    q.feed(b"\n4\n").unwrap();
    q.finish().unwrap();
    assert_eq!(q.take_output(), b"1\n2\n");
    assert!(Query::new(&req("select _1 from s3object limit 0", csv_in(), csv_out())).unwrap().done());
    assert_eq!(csv("select _1 from s3object limit 3", "1\n2\n3\n4\n"), "1\n2\n3\n");
}

fn scan(sql: &str, input: Input, data: &str, start: Option<u64>, end: Option<u64>) -> String {
    let r = SelectRequest { scan_range: Some(ScanRange { start, end }), ..req(sql, input, csv_out()) };
    run_chunked(&r, data.as_bytes(), 3).unwrap()
}

#[test]
fn scan_ranges() {
    // AWS's example: start 1, end 4 over "A,B\nC,D\n…" returns C,D.
    let data = "A,B\nC,D\nD,E\nE,F\nG,H\nI,J\n";
    let all = |s, e| scan("select * from s3object", csv_in(), data, s, e);
    assert_eq!(all(Some(1), Some(4)), "C,D\n");
    assert_eq!(all(Some(0), Some(0)), "A,B\n");
    assert_eq!(all(Some(0), Some(3)), "A,B\n");
    assert_eq!(all(Some(0), Some(4)), "A,B\nC,D\n");
    assert_eq!(all(Some(4), Some(4)), "C,D\n");
    assert_eq!(all(Some(5), Some(8)), "D,E\n");
    assert_eq!(all(Some(5), None), "D,E\nE,F\nG,H\nI,J\n");
    assert_eq!(all(None, Some(8)), "G,H\nI,J\n");
    assert_eq!(all(Some(23), Some(100)), "");
    assert_eq!(all(Some(24), Some(100)), "");
    assert_eq!(all(None, Some(0)), "");
    // Splitting at every offset processes every record exactly once.
    for cut in 0..data.len() as u64 {
        let a = all(Some(0), Some(cut));
        let b = all(Some(cut + 1), None);
        assert_eq!(a + &b, data, "cut {cut}");
    }
    // input_range: one delimiter before start; to the end of the object.
    let r = SelectRequest { scan_range: Some(ScanRange { start: Some(5), end: Some(8) }), ..req("select * from s3object", csv_in(), csv_out()) };
    let q = Query::new(&r).unwrap();
    assert_eq!(q.input_range(data.len() as u64), Some(4..24));
    assert_eq!(q.input_range(0), None);
    assert_eq!(q.input_range(3), None);
    // A two-byte delimiter.
    let crlf = CsvInput { record_delimiter: "\r\n".into(), ..CsvInput::default() };
    let d2 = "a\r\nb\r\nc\r\n";
    for cut in 0..d2.len() as u64 {
        let a = scan("select * from s3object", Input::Csv(crlf.clone()), d2, Some(0), Some(cut));
        let b = scan("select * from s3object", Input::Csv(crlf.clone()), d2, Some(cut + 1), None);
        assert_eq!(a + &b, "a\nb\nc\n", "cut {cut}");
    }
    // With a header: USE reads it from the start of the object; IGNORE skips it only at 0.
    let h = "x,y\n1,2\n3,4\n";
    let use_ = CsvInput { file_header_info: FileHeaderInfo::Use, ..CsvInput::default() };
    assert_eq!(scan("select y from s3object", Input::Csv(use_), h, Some(5), None), "4\n");
    let ignore = CsvInput { file_header_info: FileHeaderInfo::Ignore, ..CsvInput::default() };
    assert_eq!(scan("select * from s3object", Input::Csv(ignore.clone()), h, Some(0), Some(5)), "1,2\n");
    assert_eq!(scan("select * from s3object", Input::Csv(ignore), h, Some(5), None), "3,4\n");
    // JSON LINES.
    let jl = "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n";
    assert_eq!(scan("select s.a from s3object s", lines(), jl, Some(1), Some(8)), "2\n");
    assert_eq!(scan("select count(*) from s3object s", lines(), jl, Some(8), None), "2\n");
    // The query is done once past the range.
    let r = SelectRequest { scan_range: Some(ScanRange { start: Some(0), end: Some(1) }), ..req("select * from s3object", csv_in(), csv_out()) };
    let mut q = Query::new(&r).unwrap();
    q.input_range(data.len() as u64);
    q.feed(b"A,B\nC").unwrap();
    assert!(q.done());
}

#[test]
fn parse_request_then_query() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<SelectObjectContentRequest xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Expression>SELECT s.name FROM S3Object s WHERE s.age &gt; 30</Expression><ExpressionType>SQL</ExpressionType><InputSerialization><CSV><FileHeaderInfo>USE</FileHeaderInfo></CSV><CompressionType>NONE</CompressionType></InputSerialization><OutputSerialization><JSON><RecordDelimiter>&#10;</RecordDelimiter></JSON></OutputSerialization><RequestProgress><Enabled>true</Enabled></RequestProgress></SelectObjectContentRequest>"#;
    let r = parse_request(xml.as_bytes()).unwrap();
    assert!(r.progress);
    assert_eq!(run(&r, b"name,age\nann,31\nbo,30\n").unwrap(), "{\"name\":\"ann\"}\n");
}

/// The SQL in ceph/s3-tests' test_s3select.py that AWS's dialect also has (and the cheap Ceph
/// extensions), run on objects like the ones the tests make.
#[test]
fn s3_tests_queries() {
    let mut rng = Rng(42);
    let data = random_csv(&mut rng, 2000, 10, ",", "\n");
    let q = |sql: &str| csv(sql, &data);
    let count = |sql: &str| q(sql).trim().parse::<i64>().unwrap_or_else(|_| panic!("{sql}: {}", q(sql)));

    assert_eq!(count("select count(0) from s3object;"), 2000);
    assert_eq!(q("select count(0) from stdin where nullif(_1,_2) is null ;"), q("select count(0) from stdin where _1 = _2  ;"));
    assert_eq!(q("select count(0) from stdin where not nullif(_1,_2) is null ;"), q("select count(0) from stdin where _1 != _2  ;"));
    assert_eq!(q("select count(0) from stdin where  nullif(_1,_2) = _1 ;"), q("select count(0) from stdin where _1 != _2  ;"));
    assert_eq!(q("select (nullif(_1,_2) is null) from s3object ;"), q("select (_1 = _2) from s3object  ;"));
    assert_eq!(q("select count(0) from s3object where (nullif(_1,_2) is null) = true ;"), q("select count(0) from s3object where _1 = _2  ;"));
    assert_eq!(q("select count(0) from s3object where not (nullif(_1,_2) is null) = true ;"), q("select count(0) from s3object where _1 != _2  ;"));
    assert_eq!(q("select count(*) from s3object where (nullif(_1,_1) and _1 = _2) is not null ;"), q("select count(*) from s3object where _1 != _2  ;"));
    assert_eq!(q("select lower(\"AB12cd$$\") from stdin ;").lines().next(), Some("ab12cd$$"));
    assert_eq!(q("select upper(\"ab12CD$$\") from stdin ;").lines().next(), Some("AB12CD$$"));
    assert_eq!(q("select int(_1) from stdin where int(_1) in(1,0);"), q("select int(_1) from stdin where int(_1) = 1 or int(_1) = 0;"));
    assert_eq!(
        q("select int(_2) from stdin where int(_2)*2 in(int(_3)*2,int(_4)*3,int(_5)*5);"),
        q("select int(_2) from stdin where int(_2)*2 = int(_3)*2 or int(_2)*2 = int(_4)*3 or int(_2)*2 = int(_5)*5;")
    );
    assert_eq!(
        q("select int(_1) from stdin where character_length(_1) = 2 and substring(_1,2,1) in (\"3\");"),
        q("select int(_1) from stdin where _1 like '_3';")
    );
    // A double-quoted `_3` is column 3 in S3 Select (Ceph reads it as a string).
    assert_eq!(q("select int(_1) from stdin where _1 like \"_3\";"), q("select int(_1) from stdin where _1 = _3;"));
    assert_eq!(q("select (int(_2) in(1,0,2)) from s3object;"), q("select (int(_2) = 1 or int(_2) = 0 or int(_2) = 2) from s3object;"));
    let col = |i: usize| data.lines().map(move |l| l.split(',').nth(i).unwrap().parse::<i64>().unwrap());
    let (mn, mx, m3) = (col(0).min().unwrap(), col(1).max().unwrap(), col(2).min().unwrap() + 1);
    assert_eq!(q("select min(int(_1)),max(int(_2)),min(int(_3))+1 from s3object;"), format!("{mn},{mx},{m3}\n"));
    assert_eq!(
        q("select min(int(_2)),max(int(_2)) from s3object where substring(_2,1,1) = \"1\" and char_length(_2) = 3;"),
        q("select min(int(_2)),max(int(_2)) from s3object where int(_2)>=100 and int(_2)<200;")
    );
    assert_eq!(
        q("select min(int(_2)),max(int(_2)) from s3object where int(_2)>=100 and int(_2)<200;"),
        q("select min(int(_2)),max(int(_2)) from s3object where int(_2)/100 = 1 and character_length(_2) = 3;")
    );
    let sums = q("select count(0),sum(int(_1)),sum(int(_2)) from s3object where (int(_1)-int(_2)) = 2;");
    let v: Vec<i64> = sums.trim().split(',').map(|x| x.parse().unwrap_or(0)).collect();
    assert_eq!(v[0] * 2, v[1] - v[2]);
    assert_eq!(
        q("select int(_1) as a1, int(_2) as a2 , (a1+a2) as a3 from s3object where a3>100 and a3<300;"),
        q("select int(_1),int(_2),int(_1)+int(_2) from s3object where (int(_1)+int(_2))>100 and (int(_1)+int(_2))<300;")
    );
    let cyclic = "select int(_1) as a1,int(_2) as a2, a1+a4 as a3, a5+a1 as a4, int(_3)+a3 as a5 from s3object;";
    let e = run(&req(cyclic, csv_in(), csv_out()), data.as_bytes()).unwrap_err();
    assert!(e.message.contains("number of calls exceed maximum size, probably a cyclic reference to alias"));
    assert_eq!(
        q("select (int(_1) in (1,2,0)) as a1 from s3object where a1 = true;"),
        q("select \"true\"from s3object where (int(_1) in (1,0,2)) ;")
    );
    let cases = q("select case when cast(_1 as int)>100 and cast(_1 as int)<200 then \"(100-200)\" when cast(_1 as int)>200 and cast(_1 as int)<300 then \"(200-300)\" else \"NONE\" end from s3object;");
    assert_eq!(
        cases.matches("(100-200)").count() as i64,
        count("select count(*) from s3object where  cast(_1 as int)>100 and cast(_1 as int)<200  ;")
    );
    assert_eq!(
        cases.matches("NONE").count() as i64,
        count("select count(*) from s3object where  cast(_1 as int)<=100 or cast(_1 as int)>=300 or cast(_1 as int)=200  ;")
    );
    assert_eq!(
        q("select count(*) from s3object where char_length(_3)>2 and char_length(_4)>2 and cast(substring(_3,1,2) as int) = cast(substring(_4,1,2) as int);"),
        q("select count(*) from s3object where cast(_3 as int)>99 and cast(_4 as int)>99 and coalesce(nullif(cast(substring(_3,1,2) as int),cast(substring(_4,1,2) as int)),7) = 7;")
    );
    assert_eq!(q("select coalesce(nullif(_5,_5),nullif(_1,_1),_2) from s3object;"), q("select coalesce(_2) from s3object;"));
    assert_eq!(q("select count(*) from s3object where cast(_3 as int)>999;"), q("select count(*) from s3object where char_length(_3)>3;"));
    assert_eq!(
        q("select count(*) from s3object where cast(_3 as int)>99 and cast(_3 as int)<1000;"),
        q("select count(*) from s3object where char_length(_3)=3;")
    );
    assert_eq!(
        q("select case cast(_1 as int) when cast(_2 as int) then \"case_1_1\" else \"case_2_2\" end from s3object;"),
        q("select case when cast(_1 as int) = cast(_2 as int) then \"case_1_1\" else \"case_2_2\" end from s3object;")
    );
    assert_eq!(
        q("select count(*) from s3object where cast(int(_1) as bool) = true ;"),
        q("select count(*) from s3object where cast(_1 as int) != 0 ;")
    );
    assert!(count("select sum(int(_1)) from s3object;") > 0);
    // Rows with empty fields (s3-tests appends "1,2,,,,,,,,,,").
    let with_empty = format!("{data}1,2,,,,,,,,,,\n");
    let r = req("select int(_2) from s3object where (int(_2)*2 in(int(_3)*2,int(_4)*3,int(_5)*5)) = true;", csv_in(), csv_out());
    assert!(run(&r, with_empty.as_bytes()).is_ok());
    // test_csv_definition: "|" and "\t".
    let piped = random_csv(&mut rng, 500, 10, "|", "\t");
    let input = CsvInput { field_delimiter: "|".into(), record_delimiter: "\t".into(), ..CsvInput::default() };
    assert_eq!(csv_with("select count(0) from s3object;", input.clone(), &piped), "500\n");
    assert!(csv_with("select min(int(_1)),max(int(_2)),min(int(_3))+1 from s3object;", input, &piped).contains(','));
    // Random arithmetic like test_generate_projection / test_generate_where_clause.
    let one = random_csv(&mut rng, 1, 1, ",", "\n");
    assert_eq!(csv("select (((1.0+2.0)*(3.0/4.0))-((5.0*6.0)+(7.0-8.0))) from s3object;", &one), "-26.75\n");
    assert_eq!(csv("select count(0) from s3object where (((1.0+2.0)*(3.0/4.0))-((5.0*6.0)+(7.0-8.0)))<(10.0/3.0);", &one), "1\n");
    assert_eq!(csv("select (93.0/7.0) from s3object;", &one), format!("{}\n", 93.0f64 / 7.0));
}

#[test]
fn s3_tests_string_queries() {
    let mut rng = Rng(9);
    let letter = |rng: &mut Rng| -> char { ((if rng.below(2) == 0 { b'a' } else { b'A' }) + rng.below(26) as u8) as char };
    // create_random_csv_object_string
    let mut data = String::new();
    for _ in 0..1000 {
        for _ in 0..10 {
            if rng.below(10) == 5 {
                data += &(0..10).map(|_| letter(&mut rng)).collect::<String>();
                data += "aeiou,";
            } else {
                data += &(0..10).map(|_| format!("cbcd{}", letter(&mut rng))).collect::<String>();
                data += "vwxyzzvwxyz,";
            }
        }
        data += "\n";
    }
    let q = |sql: &str| csv(sql, &data);
    assert_eq!(q("select count(*) from stdin where _1 like \"%aeio%\";"), q("select count(*) from stdin where substring(_1,11,4) = \"aeio\" ;"));
    assert_eq!(q("select count(*) from stdin where _1 like \"cbcd%\";"), q("select count(*) from stdin where substring(_1,1,4) = \"cbcd\";"));
    assert_eq!(
        q("select count(*) from stdin where _2 like \"%yz\";"),
        q("select count(*) from stdin where substring(_2,character_length(_2),1) = \"z\" and substring(_2,character_length(_2)-1,1) = \"y\";")
    );
    assert_eq!(
        q("select count(*) from stdin where _3 like \"c%z\";"),
        q("select count(*) from stdin where substring(_3,character_length(_3),1) = \"z\" and substring(_3,1,1) = \"c\";")
    );
    assert_eq!(
        q("select count(*) from stdin where _2 like \"%xy_\";"),
        q("select count(*) from stdin where substring(_2,character_length(_2)-1,1) = \"y\" and substring(_2,character_length(_2)-2,1) = \"x\";")
    );
    // `[y-z]` is literal in S3 Select's LIKE (only % and _ are wildcards), unlike Ceph's.
    assert_eq!(q("select count(*) from stdin where _3 like \"%y[y-z]\";"), "0\n");
    assert_eq!(q("select (_1 like \"%aeio%\") from s3object ;"), q("select (substring(_1,11,4) = \"aeio\") from s3object ;"));
    assert_eq!(
        q("select count(*) from s3object where (_1 like \"cbcd%\") = true;"),
        q("select count(*) from s3object where substring(_1,1,4) = \"cbcd\";")
    );

    // create_random_csv_object_trim
    let mut trim_data = String::new();
    for _ in 0..1000 {
        for _ in 0..10 {
            trim_data += if rng.below(6) == 2 { "   aeiou    ," } else { "abcd," };
        }
        trim_data += "\n";
    }
    let t = |sql: &str| csv(sql, &trim_data);
    let want = t("select count(*) from s3object where substring(_1 from 4 for 5) = \"aeiou\";");
    assert_ne!(want, "0\n");
    for sql in [
        "select count(*) from s3object where trim(_1) = \"aeiou\";",
        "select count(*) from s3object where trim(both from _1) = \"aeiou\";",
        "select count(*) from s3object where trim(trailing from _1) = \"   aeiou\";",
        "select count(*) from s3object where trim(leading from _1) = \"aeiou    \";",
        "select count(*) from s3object where trim(trim(leading from _1)) = \"aeiou\";",
        "select count(*) from s3object where trim(_1) = \"aeiou\" = true;",
        "select count(*) from s3object where trim(leading from _1) = \"aeiou    \" = true;",
    ] {
        assert_eq!(t(sql), want, "{sql}");
    }

    // create_random_csv_object_escape
    let mut esc = String::new();
    for _ in 0..1000 {
        for _ in 0..10 {
            esc += if rng.below(10) == 5 { "_ar," } else { "aeio_," };
        }
        esc += "\n";
    }
    let e = |sql: &str| csv(sql, &esc);
    assert_eq!(
        e("select count(*) from s3object where _1 like \"%_ar\" escape \"%\";"),
        e("select count(*) from s3object where substring(_1,char_length(_1),1) = \"r\" and substring(_1,char_length(_1)-1,1) = \"a\" and substring(_1,char_length(_1)-2,1) = \"_\";")
    );
    assert_eq!(
        e("select count(*) from s3object where _1 like \"%aeio$_\" escape \"$\";"),
        e("select count(*) from s3object where substring(_1,1,5) = \"aeio_\";")
    );
}

#[test]
fn s3_tests_datetime_queries() {
    let mut rng = Rng(5);
    let mut data = String::new();
    for _ in 0..2000 {
        data += &format!(
            "{}{:02}{:02}T{:02}{:02}{:02}Z,\n",
            rng.below(101) + 1900,
            rng.below(12) + 1,
            rng.below(28) + 1,
            rng.below(24),
            rng.below(60),
            rng.below(60)
        );
    }
    let q = |sql: &str| csv(sql, &data);
    assert_eq!(
        q("select count(0) from  s3object where extract(year from to_timestamp(_1)) > 1950 and extract(year from to_timestamp(_1)) < 1960;"),
        q("select count(0) from  s3object where int(substring(_1,1,4))>1950 and int(substring(_1,1,4))<1960;")
    );
    assert_eq!(q("select cast(to_string(to_timestamp(_1), 'x') as int) from  s3object;"), "0\n".repeat(2000));
    assert_eq!(q("select extract(timezone_hour from to_timestamp(_1)) from  s3object;"), "0\n".repeat(2000));
    assert_eq!(
        q("select extract(month from to_timestamp(_1)) from s3object where extract(month from to_timestamp(_1)) = 5;"),
        q("select cast(substring(_1, 5, 2) as int) from s3object where _1 like '____05%';")
    );
    let all = q("select count(0) from  s3object;");
    assert_eq!(
        q("select count(0) from  s3object where  (date_diff(month,to_timestamp(_1),date_add(month,2,to_timestamp(_1)) ) = 2) = true;"),
        all
    );
    assert_eq!(
        q("select count(0) from  s3object where (date_diff(year,to_timestamp(_1),date_add(day, 366 ,to_timestamp(_1))) = 1) = true ;"),
        all
    );
    assert_eq!(q("select count(0) from  s3object where (date_diff(hour,utcnow(),date_add(day,1,utcnow())) = 24) = true ;"), all);
    assert_eq!(
        csv("select to_string(to_timestamp(_1), 'yyyy-MM-dd''T''HH:mm:ssXXX') from s3object", "19690720T201800Z\n"),
        "1969-07-20T20:18:00Z\n"
    );
    assert_eq!(csv("select to_timestamp(_1) from s3object", "2007-02-23T12:14:33.079-08:00\n"), "2007-02-23T12:14:33.079-08:00\n");
}
