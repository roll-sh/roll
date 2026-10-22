//! roll storage's S3 Select (SelectObjectContent): the request body, the SQL dialect, and the
//! input and output formats. No I/O and no async: [`Query`] is fed the object's bytes and
//! produces the Records payload; the caller does the event stream.
//!
//! Supported, as AWS documents S3 Select:
//! - Input: CSV (FileHeaderInfo NONE/USE/IGNORE, field and record delimiters of 1–2 characters,
//!   `\r\n` records with the default `\n`, quote and escape characters, comment lines,
//!   AllowQuotedRecordDelimiter) and JSON (DOCUMENT, streamed; LINES), uncompressed, GZIP
//!   (multi-member) or BZIP2 (multi-stream); ScanRange on uncompressed CSV and JSON LINES.
//! - Output: CSV (QuoteFields ALWAYS/ASNEEDED, delimiters, quote, escape) and JSON.
//! - SQL: `SELECT` (`*`, expressions, aliases) `FROM S3Object[*].path [AS] alias` `WHERE …`
//!   `LIMIT n`; operators (`AND OR NOT`, comparisons, `LIKE … ESCAPE`, `BETWEEN`, `IN`,
//!   `IS [NOT] NULL|MISSING`, arithmetic, `||`), `CASE`, `CAST`, the string, date, conditional
//!   and aggregate functions, and the bool, int, float/decimal, string and timestamp types with
//!   NULL and MISSING.
//! - Cheap, unambiguous extensions from Ceph's dialect (each is an error in AWS's): `FROM stdin`,
//!   `INT(x)`/`FLOAT(x)` for CAST, SELECT-list aliases referenced elsewhere in a CSV query, a
//!   double-quoted name read as a string when a CSV object has no header, and ISO 8601 basic
//!   timestamps (`20200101T101010Z`).
//!
//! Not supported: Parquet input (NotImplemented: it needs random access and whole row groups in
//! memory). DECIMAL is computed as FLOAT.
//!
//! Memory is bounded whatever the input; see [`Query`].

mod ast;
mod bind;
mod csv;
mod decompress;
mod error;
mod eval;
mod jsondoc;
mod lexer;
mod output;
mod parser;
mod query;
mod request;
mod ts;
mod value;

pub use error::SelectError;
pub use eval::MAX_RECORD_BYTES;
pub use parser::MAX_EXPRESSION_BYTES;
pub use query::{DECOMPRESS_CHUNK, OUTPUT_HIGH_WATER, Query, Stats};
pub use request::{
    Compression, CsvInput, CsvOutput, FileHeaderInfo, Input, JsonInput, JsonOutput, JsonType, MAX_REQUEST_BYTES, Output,
    QuoteFields, ScanRange, SelectRequest, parse_request,
};

#[cfg(test)]
mod tests;
