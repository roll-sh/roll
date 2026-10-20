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
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        let mut c = PgClient {
            stream,
            buf: BytesMut::new(),
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
                    let f = fields(e.fields());
                    return Err(std::io::Error::other(format!("{}: {}", f.code, f.message)));
                }
                Message::ReadyForQuery(_) => return Ok(c),
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
        let (sql, copy_data) = super::split_copy(stmt);
        let mut out = BytesMut::new();
        frontend::query(sql, &mut out)?;
        self.stream.write_all(&out)?;
        let mut events = Vec::new();
        let mut cols: Option<Vec<(String, u32)>> = None;
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
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
                        row.push(rg.map(|rg| String::from_utf8_lossy(&buf[rg]).into_owned()));
                    }
                    rows.push(row);
                }
                Message::CommandComplete(c) => {
                    let tag = c.tag().map_err(std::io::Error::other)?.to_string();
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
                        if !d.is_empty() {
                            frontend::CopyData::new(d.as_bytes())?.write(&mut out);
                        }
                        frontend::copy_done(&mut out);
                    } else {
                        frontend::copy_fail("COPY FROM STDIN without data", &mut out)?;
                    }
                    self.stream.write_all(&out)?;
                }
                Message::CopyOutResponse(_) => {
                    cols = Some(vec![("copy".into(), 25)]);
                }
                Message::CopyData(d) => {
                    let mut b = d.data();
                    let mut s = Vec::new();
                    while b.has_remaining() {
                        s.push(b.get_u8());
                    }
                    rows.push(vec![Some(
                        String::from_utf8_lossy(&s)
                            .trim_end_matches('\n')
                            .to_string(),
                    )]);
                }
                Message::ReadyForQuery(_) => return Ok(events),
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
