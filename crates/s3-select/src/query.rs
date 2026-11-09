//! [`Query`]: the pipeline from the object's bytes (decompress → split records → filter →
//! project or aggregate → serialize) with bounded memory.

use std::cell::Cell as StdCell;
use std::collections::VecDeque;
use std::ops::Range;

use crate::ast::{AggFunc, Expr, FromStep, Name, Select, SelectList};
use crate::bind::{Bound, Schema, bind};
use crate::csv::{BOM, Flow, Quoting, Splitter, split_fields};
use crate::decompress::Decoder;
use crate::error::{Result, SelectError};
use crate::eval::{Ctx, Fields, Row, eval};
use crate::jsondoc::{DocReader, from_path};
use crate::output::{Cell, Output};
use crate::request::{
    CsvInput, FileHeaderInfo, Input, JsonType, SelectRequest, validate_csv_input, validate_output, validate_scan_range,
};
use crate::ts::Timestamp;
use crate::value::{Num, Value, compare, parse_json, parse_num};

/// Once this much output is buffered, processing pauses (the rest of the input is kept) until
/// the caller takes the output; see [`Query::backlogged`].
pub const OUTPUT_HIGH_WATER: usize = 1 << 20;
/// Decompressed bytes are produced and processed this much at a time.
pub const DECOMPRESS_CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub bytes_scanned: u64,
    pub bytes_processed: u64,
    pub bytes_returned: u64,
}

/// A compiled query, fed the object's bytes as they stream in. No I/O, no async.
///
/// Memory is bounded whatever the input: a record (input or output) is at most 1 MiB, decompression
/// works [`DECOMPRESS_CHUNK`] at a time, JSON documents stream (only selected rows are buffered),
/// and output pauses at [`OUTPUT_HIGH_WATER`]. To keep it bounded when a little input makes a lot
/// of output (a decompression bomb, `SELECT _1 || _1 …`), drain a backlog before feeding more:
///
/// ```ignore
/// for chunk in object {
///     q.feed(&chunk)?;
///     while q.backlogged() {
///         send(q.take_output());
///         q.feed(&[])?;
///     }
///     if q.pending_output() > 256 * 1024 { send(q.take_output()); }
///     if q.done() { break; }
/// }
/// q.finish()?;
/// send(q.take_output());
/// ```
///
/// (Without that loop the result is the same; unprocessed input is just held until fed again.)
pub struct Query {
    engine: Engine,
    spec: Spec,
    reader: Option<Reader>,
    decoder: Option<Decoder>,
    dec_buf: Vec<u8>,
    dec_pos: usize,
    stash: Vec<u8>,
    stash_pos: usize,
    scan: Option<crate::request::ScanRange>,
    plan: StdCell<Option<ScanPlan>>,
    scanned: u64,
    processed: u64,
    done: bool,
    finished: bool,
    failed: Option<SelectError>,
}

/// Which bytes are fed, and which records count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScanPlan {
    /// Absolute offset of the first byte fed.
    first: u64,
    /// Records starting before this are skipped.
    start: u64,
    /// Records starting after this are not processed (inclusive).
    end: u64,
}

enum Spec {
    Csv(CsvInput),
    Lines(Vec<FromStep>),
    Doc(Vec<FromStep>),
}

impl Query {
    /// Parse and check the SQL against the input format.
    pub fn new(req: &SelectRequest) -> Result<Query> {
        if req.input == Input::Parquet {
            return Err(SelectError::new(
                "NotImplemented",
                "Parquet input is not supported by roll storage's S3 Select (it needs random access and whole row groups in memory); use CSV or JSON.",
            )
            .with_status(501));
        }
        if let Some(sr) = &req.scan_range {
            validate_scan_range(&req.input, req.compression, sr)?;
        }
        validate_output(&req.output)?;
        let select = crate::parser::parse(&req.expression)?;
        let (kind, spec) = match &req.input {
            Input::Csv(c) => {
                validate_csv_input(c)?;
                if !select.from.steps.is_empty() {
                    return Err(SelectError::new("InvalidKeyPath", "A path in FROM (S3Object[*].…) only applies to JSON input."));
                }
                (Kind::Csv { header_info: c.file_header_info }, Spec::Csv(c.clone()))
            }
            Input::Json(j) => {
                let f = &select.from;
                let mut aliases = Vec::new();
                match &f.alias {
                    Some(a) => aliases.push(a.clone()),
                    None => {
                        aliases.push(Name { text: "s3object".into(), quoted: false });
                        match f.steps.last() {
                            Some(FromStep::Name(n)) => aliases.push(n.clone()),
                            Some(_) => aliases.push(Name { text: "_1".into(), quoted: false }),
                            None if f.wildcard => aliases.push(Name { text: "_1".into(), quoted: false }),
                            None => {}
                        }
                    }
                }
                let steps = f.steps.clone();
                (Kind::Json { aliases }, if j.kind == JsonType::Lines { Spec::Lines(steps) } else { Spec::Doc(steps) })
            }
            Input::Parquet => unreachable!(),
        };
        let mut engine = Engine::new(select, kind, &req.output);
        if !matches!(engine.kind, Kind::Csv { header_info: FileHeaderInfo::Use }) {
            engine.bind()?;
        }
        Ok(Query {
            engine,
            spec,
            reader: None,
            decoder: Decoder::new(req.compression),
            dec_buf: Vec::new(),
            dec_pos: 0,
            stash: Vec::new(),
            stash_pos: 0,
            scan: req.scan_range,
            plan: StdCell::new(None),
            scanned: 0,
            processed: 0,
            done: false,
            finished: false,
            failed: None,
        })
    }

    /// Which bytes of an object of `object_size` bytes to feed (`None`: nothing needs reading).
    ///
    /// With a ScanRange, a record is processed iff it starts within `[start, end]`; one that
    /// starts inside and ends past `end` is processed whole, so the range returned runs to the end
    /// of the object and [`Query::done`] turns true once the scan range's records are done. To
    /// tell whether a record starts exactly at `start`, the range starts one record delimiter
    /// before it (`start - delimiter length`). With FileHeaderInfo USE it starts at 0, since the
    /// header is needed. Call this before feeding when the request has a ScanRange.
    pub fn input_range(&self, object_size: u64) -> Option<Range<u64>> {
        if object_size == 0 {
            return None;
        }
        let Some(sr) = self.scan else {
            self.plan.set(Some(ScanPlan { first: 0, start: 0, end: u64::MAX }));
            return Some(0..object_size);
        };
        let last = object_size - 1;
        let (start, end) = match (sr.start, sr.end) {
            (Some(s), Some(e)) => (s, e),
            (Some(s), None) => (s, last),
            (None, Some(0)) => return None,
            (None, Some(n)) => (object_size.saturating_sub(n), last),
            (None, None) => (0, last),
        };
        let end = end.min(last);
        if start > end {
            return None;
        }
        let (dlen, use_header) = match &self.spec {
            Spec::Csv(c) => (c.record_delimiter.len() as u64, c.file_header_info == FileHeaderInfo::Use),
            _ => (1, false),
        };
        let first = if use_header { 0 } else { start.saturating_sub(dlen) };
        self.plan.set(Some(ScanPlan { first, start, end }));
        Some(first..object_size)
    }

    fn scan_plan(&self) -> ScanPlan {
        if let Some(p) = self.plan.get() {
            return p;
        }
        match self.scan {
            Some(sr) if sr.start.is_some() => ScanPlan { first: 0, start: sr.start.unwrap_or(0), end: sr.end.unwrap_or(u64::MAX) },
            _ => ScanPlan { first: 0, start: 0, end: u64::MAX },
        }
    }

    fn ensure_reader(&mut self) {
        if self.reader.is_some() {
            return;
        }
        let p = self.scan_plan();
        self.reader = Some(match &self.spec {
            Spec::Csv(c) => {
                let quote = c.quote_character.as_bytes()[0];
                let esc = c.quote_escape_character.as_bytes()[0];
                let comment = c.comments.as_ref().map(|s| s.as_bytes()[0]);
                let fd = c.field_delimiter.as_bytes().to_vec();
                let quoting = c.allow_quoted_record_delimiter.then(|| Quoting { fd: fd.clone(), quote, esc, comment });
                Reader::Csv(CsvReader {
                    split: Splitter::new(c.record_delimiter.as_bytes(), quoting, p.first, p.first, p.end),
                    fd,
                    quote,
                    esc,
                    comment,
                    header_info: c.file_header_info,
                    // Reading from mid-object: the object's first line (a header to IGNORE) isn't fed.
                    header_done: c.file_header_info == FileHeaderInfo::None || (p.first > 0 && c.file_header_info == FileHeaderInfo::Ignore),
                    skip_before: p.start,
                    fields: Fields::default(),
                })
            }
            Spec::Lines(steps) => Reader::Lines(LinesReader {
                split: Splitter::new(b"\n", None, p.first, p.first, p.end),
                steps: steps.clone(),
                pending: VecDeque::new(),
                skip_before: p.start,
            }),
            Spec::Doc(steps) => Reader::Doc(DocReader::new(steps.clone())),
        });
    }

    /// Run `raw` (as fed) through the pipeline; returns how much was consumed (less than all
    /// only when output paused).
    fn process(&mut self, raw: &[u8]) -> Result<usize> {
        self.ensure_reader();
        let Some(reader) = self.reader.as_mut() else { return Err(SelectError::internal("no reader")) };
        let Some(dec) = self.decoder.as_mut() else {
            let (n, flow) = reader.feed(raw, &mut self.engine)?;
            self.processed += n as u64;
            if flow == Flow::Done {
                self.done = true;
                return Ok(raw.len());
            }
            return Ok(n);
        };
        if self.dec_buf.capacity() < DECOMPRESS_CHUNK {
            self.dec_buf.reserve_exact(DECOMPRESS_CHUNK);
        }
        let mut rc = 0;
        loop {
            if self.dec_pos < self.dec_buf.len() {
                let (n, flow) = reader.feed(&self.dec_buf[self.dec_pos..], &mut self.engine)?;
                self.dec_pos += n;
                self.processed += n as u64;
                match flow {
                    Flow::Done => {
                        self.done = true;
                        return Ok(raw.len());
                    }
                    Flow::Pause => return Ok(rc),
                    Flow::Continue => {}
                }
            } else if self.engine.paused() {
                // Processed everything decompressed so far, but the output is full.
                return Ok(rc);
            }
            self.dec_buf.clear();
            self.dec_pos = 0;
            let n = dec.decompress(&raw[rc..], &mut self.dec_buf)?;
            rc += n;
            if self.dec_buf.is_empty() {
                if rc >= raw.len() {
                    return Ok(rc);
                }
                if n == 0 {
                    return Err(SelectError::internal("decompression made no progress"));
                }
            }
        }
    }

    /// Feed the next bytes (compressed if the request says so). Output records accumulate in an
    /// internal buffer. An error ends the query (later calls return it again).
    pub fn feed(&mut self, data: &[u8]) -> Result<()> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        if self.finished {
            return Err(SelectError::internal("feed after finish"));
        }
        self.scanned += data.len() as u64;
        if self.done() {
            return Ok(());
        }
        let r = self.feed_inner(data);
        if let Err(e) = &r {
            self.failed = Some(e.clone());
        }
        r
    }

    fn feed_inner(&mut self, data: &[u8]) -> Result<()> {
        if self.stash_pos < self.stash.len() {
            self.stash.extend_from_slice(data);
            let stash = std::mem::take(&mut self.stash);
            let r = self.process(&stash[self.stash_pos..]);
            self.stash = stash;
            self.stash_pos += r?;
            if self.done || self.stash_pos >= self.stash.len() {
                self.stash.clear();
                self.stash_pos = 0;
            } else if self.stash_pos > self.stash.len() / 2 {
                self.stash.drain(..self.stash_pos);
                self.stash_pos = 0;
            }
        } else {
            let n = self.process(data)?;
            if !self.done && n < data.len() {
                self.stash.clear();
                self.stash.extend_from_slice(&data[n..]);
                self.stash_pos = 0;
            }
        }
        Ok(())
    }

    /// The input ended: process the last record and emit aggregate results.
    pub fn finish(&mut self) -> Result<()> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        if self.finished {
            return Ok(());
        }
        let r = self.finish_inner();
        match &r {
            Err(e) => self.failed = Some(e.clone()),
            Ok(()) => self.finished = true,
        }
        r
    }

    fn finish_inner(&mut self) -> Result<()> {
        self.engine.draining = true;
        if !self.done() && self.stash_pos < self.stash.len() {
            let stash = std::mem::take(&mut self.stash);
            let pos = std::mem::take(&mut self.stash_pos);
            self.process(&stash[pos..])?;
        }
        if !self.done() && self.decoder.is_some() {
            self.process(&[])?;
            if !self.done()
                && let Some(d) = &self.decoder
            {
                d.finish()?;
            }
        }
        if !self.done() {
            self.ensure_reader();
            if let Some(r) = self.reader.as_mut()
                && r.finish(&mut self.engine)? == Flow::Done
            {
                self.done = true;
            }
        }
        self.engine.finish()
    }

    /// Output bytes produced so far (and clears the buffer).
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.engine.out.buf)
    }

    /// Bytes currently buffered.
    pub fn pending_output(&self) -> usize {
        self.engine.out.buf.len()
    }

    /// No more input is needed (LIMIT reached, or past the scan range): stop reading and call
    /// [`Query::finish`].
    pub fn done(&self) -> bool {
        self.done || self.engine.done
    }

    /// Input was held back because the output buffer filled up: take the output and call
    /// `feed(&[])` until this is false before feeding more (that keeps memory bounded).
    pub fn backlogged(&self) -> bool {
        !self.done()
            && (self.stash_pos < self.stash.len()
                || self.dec_pos < self.dec_buf.len()
                || matches!(&self.reader, Some(Reader::Lines(l)) if !l.pending.is_empty()))
    }

    /// Scanned = bytes fed (compressed); processed = after decompression; returned = output
    /// bytes produced.
    pub fn stats(&self) -> Stats {
        Stats { bytes_scanned: self.scanned, bytes_processed: self.processed, bytes_returned: self.engine.out.produced }
    }
}

enum Reader {
    Csv(CsvReader),
    Lines(LinesReader),
    Doc(DocReader),
}

impl Reader {
    fn feed(&mut self, data: &[u8], eng: &mut Engine) -> Result<(usize, Flow)> {
        match self {
            Reader::Csv(r) => r.feed(data, eng),
            Reader::Lines(r) => r.feed(data, eng),
            Reader::Doc(r) => r.feed(data, &mut |v| eng.row(Row::Json(&v))),
        }
    }

    fn finish(&mut self, eng: &mut Engine) -> Result<Flow> {
        match self {
            Reader::Csv(r) => r.finish(eng),
            Reader::Lines(r) => r.finish(eng),
            Reader::Doc(r) => r.finish(&mut |v| eng.row(Row::Json(&v))),
        }
    }
}

struct CsvReader {
    split: Splitter,
    fd: Vec<u8>,
    quote: u8,
    esc: u8,
    comment: Option<u8>,
    header_info: FileHeaderInfo,
    header_done: bool,
    skip_before: u64,
    fields: Fields,
}

impl CsvReader {
    fn feed(&mut self, data: &[u8], eng: &mut Engine) -> Result<(usize, Flow)> {
        let CsvReader { split, fd, quote, esc, comment, header_info, header_done, skip_before, fields } = self;
        let mut f = |rec: &[u8], start: u64| {
            on_csv_record(rec, start, fd, *quote, *esc, *comment, *header_info, header_done, *skip_before, fields, eng)
        };
        split.feed(data, &mut f)
    }

    fn finish(&mut self, eng: &mut Engine) -> Result<Flow> {
        let CsvReader { split, fd, quote, esc, comment, header_info, header_done, skip_before, fields } = self;
        let mut f = |rec: &[u8], start: u64| {
            on_csv_record(rec, start, fd, *quote, *esc, *comment, *header_info, header_done, *skip_before, fields, eng)
        };
        split.finish(&mut f)
    }
}

#[allow(clippy::too_many_arguments)]
fn on_csv_record(
    rec: &[u8],
    start: u64,
    fd: &[u8],
    quote: u8,
    esc: u8,
    comment: Option<u8>,
    header_info: FileHeaderInfo,
    header_done: &mut bool,
    skip_before: u64,
    fields: &mut Fields,
    eng: &mut Engine,
) -> Result<Flow> {
    let rec = if start == 0 { rec.strip_prefix(BOM).unwrap_or(rec) } else { rec };
    if rec.is_empty() || comment.is_some_and(|c| rec[0] == c) {
        return Ok(Flow::Continue);
    }
    if !*header_done {
        *header_done = true;
        match header_info {
            FileHeaderInfo::Use => {
                split_fields(rec, fd, quote, esc, usize::MAX, fields);
                let names = (0..fields.len()).map(|i| fields.text(i).map(|t| t.unwrap_or("").to_string())).collect::<Result<Vec<_>>>()?;
                eng.set_header(names)?;
                return Ok(Flow::Continue);
            }
            FileHeaderInfo::Ignore => return Ok(Flow::Continue),
            FileHeaderInfo::None => {}
        }
    }
    if start < skip_before {
        return Ok(Flow::Continue);
    }
    split_fields(rec, fd, quote, esc, eng.needed_columns(), fields);
    eng.row(Row::Csv(fields))
}

struct LinesReader {
    split: Splitter,
    steps: Vec<FromStep>,
    /// Rows of a line not yet processed (output paused in the middle of a line's rows).
    pending: VecDeque<Value>,
    skip_before: u64,
}

impl LinesReader {
    fn drain(&mut self, eng: &mut Engine) -> Result<Flow> {
        while let Some(v) = self.pending.pop_front() {
            let flow = eng.row(Row::Json(&v))?;
            if flow != Flow::Continue {
                return Ok(flow);
            }
        }
        Ok(Flow::Continue)
    }

    fn feed(&mut self, data: &[u8], eng: &mut Engine) -> Result<(usize, Flow)> {
        let flow = self.drain(eng)?;
        if flow != Flow::Continue {
            return Ok((0, flow));
        }
        let LinesReader { split, steps, pending, skip_before } = self;
        let mut f = |rec: &[u8], start: u64| on_line(rec, start, steps, pending, *skip_before, eng);
        split.feed(data, &mut f)
    }

    fn finish(&mut self, eng: &mut Engine) -> Result<Flow> {
        let flow = self.drain(eng)?;
        if flow == Flow::Done {
            return Ok(flow);
        }
        let LinesReader { split, steps, pending, skip_before } = self;
        let mut f = |rec: &[u8], start: u64| on_line(rec, start, steps, pending, *skip_before, eng);
        let flow = split.finish(&mut f)?;
        if flow == Flow::Done {
            return Ok(flow);
        }
        self.drain(eng)
    }
}

fn on_line(rec: &[u8], start: u64, steps: &[FromStep], pending: &mut VecDeque<Value>, skip_before: u64, eng: &mut Engine) -> Result<Flow> {
    let rec = if start == 0 { rec.strip_prefix(BOM).unwrap_or(rec) } else { rec };
    if start < skip_before || rec.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Flow::Continue);
    }
    let mut rows = Vec::new();
    from_path(parse_json(rec)?, steps, &mut rows);
    let mut it = rows.into_iter();
    while let Some(v) = it.next() {
        let flow = eng.row(Row::Json(&v))?;
        if flow != Flow::Continue {
            pending.extend(it);
            return Ok(flow);
        }
    }
    Ok(Flow::Continue)
}

enum Kind {
    Csv { header_info: FileHeaderInfo },
    Json { aliases: Vec<Name> },
}

/// An aggregate's running state.
#[derive(Debug)]
enum Acc {
    Count(i64),
    Sum { int: i64, float: f64, is_float: bool, any: bool },
    Avg { sum: f64, n: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

impl Acc {
    fn new(f: AggFunc) -> Acc {
        match f {
            AggFunc::Count => Acc::Count(0),
            AggFunc::Sum => Acc::Sum { int: 0, float: 0.0, is_float: false, any: false },
            AggFunc::Avg => Acc::Avg { sum: 0.0, n: 0 },
            AggFunc::Min => Acc::Min(None),
            AggFunc::Max => Acc::Max(None),
        }
    }

    fn num(v: &Value, f: &str) -> Result<Num> {
        match v {
            Value::Int(i) => Ok(Num::Int(*i)),
            Value::Float(x) => Ok(Num::Float(*x)),
            Value::Str(s) => parse_num(s).ok_or_else(|| {
                SelectError::new("InvalidDataType", format!("{f} needs numbers, and {s:?} is not a number; CAST it or filter it out."))
            }),
            v => Err(SelectError::new("InvalidDataType", format!("{f} needs numbers, not {}.", v.type_name()))),
        }
    }

    /// Add a value (`None`: a COUNT(*) row).
    fn add(&mut self, v: Option<Value>) -> Result<()> {
        let Some(v) = v else {
            if let Acc::Count(n) = self {
                *n += 1;
            }
            return Ok(());
        };
        if v.is_absent() {
            return Ok(());
        }
        let is_min = matches!(self, Acc::Min(_));
        match self {
            Acc::Count(n) => *n += 1,
            Acc::Sum { int, float, is_float, any } => {
                *any = true;
                match Self::num(&v, "SUM")? {
                    Num::Int(i) if !*is_float => *int = int.checked_add(i).ok_or_else(SelectError::overflow)?,
                    n => {
                        if !*is_float {
                            *is_float = true;
                            *float = *int as f64;
                        }
                        *float += match n {
                            Num::Int(i) => i as f64,
                            Num::Float(x) => x,
                        };
                    }
                }
            }
            Acc::Avg { sum, n } => {
                *sum += match Self::num(&v, "AVG")? {
                    Num::Int(i) => i as f64,
                    Num::Float(x) => x,
                };
                *n += 1;
            }
            Acc::Min(cur) | Acc::Max(cur) => {
                let v = match v {
                    Value::Str(s) => parse_num(&s).map(Num::value).unwrap_or(Value::Str(s)),
                    v => v,
                };
                match cur {
                    None => *cur = Some(v),
                    Some(c) => match compare(&v, c) {
                        Some(o) if (is_min && o.is_lt()) || (!is_min && o.is_gt()) => *cur = Some(v),
                        Some(_) => {}
                        None => {
                            return Err(SelectError::new(
                                "InvalidDataType",
                                format!("{} can't compare {} with {}.", if is_min { "MIN" } else { "MAX" }, v.type_name(), c.type_name()),
                            ));
                        }
                    },
                }
            }
        }
        Ok(())
    }

    fn result(&self) -> Value {
        match self {
            Acc::Count(n) => Value::Int(*n),
            Acc::Sum { any: false, .. } => Value::Null,
            Acc::Sum { float, is_float: true, .. } => Value::Float(*float),
            Acc::Sum { int, .. } => Value::Int(*int),
            Acc::Avg { n: 0, .. } => Value::Null,
            Acc::Avg { sum, n } => Value::Float(*sum / *n as f64),
            Acc::Min(v) | Acc::Max(v) => v.clone().unwrap_or(Value::Null),
        }
    }
}

/// Filters, projects or aggregates rows and writes the output.
struct Engine {
    select: Select,
    kind: Kind,
    bound: Option<Bound>,
    header: Option<Vec<String>>,
    /// `_1`, `_2`, … for `SELECT *` output names.
    positional: Vec<String>,
    out: Output,
    now: Timestamp,
    emitted: u64,
    accs: Vec<Acc>,
    /// Finishing: never pause.
    draining: bool,
    done: bool,
}

impl Engine {
    fn new(select: Select, kind: Kind, out: &crate::request::Output) -> Engine {
        let done = select.limit == Some(0);
        Engine {
            select,
            kind,
            bound: None,
            header: None,
            positional: Vec::new(),
            out: Output::new(out),
            now: Timestamp::now(),
            emitted: 0,
            accs: Vec::new(),
            draining: false,
            done,
        }
    }

    fn bind(&mut self) -> Result<()> {
        let schema = match &self.kind {
            Kind::Csv { header_info } => Schema::Csv {
                header: self.header.as_deref(),
                quoted_literals: *header_info != FileHeaderInfo::Use,
                alias: self.select.from.alias.as_ref(),
            },
            Kind::Json { aliases } => Schema::Json { aliases: aliases.clone() },
        };
        let b = bind(&self.select, &schema)?;
        self.accs = b.aggs.iter().map(|(f, _)| Acc::new(*f)).collect();
        self.bound = Some(b);
        Ok(())
    }

    fn set_header(&mut self, names: Vec<String>) -> Result<()> {
        self.header = Some(names);
        self.bind()
    }

    fn needed_columns(&self) -> usize {
        self.bound.as_ref().map_or(usize::MAX, |b| b.needed_columns)
    }

    fn paused(&self) -> bool {
        !self.draining && self.out.buf.len() >= OUTPUT_HIGH_WATER
    }

    fn row(&mut self, row: Row<'_>) -> Result<Flow> {
        if self.done {
            return Ok(Flow::Done);
        }
        let Some(b) = &self.bound else { return Err(SelectError::internal("the query isn't bound")) };
        let cx = Ctx { now: &self.now, aggs: &[] };
        if let Some(f) = &b.filter
            && eval(f, row, &cx)? != Value::Bool(true)
        {
            return Ok(Flow::Continue);
        }
        if b.aggregate {
            for (acc, (_, arg)) in self.accs.iter_mut().zip(&b.aggs) {
                let v = arg.as_ref().map(|a| eval(a, row, &cx)).transpose()?;
                acc.add(v)?;
            }
            return Ok(Flow::Continue);
        }
        if b.star {
            match row {
                Row::Csv(f) => {
                    while self.positional.len() < f.len() {
                        self.positional.push(format!("_{}", self.positional.len() + 1));
                    }
                    let header = self.header.as_deref().unwrap_or(&[]);
                    let mut cells = Vec::with_capacity(f.len());
                    for i in 0..f.len() {
                        let name = header.get(i).unwrap_or(&self.positional[i]);
                        cells.push((name.as_str(), Cell::Text(f.text(i)?.unwrap_or(""))));
                    }
                    self.out.record(&cells)?;
                }
                Row::Json(Value::Struct(fields)) => {
                    let cells: Vec<(&str, Cell)> = fields.iter().map(|(k, v)| (k.as_str(), Cell::Value(v))).collect();
                    self.out.record(&cells)?;
                }
                Row::Json(Value::Missing) => self.out.record(&[])?,
                Row::Json(v) => self.out.record(&[("_1", Cell::Value(v))])?,
                Row::None => {}
            }
        } else {
            let vals = b.columns.iter().map(|(_, e)| eval(e, row, &cx)).collect::<Result<Vec<_>>>()?;
            let cells: Vec<(&str, Cell)> = b.columns.iter().zip(&vals).map(|((n, _), v)| (n.as_str(), Cell::Value(v))).collect();
            self.out.record(&cells)?;
        }
        self.emitted += 1;
        if self.select.limit.is_some_and(|l| self.emitted >= l) {
            self.done = true;
            return Ok(Flow::Done);
        }
        Ok(if self.paused() { Flow::Pause } else { Flow::Continue })
    }

    fn has_aggregates(&self) -> bool {
        let mut found = false;
        if let SelectList::Items(items) = &self.select.items {
            for it in items {
                it.expr.walk(&mut |e| found |= matches!(e, Expr::Agg(..)));
            }
        }
        found
    }

    /// The input ended: an aggregate query's one record.
    fn finish(&mut self) -> Result<()> {
        if self.bound.is_none() {
            // CSV with a header, but the object was empty.
            if !self.has_aggregates() {
                return Ok(());
            }
            self.header = Some(Vec::new());
            self.bind()?;
        }
        let Some(b) = &self.bound else { return Ok(()) };
        if !b.aggregate || self.select.limit == Some(0) {
            return Ok(());
        }
        let vals: Vec<Value> = self.accs.iter().map(Acc::result).collect();
        let cx = Ctx { now: &self.now, aggs: &vals };
        let out = b.columns.iter().map(|(_, e)| eval(e, Row::None, &cx)).collect::<Result<Vec<_>>>()?;
        let cells: Vec<(&str, Cell)> = b.columns.iter().zip(&out).map(|((n, _), v)| (n.as_str(), Cell::Value(v))).collect();
        self.out.record(&cells)?;
        self.emitted += 1;
        Ok(())
    }
}
