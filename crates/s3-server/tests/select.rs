//! SelectObjectContent through the AWS SDK (which decodes the event stream, CRCs included) and
//! raw (the event stream's bytes checked frame by frame).

mod common;

use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    CompressionType, CsvInput, CsvOutput, ExpressionType, FileHeaderInfo, InputSerialization, JsonInput, JsonOutput, JsonType, OutputSerialization, RequestProgress, ScanRange,
    SelectObjectContentEventStream,
};
use common::*;

const PEOPLE: &[u8] = b"name,city,age\nada,london,36\nalan,london,41\ngrace,arlington,85\nlinus,helsinki,56\n";

fn csv_in(header: FileHeaderInfo, compression: CompressionType) -> InputSerialization {
    InputSerialization::builder().csv(CsvInput::builder().file_header_info(header).build()).compression_type(compression).build()
}

fn csv_out() -> OutputSerialization {
    OutputSerialization::builder().csv(CsvOutput::builder().build()).build()
}

/// What a select streams back: the records, and whether Stats, Progress and End came.
#[derive(Debug, Default)]
struct Outcome {
    records: Vec<u8>,
    records_events: usize,
    stats: Option<(i64, i64, i64)>,
    progress: usize,
    end: bool,
    error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn select(c: &Client, bucket: &str, key: &str, sql: &str, input: InputSerialization, output: OutputSerialization, progress: bool, range: Option<ScanRange>) -> Result<Outcome, String> {
    let mut req = c
        .select_object_content()
        .bucket(bucket)
        .key(key)
        .expression(sql)
        .expression_type(ExpressionType::Sql)
        .input_serialization(input)
        .output_serialization(output)
        .request_progress(RequestProgress::builder().enabled(progress).build());
    if let Some(r) = range {
        req = req.scan_range(r);
    }
    let mut out = req.send().await.map_err(|e| e.code().unwrap_or("?").to_string())?;
    let mut o = Outcome::default();
    loop {
        match out.payload.recv().await {
            Ok(Some(SelectObjectContentEventStream::Records(r))) => {
                o.records.extend_from_slice(r.payload().map(|p| p.as_ref()).unwrap_or_default());
                o.records_events += 1;
            }
            Ok(Some(SelectObjectContentEventStream::Stats(s))) => {
                let d = s.details().unwrap();
                o.stats = Some((d.bytes_scanned().unwrap_or(-1), d.bytes_processed().unwrap_or(-1), d.bytes_returned().unwrap_or(-1)));
            }
            Ok(Some(SelectObjectContentEventStream::Progress(_))) => o.progress += 1,
            Ok(Some(SelectObjectContentEventStream::End(_))) => o.end = true,
            Ok(Some(_)) => {}
            Ok(None) => return Ok(o),
            Err(e) => {
                o.error = Some(format!("{e:?}"));
                return Ok(o);
            }
        }
    }
}

async fn put(c: &Client, bucket: &str, key: &str, body: Vec<u8>) {
    c.put_object().bucket(bucket).key(key).body(ByteStream::from(body)).send().await.unwrap();
}

#[tokio::test]
async fn csv_queries_through_the_sdk() {
    let t = start().await;
    let c = &t.client;
    c.create_bucket().bucket("sel").send().await.unwrap();
    put(c, "sel", "people.csv", PEOPLE.to_vec()).await;
    let o = select(c, "sel", "people.csv", "SELECT s.name FROM S3Object s WHERE s.city = 'london'", csv_in(FileHeaderInfo::Use, CompressionType::None), csv_out(), false, None).await.unwrap();
    assert_eq!(String::from_utf8(o.records.clone()).unwrap(), "ada\nalan\n");
    assert!(o.end, "{o:?}");
    let (scanned, processed, returned) = o.stats.unwrap();
    assert_eq!((scanned, processed, returned), (PEOPLE.len() as i64, PEOPLE.len() as i64, 9));
    // Aggregates, positional columns, LIMIT, CAST and JSON output.
    let o = select(c, "sel", "people.csv", "SELECT COUNT(*), MAX(CAST(s.age AS INT)) FROM S3Object s", csv_in(FileHeaderInfo::Use, CompressionType::None), csv_out(), false, None).await.unwrap();
    assert_eq!(String::from_utf8(o.records).unwrap(), "4,85\n");
    let json = OutputSerialization::builder().json(JsonOutput::builder().build()).build();
    let o = select(c, "sel", "people.csv", "SELECT s._1 AS who, s._3 AS age FROM S3Object s LIMIT 2", csv_in(FileHeaderInfo::Ignore, CompressionType::None), json, false, None).await.unwrap();
    let lines: Vec<serde_json::Value> = String::from_utf8(o.records).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines, [serde_json::json!({"who": "ada", "age": "36"}), serde_json::json!({"who": "alan", "age": "41"})]);
    // Progress, when asked for.
    let o = select(c, "sel", "people.csv", "SELECT * FROM S3Object", csv_in(FileHeaderInfo::None, CompressionType::None), csv_out(), true, None).await.unwrap();
    assert!(o.progress >= 1, "{o:?}");
    assert_eq!(o.records, PEOPLE);
}

#[tokio::test]
async fn compressed_json_and_scan_ranges() {
    use std::io::Write;
    let t = start().await;
    let c = &t.client;
    c.create_bucket().bucket("sel").send().await.unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(PEOPLE).unwrap();
    let gz = gz.finish().unwrap();
    let n = gz.len() as i64;
    put(c, "sel", "people.csv.gz", gz).await;
    let o = select(c, "sel", "people.csv.gz", "SELECT s.name FROM S3Object s WHERE CAST(s.age AS INT) > 50", csv_in(FileHeaderInfo::Use, CompressionType::Gzip), csv_out(), false, None).await.unwrap();
    assert_eq!(String::from_utf8(o.records).unwrap(), "grace\nlinus\n");
    assert_eq!(o.stats.unwrap().0, n, "bytes scanned are the compressed ones");
    let lines = b"{\"a\": 1, \"tags\": [\"x\"]}\n{\"a\": 2, \"tags\": []}\n{\"a\": 3}\n";
    put(c, "sel", "rows.json", lines.to_vec()).await;
    let json_in = || InputSerialization::builder().json(JsonInput::builder().r#type(JsonType::Lines).build()).build();
    let json_out = || OutputSerialization::builder().json(JsonOutput::builder().build()).build();
    let o = select(c, "sel", "rows.json", "SELECT s.a FROM S3Object s WHERE s.a >= 2", json_in(), json_out(), false, None).await.unwrap();
    assert_eq!(String::from_utf8(o.records).unwrap(), "{\"a\":2}\n{\"a\":3}\n");
    // Records that start in the range: the second line starts at byte 24.
    let range = ScanRange::builder().start(1).end(30).build();
    let o = select(c, "sel", "rows.json", "SELECT s.a FROM S3Object s", json_in(), json_out(), false, Some(range)).await.unwrap();
    assert_eq!(String::from_utf8(o.records).unwrap(), "{\"a\":2}\n");
}

#[tokio::test]
async fn errors_before_and_during_the_stream() {
    let t = start().await;
    let c = &t.client;
    c.create_bucket().bucket("sel").send().await.unwrap();
    put(c, "sel", "people.csv", PEOPLE.to_vec()).await;
    let e = select(c, "sel", "people.csv", "SELEC name FROM S3Object", csv_in(FileHeaderInfo::Use, CompressionType::None), csv_out(), false, None).await.unwrap_err();
    assert!(e.starts_with("Parse") || e.starts_with("Lexer"), "{e}");
    let e = select(c, "sel", "missing.csv", "SELECT * FROM S3Object", csv_in(FileHeaderInfo::Use, CompressionType::None), csv_out(), false, None).await.unwrap_err();
    assert_eq!(e, "NoSuchKey");
    let parquet = InputSerialization::builder().parquet(aws_sdk_s3::types::ParquetInput::builder().build()).build();
    let e = select(c, "sel", "people.csv", "SELECT * FROM S3Object", parquet, csv_out(), false, None).await.unwrap_err();
    assert_eq!(e, "NotImplemented");
    // A cast that fails on a later row: an error event, after the records before it.
    let mut rows = b"n\n".to_vec();
    for i in 0..20_000 {
        rows.extend_from_slice(format!("{i}\n").as_bytes());
    }
    rows.extend_from_slice(b"oops\n");
    put(c, "sel", "numbers.csv", rows).await;
    let o = select(c, "sel", "numbers.csv", "SELECT CAST(s.n AS INT) + 1 FROM S3Object s", csv_in(FileHeaderInfo::Use, CompressionType::None), csv_out(), false, None).await.unwrap();
    assert!(o.error.is_some() && !o.end, "{:?}", o.error);
    // Directory buckets don't have S3 Select.
    c.create_bucket()
        .bucket("sel--use1-az4--x-s3")
        .create_bucket_configuration(
            aws_sdk_s3::types::CreateBucketConfiguration::builder()
                .location(aws_sdk_s3::types::LocationInfo::builder().r#type(aws_sdk_s3::types::LocationType::AvailabilityZone).name("use1-az4").build())
                .bucket(aws_sdk_s3::types::BucketInfo::builder().r#type(aws_sdk_s3::types::BucketType::Directory).build())
                .build(),
        )
        .send()
        .await
        .unwrap();
    put(c, "sel--use1-az4--x-s3", "people.csv", PEOPLE.to_vec()).await;
    let e = select(c, "sel--use1-az4--x-s3", "people.csv", "SELECT * FROM S3Object", csv_in(FileHeaderInfo::Use, CompressionType::None), csv_out(), false, None).await.unwrap_err();
    assert_eq!(e, "NotImplemented");
}

/// A big object streams as many Records events (bounded buffers), all of it arriving.
#[tokio::test]
async fn large_results_stream_in_many_events() {
    let t = start().await;
    let c = &t.client;
    c.create_bucket().bucket("sel").send().await.unwrap();
    let mut rows = Vec::new();
    for i in 0..200_000 {
        rows.extend_from_slice(format!("{i},row number {i},{}\n", i % 7).as_bytes());
    }
    let n = rows.len();
    put(c, "sel", "big.csv", rows.clone()).await;
    let o = select(c, "sel", "big.csv", "SELECT * FROM S3Object", csv_in(FileHeaderInfo::None, CompressionType::None), csv_out(), false, None).await.unwrap();
    assert_eq!(o.records.len(), n);
    assert!(o.records == rows, "every record, in order");
    assert!(o.records_events > 4, "{} events", o.records_events);
    let o = select(c, "sel", "big.csv", "SELECT COUNT(*) FROM S3Object s WHERE s._3 = '0'", csv_in(FileHeaderInfo::None, CompressionType::None), csv_out(), false, None).await.unwrap();
    assert_eq!(String::from_utf8(o.records).unwrap(), format!("{}\n", (0..200_000).filter(|i| i % 7 == 0).count()));
}

/// The wire format itself: every frame decodes with valid prelude and message CRCs, in S3's
/// order.
#[tokio::test]
async fn the_event_stream_on_the_wire() {
    let t = start().await;
    t.client.create_bucket().bucket("sel").send().await.unwrap();
    put(&t.client, "sel", "people.csv", PEOPLE.to_vec()).await;
    let body = "<SelectObjectContentRequest xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Expression>SELECT s.name FROM S3Object s</Expression><ExpressionType>SQL</ExpressionType>\
                <InputSerialization><CSV><FileHeaderInfo>USE</FileHeaderInfo></CSV></InputSerialization><OutputSerialization><CSV/></OutputSerialization></SelectObjectContentRequest>";
    let mut r = Raw::new("POST", "/sel/people.csv");
    r.query = "select=&select-type=2";
    r.body = body.as_bytes().to_vec();
    let resp = r.send(&t).await;
    assert_eq!(resp.status(), 200);
    let bytes = resp.bytes().await.unwrap();
    let mut at = 0;
    let mut kinds = Vec::new();
    let mut records = Vec::new();
    while at < bytes.len() {
        let (m, used) = s3_proto::eventstream::decode(&bytes[at..]).expect("a whole, valid frame");
        at += used;
        assert_eq!(m.header(":message-type"), Some("event"));
        let kind = m.header(":event-type").unwrap().to_string();
        if kind == "Records" {
            records.extend_from_slice(&m.payload);
        }
        kinds.push(kind);
    }
    assert_eq!(kinds.last().map(String::as_str), Some("End"));
    assert_eq!(kinds[kinds.len() - 2], "Stats");
    assert_eq!(String::from_utf8(records).unwrap(), "ada\nalan\ngrace\nlinus\n");
}
