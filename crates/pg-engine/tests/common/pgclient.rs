//! A small synchronous Postgres client (simple query protocol) that reports everything a
//! golden file records: command tags, notices, error fields and column types.

use bytes::{Buf, BytesMut};
use fallible_iterator::FallibleIterator;
use postgres_protocol::authentication::sasl::{ChannelBinding, ScramSha256};
use postgres_protocol::message::backend::{ErrorFields, Message};
use postgres_protocol::message::frontend;
use std::io::{Read, Write};
use std::net::TcpStream;

use super::{Event, Field};

pub struct PgClient {
    stream: TcpStream,
    buf: BytesMut,
    /// The transaction status of the last ReadyForQuery: `I`, `T` or `E`.
    pub status: u8,
    /// COPY FROM STDIN's data for the query in flight (`send_query`).
    copy_data: Option<String>,
    /// The result formats of the portal being read (`extended`): one for all, or per column.
    binary_cols: Vec<i16>,
    /// Report the portal's executions as one result (`extended`).
    merge: bool,
    /// The read timeout (`set_timeout`): a statement running longer fails the read.
    timeout: Option<std::time::Duration>,
}

/// A binary column value as compared: `\x` and hex, with the oids of user types (which
/// differ between servers) shown as `<user>`: an oid-valued type's value, an array's element
/// type.
fn binary_shown(ty: u32, b: &[u8]) -> String {
    let word = |at: usize| b.get(at..at + 4).map(|w| u32::from_be_bytes(w.try_into().unwrap()));
    if matches!(ty, 26 | 2205 | 2206 | 4096 | 4089) && b.len() == 4 && word(0).is_some_and(|o| o >= 16384) {
        return "\\x<user>".into();
    }
    let s = super::copy_hex(b);
    // (An array: ndim, the null flag, the element type.)
    if b.len() >= 12 && word(0).is_some_and(|n| n <= 6) && word(4).is_some_and(|f| f <= 1) && word(8).is_some_and(|o| o >= 16384) {
        return format!("{}<user>{}", &s[..2 + 16], &s[2 + 24..]);
    }
    s
}

fn fields(mut f: ErrorFields<'_>) -> Field {
    let mut out = Field::default();
    while let Some(x) = f.next().unwrap() {
        let v = String::from_utf8_lossy(x.value_bytes()).into_owned();
        match x.type_() {
            b'V' => out.severity = v,
            b'C' => out.code = v,
            b'M' => out.message = v,
            b'D' => out.detail = Some(v),
            b'H' => out.hint = Some(v),
            b'P' => out.position = v.parse().ok(),
            b'W' => out.context = Some(v),
            b'p' => {
                out.internal = Some((
                    v.parse().unwrap_or(0),
                    out.internal.take().map(|i| i.1).unwrap_or_default(),
                ))
            }
            b'q' => out.internal = Some((out.internal.take().map(|i| i.0).unwrap_or(0), v)),
            b's' => out.objects.push(format!("schema={v}")),
            b't' => out.objects.push(format!("table={v}")),
            b'c' => out.objects.push(format!("column={v}")),
            b'd' => out.objects.push(format!("datatype={v}")),
            b'n' => out.objects.push(format!("constraint={v}")),
            _ => {}
        }
    }
    out
}

impl PgClient {
    pub fn connect(
        addr: &str,
        user: &str,
        password: &str,
        database: &str,
    ) -> std::io::Result<PgClient> {
        let (c, events) = Self::connect_events(addr, user, password, database)?;
        match (c, events.iter().find(|e| matches!(e, Event::Error(_)))) {
            (Some(c), None) => Ok(c),
            (_, Some(Event::Error(f))) => {
                Err(std::io::Error::other(format!("{}: {}", f.code, f.message)))
            }
            _ => Err(std::io::Error::other("connection failed")),
        }
    }

    /// Connect, reporting what the server sent before it was ready: notices (a collation
    /// version WARNING, ...) and the error a refused connection ends with (then no client).
    pub fn connect_events(
        addr: &str,
        user: &str,
        password: &str,
        database: &str,
    ) -> std::io::Result<(Option<PgClient>, Vec<Event>)> {
        let mut events = Vec::new();
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        let mut c = PgClient {
            stream,
            buf: BytesMut::new(),
            status: b'I',
            copy_data: None,
            timeout: None,
            binary_cols: Vec::new(),
            merge: false,
        };
        let mut out = BytesMut::new();
        frontend::startup_message(
            [
                ("user", user),
                ("database", database),
                ("client_encoding", "UTF8"),
            ],
            &mut out,
        )?;
        c.stream.write_all(&out)?;
        let mut scram: Option<ScramSha256> = None;
        loop {
            match c.read()? {
                Message::AuthenticationOk => {}
                Message::AuthenticationCleartextPassword => {
                    let mut out = BytesMut::new();
                    frontend::password_message(password.as_bytes(), &mut out)?;
                    c.stream.write_all(&out)?;
                }
                Message::AuthenticationMd5Password(b) => {
                    let h = postgres_protocol::authentication::md5_hash(
                        user.as_bytes(),
                        password.as_bytes(),
                        b.salt(),
                    );
                    let mut out = BytesMut::new();
                    frontend::password_message(h.as_bytes(), &mut out)?;
                    c.stream.write_all(&out)?;
                }
                Message::AuthenticationSasl(_) => {
                    let s = ScramSha256::new(password.as_bytes(), ChannelBinding::unsupported());
                    let mut out = BytesMut::new();
                    frontend::sasl_initial_response("SCRAM-SHA-256", s.message(), &mut out)?;
                    c.stream.write_all(&out)?;
                    scram = Some(s);
                }
                Message::AuthenticationSaslContinue(b) => {
                    let s = scram.as_mut().unwrap();
                    s.update(b.data())?;
                    let mut out = BytesMut::new();
                    frontend::sasl_response(s.message(), &mut out)?;
                    c.stream.write_all(&out)?;
                }
                Message::AuthenticationSaslFinal(b) => scram.as_mut().unwrap().finish(b.data())?,
                Message::ErrorResponse(e) => {
                    events.push(Event::Error(fields(e.fields())));
                    return Ok((None, events));
                }
                Message::NoticeResponse(n) => events.push(Event::Notice(fields(n.fields()))),
                Message::ReadyForQuery(_) => return Ok((Some(c), events)),
                _ => {}
            }
        }
    }

    fn read(&mut self) -> std::io::Result<Message> {
        loop {
            if let Some(m) = Message::parse(&mut self.buf)? {
                return Ok(m);
            }
            let mut tmp = [0u8; 65536];
            let n = self.stream.read(&mut tmp)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed",
                ));
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Run one simple query; returns what the server sent, in order. A statement carrying COPY
    /// data (see `split_copy`) sends it when the server asks.
    pub fn simple_query(&mut self, stmt: &str) -> std::io::Result<Vec<Event>> {
        self.send_query(stmt)?;
        self.results()
    }

    /// Send a simple query without reading its results (`results` reads them): a statement
    /// that may wait for a lock another session holds.
    pub fn send_query(&mut self, stmt: &str) -> std::io::Result<()> {
        let (sql, copy_data) = super::split_copy(stmt);
        let mut out = BytesMut::new();
        frontend::query(sql, &mut out)?;
        self.copy_data = copy_data;
        self.stream.write_all(&out)
    }

    /// Whether the server sent something within `wait` (a statement sent with `send_query`
    /// finished, or failed, rather than waiting).
    pub fn readable_within(&mut self, wait: std::time::Duration) -> std::io::Result<bool> {
        if !self.buf.is_empty() {
            return Ok(true);
        }
        self.stream.set_read_timeout(Some(wait.max(std::time::Duration::from_millis(1))))?;
        let mut tmp = [0u8; 65536];
        let r = self.stream.read(&mut tmp);
        self.stream.set_read_timeout(self.timeout)?;
        match r {
            Ok(0) => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed")),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Fail reads that wait longer than `t` (a hang).
    pub fn set_timeout(&mut self, t: Option<std::time::Duration>) -> std::io::Result<()> {
        self.timeout = t;
        self.stream.set_read_timeout(t)
    }

    /// The extended protocol: Parse `sql` as statement `name` with `param_types` (unless
    /// `parse` is false: the statement exists), Describe it when `describe`, then Bind
    /// `params` (format, value) with `result_formats` to the unnamed portal, Describe and
    /// Execute it (at most `max_rows` rows a time, 0: all, executing again while suspended),
    /// and Sync. A binary column value is shown as `\x` and its bytes in hex.
    #[allow(clippy::too_many_arguments)]
    pub fn extended(
        &mut self,
        name: &str,
        sql: Option<&str>,
        param_types: &[u32],
        describe: bool,
        params: &[(i16, Option<Vec<u8>>)],
        result_formats: &[i16],
        max_rows: i32,
    ) -> std::io::Result<Vec<Event>> {
        let mut out = BytesMut::new();
        if let Some(sql) = sql {
            frontend::parse(name, sql, param_types.iter().copied(), &mut out)?;
        }
        if describe {
            frontend::describe(b'S', name, &mut out)?;
        }
        let mut body = vec![0u8];
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&(params.len() as i16).to_be_bytes());
        for (f, _) in params {
            body.extend_from_slice(&f.to_be_bytes());
        }
        body.extend_from_slice(&(params.len() as i16).to_be_bytes());
        for (_, v) in params {
            match v {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(b) => {
                    body.extend_from_slice(&(b.len() as i32).to_be_bytes());
                    body.extend_from_slice(b);
                }
            }
        }
        body.extend_from_slice(&(result_formats.len() as i16).to_be_bytes());
        for f in result_formats {
            body.extend_from_slice(&f.to_be_bytes());
        }
        out.extend_from_slice(b"B");
        out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        out.extend_from_slice(&body);
        frontend::describe(b'P', "", &mut out)?;
        frontend::execute("", max_rows, &mut out)?;
        if max_rows > 0 {
            // (A suspended portal runs again: twice more at most, then the rest.)
            frontend::execute("", max_rows, &mut out)?;
            frontend::execute("", 0, &mut out)?;
        }
        frontend::sync(&mut out);
        self.stream.write_all(&out)?;
        self.copy_data = None;
        self.binary_cols = result_formats.to_vec();
        self.merge = true;
        let r = self.results();
        self.binary_cols.clear();
        self.merge = false;
        r
    }

    /// Read what the server sends up to ReadyForQuery.
    pub fn results(&mut self) -> std::io::Result<Vec<Event>> {
        let copy_data = self.copy_data.take();
        let mut merged: Option<Vec<String>> = self.merge.then(Vec::new);
        let mut events = Vec::new();
        let mut cols: Option<Vec<(String, u32)>> = None;
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut binary_out = false;
        loop {
            match self.read()? {
                Message::RowDescription(d) => {
                    let mut f = d.fields();
                    let mut v = Vec::new();
                    while let Some(x) = f.next()? {
                        v.push((x.name().to_string(), x.type_oid()));
                    }
                    cols = Some(v);
                    rows.clear();
                }
                Message::DataRow(r) => {
                    let buf = r.buffer();
                    let mut ranges = r.ranges();
                    let mut row = Vec::new();
                    while let Some(rg) = ranges.next()? {
                        let i = row.len();
                        let bin = match self.binary_cols.len() {
                            0 => false,
                            1 => self.binary_cols[0] == 1,
                            _ => self.binary_cols.get(i) == Some(&1),
                        };
                        let ty = cols.as_ref().and_then(|c| c.get(i)).map_or(0, |c| c.1);
                        row.push(rg.map(|rg| if bin { binary_shown(ty, &buf[rg]) } else { String::from_utf8_lossy(&buf[rg]).into_owned() }));
                    }
                    rows.push(row);
                }
                Message::CommandComplete(c) => {
                    let tag = c.tag().map_err(std::io::Error::other)?.to_string();
                    if let Some(t) = merged.as_mut() {
                        // (The extended protocol's executions of one portal: one result.)
                        t.push(tag);
                        continue;
                    }
                    events.push(Event::Result {
                        cols: cols.take(),
                        rows: std::mem::take(&mut rows),
                        tag,
                    });
                }
                Message::EmptyQueryResponse => {}
                Message::ErrorResponse(e) => events.push(Event::Error(fields(e.fields()))),
                Message::NoticeResponse(n) => events.push(Event::Notice(fields(n.fields()))),
                Message::CopyInResponse(_) => {
                    let mut out = BytesMut::new();
                    if let Some(d) = &copy_data {
                        let d = super::copy_bytes(d);
                        if !d.is_empty() {
                            frontend::CopyData::new(&d[..])?.write(&mut out);
                        }
                        frontend::copy_done(&mut out);
                    } else {
                        frontend::copy_fail("COPY FROM STDIN without data", &mut out)?;
                    }
                    self.stream.write_all(&out)?;
                }
                Message::CopyOutResponse(r) => {
                    binary_out = r.format() == 1;
                    cols = Some(vec![("copy".into(), 25)]);
                }
                Message::CopyData(d) => {
                    let mut b = d.data();
                    let mut s = Vec::new();
                    while b.has_remaining() {
                        s.push(b.get_u8());
                    }
                    rows.push(vec![Some(if binary_out {
                        super::copy_hex(&s)
                    } else {
                        String::from_utf8_lossy(&s).trim_end_matches('\n').to_string()
                    })]);
                }
                Message::ReadyForQuery(r) => {
                    self.status = r.status();
                    if let Some(t) = merged.filter(|t| !t.is_empty() || cols.is_some()) {
                        events.push(Event::Result { cols: cols.take(), rows, tag: t.join("; ") });
                    }
                    return Ok(events);
                }
                Message::ParameterDescription(d) => {
                    let mut p = d.parameters();
                    let mut v = Vec::new();
                    while let Some(t) = p.next()? {
                        v.push(t.min(16384).to_string());
                    }
                    events.push(Event::Notice(super::Field { severity: "PARAMS".into(), message: v.join(","), ..Default::default() }));
                }
                Message::NoData => cols = None,
                Message::PortalSuspended => merged.get_or_insert_default().push("(suspended)".into()),
                _ => {}
            }
        }
    }
}

impl PgClient {
    /// A fastpath call (FunctionCall): the events up to ReadyForQuery, the result as the
    /// last one.
    pub fn function_call(&mut self, fc: &super::Fcall) -> std::io::Result<Vec<Event>> {
        let oid = match super::fcall_oid(
            fc,
            fc.lookup_sql().map(|q| self.simple_query(&q)).transpose()?,
        ) {
            Ok(oid) => oid,
            Err(events) => return Ok(events),
        };
        let mut body = Vec::new();
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&(fc.args.len() as i16).to_be_bytes());
        for a in &fc.args {
            body.extend_from_slice(&a.format().to_be_bytes());
        }
        body.extend_from_slice(&(fc.args.len() as i16).to_be_bytes());
        for a in &fc.args {
            match a.bytes() {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(b) => {
                    body.extend_from_slice(&(b.len() as i32).to_be_bytes());
                    body.extend_from_slice(&b);
                }
            }
        }
        body.extend_from_slice(&(fc.binary as i16).to_be_bytes());
        let mut msg = vec![b'F'];
        msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        msg.extend_from_slice(&body);
        self.stream.write_all(&msg)?;
        let mut events = Vec::new();
        loop {
            // FunctionCallResponse isn't a message postgres-protocol parses.
            while self.buf.len() < 5
                || self.buf.len()
                    < 1 + u32::from_be_bytes(self.buf[1..5].try_into().unwrap()) as usize
            {
                let mut tmp = [0u8; 65536];
                let n = self.stream.read(&mut tmp)?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed",
                    ));
                }
                self.buf.extend_from_slice(&tmp[..n]);
            }
            if self.buf[0] == b'V' {
                let len = u32::from_be_bytes(self.buf[1..5].try_into().unwrap()) as usize;
                let frame = self.buf.split_to(1 + len);
                let n = i32::from_be_bytes(frame[5..9].try_into().unwrap());
                events.push(fc.result((n >= 0).then(|| frame[9..9 + n as usize].to_vec())));
                continue;
            }
            match self.read()? {
                Message::ErrorResponse(e) => events.push(Event::Error(fields(e.fields()))),
                Message::NoticeResponse(n) => events.push(Event::Notice(fields(n.fields()))),
                Message::ReadyForQuery(_) => return Ok(events),
                _ => {}
            }
        }
    }
}

impl Drop for PgClient {
    fn drop(&mut self) {
        let mut out = BytesMut::new();
        frontend::terminate(&mut out);
        let _ = self.stream.write_all(&out);
    }
}
