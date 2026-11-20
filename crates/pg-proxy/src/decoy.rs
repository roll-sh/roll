//! A decoy role's login (roll secrets' Postgres decoys, docs/secrets.md "Decoys"), answered by
//! the proxy without waking a compute, as Postgres answers a wrong password: the same messages,
//! byte for byte, at the same pace, so a client can't tell a decoy from a role that exists (or
//! from one that doesn't).
//!
//! It's Postgres' own SCRAM-SHA-256 exchange (`auth-sasl.c` `CheckSASLAuth`, `auth-scram.c`
//! `scram_exchange` and the parsers it calls, `pqformat.c` for the message, `auth.c`
//! `auth_failed`, and `ProcessStartupPacket`'s `NegotiateProtocolVersion`), ported from
//! PostgreSQL 16.15, 17.11 and 18.6 (the PostgreSQL License) with every error Postgres would send
//! on the way: the same SQLSTATE, text, detail, and source file, line and routine, by major
//! version. The role is always refused: no password is checked, so none is ever held here.
//! `crates/pg-control/tests/e2e.rs` (`a_decoy_role_is_refused_as_a_wrong_password_is`) compares
//! every path here with a compute's own answers.

use bytes::{BufMut, BytesMut};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use crate::protocol::write_all;

/// What the router knows of a decoy role, from its one indexed read.
#[derive(Clone, Debug)]
pub struct Decoy {
    /// The role's SCRAM salt (base64 of 16 bytes): Postgres answers one role with one salt.
    pub salt: String,
    /// The endpoint's Postgres major version: its protocol and its errors' source lines.
    pub pg_version: u32,
    /// How long a real login would wait for the compute to start (zero when it's running).
    pub start: Duration,
}

/// What the endpoint's compute answered a wrong password with, around the user's name (its
/// language and source line, whatever they are): a decoy's refusal is the same bytes.
#[derive(Clone, Debug)]
pub struct Refusal {
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

impl Refusal {
    /// A compute's ErrorResponse fields (`Message` without its tag and length) if it's a wrong
    /// password's (`28P01`) and names `user` exactly once in its message.
    pub fn learn(fields: &[u8], user: &str) -> Option<Refusal> {
        let mut at = 0;
        let (mut code, mut message) = (None, None);
        while let Some(&tag) = fields.get(at) {
            if tag == 0 {
                break;
            }
            let end = at + 1 + fields[at + 1..].iter().position(|&b| b == 0)?;
            match tag {
                b'C' => code = Some(&fields[at + 1..end]),
                b'M' => message = Some(at + 1..end),
                _ => {}
            }
            at = end + 1;
        }
        let message = message?;
        if code? != b"28P01" || user.is_empty() {
            return None;
        }
        let text = &fields[message.clone()];
        let found: Vec<usize> = text.windows(user.len()).enumerate().filter(|(_, w)| *w == user.as_bytes()).map(|(i, _)| i).collect();
        let [i] = found[..] else { return None };
        let cut = message.start + i;
        Some(Refusal { before: fields[..cut].to_vec(), after: fields[cut + user.len()..].to_vec() })
    }
}

/// Postgres' `authentication_timeout` (its default): a client that stalls is dropped, silently.
const AUTH_TIMEOUT: Duration = Duration::from_secs(60);
/// `PG_MAX_SASL_MESSAGE_LENGTH`: a longer SASL message ends the exchange as a wrong password.
const MAX_SASL_MESSAGE: usize = 1024;
/// SCRAM's iteration count: Postgres' `scram_iterations` default, and what roll's roles use.
const ITERATIONS: u32 = 4096;
/// `SCRAM_RAW_NONCE_LEN`.
const RAW_NONCE: usize = 18;
/// `SCRAM_MAX_KEY_LEN` for SHA-256: a proof is 32 bytes.
const PROOF_LEN: usize = 32;

/// Where Postgres raises an error: its file, routine, and line in 16.15, 17.11 and 18.6.
#[derive(Clone, Copy, Debug)]
struct Site(&'static str, &'static str, [u32; 3]);

const AUTH_FAILED: Site = Site("auth.c", "auth_failed", [331, 329, 317]);
const EXPECTED_SASL: Site = Site("auth-sasl.c", "CheckSASLAuth", [95, 95, 87]);
const INVALID_MECH: Site = Site("auth-scram.c", "scram_init", [268, 261, 265]);
const EMPTY: Site = Site("auth-scram.c", "scram_exchange", [383, 376, 380]);
const LENGTH: Site = Site("auth-scram.c", "scram_exchange", [388, 381, 385]);
const NONCE: Site = Site("auth-scram.c", "scram_exchange", [421, 414, 418]);
const ATTR_EXPECTED: Site = Site("auth-scram.c", "read_attr_value", [742, 735, 749]);
const ATTR_EQUALS: Site = Site("auth-scram.c", "read_attr_value", [750, 743, 757]);
const ANY_END: Site = Site("auth-scram.c", "read_any_attr", [855, 848, 862]);
const ANY_INVALID: Site = Site("auth-scram.c", "read_any_attr", [868, 861, 875]);
const ANY_EQUALS: Site = Site("auth-scram.c", "read_any_attr", [878, 871, 885]);
const FIRST_COMMA_N: Site = Site("auth-scram.c", "read_client_first_message", [993, 986, 1000]);
const FIRST_COMMA_Y: Site = Site("auth-scram.c", "read_client_first_message", [1023, 1016, 1030]);
const FIRST_CBIND_DATA: Site = Site("auth-scram.c", "read_client_first_message", [1037, 1030, 1044]);
const FIRST_FLAG: Site = Site("auth-scram.c", "read_client_first_message", [1055, 1048, 1062]);
const FIRST_AUTHZID: Site = Site("auth-scram.c", "read_client_first_message", [1066, 1059, 1073]);
const FIRST_ATTR: Site = Site("auth-scram.c", "read_client_first_message", [1070, 1063, 1077]);
const FIRST_EXT: Site = Site("auth-scram.c", "read_client_first_message", [1087, 1080, 1094]);
const FIRST_NONCE: Site = Site("auth-scram.c", "read_client_first_message", [1101, 1094, 1108]);
const FINAL_CBIND: Site = Site("auth-scram.c", "read_client_final_message", [1366, 1359, 1372]);
const FINAL_PROOF: Site = Site("auth-scram.c", "read_client_final_message", [1384, 1377, 1390]);
const FINAL_GARBAGE: Site = Site("auth-scram.c", "read_client_final_message", [1392, 1385, 1398]);
const MSG_RAWSTRING: Site = Site("pqformat.c", "pq_getmsgrawstring", [625, 622, 622]);
const MSG_COPY: Site = Site("pqformat.c", "pq_copymsgbytes", [534, 531, 531]);
const MSG_BYTES: Site = Site("pqformat.c", "pq_getmsgbytes", [516, 513, 513]);
const MSG_END: Site = Site("pqformat.c", "pq_getmsgend", [641, 638, 638]);
/// `ProcessStartupPacket`: in `postmaster.c` before 17 ([`Site::file`]).
const REPLICATION: Site = Site("backend_startup.c", "ProcessStartupPacket", [2233, 763, 791]);

impl Site {
    fn file(self, at: usize) -> &'static str {
        if self.1 == REPLICATION.1 && at == 0 { "postmaster.c" } else { self.0 }
    }
}

const PROTOCOL_VIOLATION: &str = "08P01";
const FEATURE_NOT_SUPPORTED: &str = "0A000";
const MALFORMED: &str = "malformed SCRAM message";

/// A FATAL error Postgres raises before the password is checked.
#[derive(Debug)]
struct Fatal {
    site: Site,
    code: &'static str,
    message: String,
    detail: Option<String>,
    hint: Option<&'static str>,
}

fn fatal(site: Site, code: &'static str, message: impl Into<String>) -> Box<Fatal> {
    Box::new(Fatal { site, code, message: message.into(), detail: None, hint: None })
}

fn malformed(site: Site, detail: impl Into<String>) -> Box<Fatal> {
    Box::new(Fatal { site, code: PROTOCOL_VIOLATION, message: MALFORMED.into(), detail: Some(detail.into()), hint: None })
}

/// How the exchange ends.
enum End {
    /// The client went away (or stalled): Postgres exits without a word.
    Gone,
    /// An error before the password is checked.
    Fatal(Box<Fatal>),
    /// A wrong password, as every complete exchange is.
    Refused,
}

impl From<Box<Fatal>> for End {
    fn from(f: Box<Fatal>) -> End {
        End::Fatal(f)
    }
}

/// Answer a decoy role's login on `client`: `version` and `params` as the startup packet had them
/// (what a compute would have been sent), `user` the role, `warm` how long a compute that's up
/// takes to answer a startup, `refusal` what this endpoint's compute says to a wrong password.
pub async fn refuse<S>(client: &mut S, version: i32, params: &[(String, String)], user: &str, decoy: &Decoy, warm: Duration, refusal: Option<&Refusal>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let v = Version::of(decoy.pg_version);
    tokio::time::sleep(decoy.start + warm).await;
    let answer = match tokio::time::timeout(AUTH_TIMEOUT, exchange(client, version, params, decoy, v)).await {
        Ok(Ok(end)) => end,
        Ok(Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => End::Gone,
        Ok(Err(e)) => return Err(e),
        Err(_) => End::Gone,
    };
    let bytes = match answer {
        End::Gone => return Ok(()),
        End::Fatal(f) => v.error(&f),
        End::Refused => match refusal {
            Some(r) => message(b'E', &[&r.before[..], user.as_bytes(), &r.after[..]].concat()),
            None => v.error(&fatal(AUTH_FAILED, "28P01", format!("password authentication failed for user \"{user}\""))),
        },
    };
    // The client may be gone already: Postgres writes into the void too.
    let _ = write_all(client, &bytes).await;
    Ok(())
}

/// A Postgres major version's protocol and source lines.
#[derive(Clone, Copy)]
struct Version {
    major: u32,
    /// Index into each site's lines.
    at: usize,
}

impl Version {
    fn of(major: u32) -> Version {
        Version { major, at: major.clamp(16, 18) as usize - 16 }
    }

    /// `PG_PROTOCOL_LATEST`'s minor: 3.2 from 18.
    fn latest_minor(self) -> i32 {
        if self.major >= 18 { 2 } else { 0 }
    }

    fn error(self, f: &Fatal) -> BytesMut {
        let site = f.site;
        let mut fields = BytesMut::new();
        let line = site.2[self.at].to_string();
        let mut put = |tag: u8, value: &str| {
            fields.put_u8(tag);
            fields.put_slice(value.as_bytes());
            fields.put_u8(0);
        };
        put(b'S', "FATAL");
        put(b'V', "FATAL");
        put(b'C', f.code);
        put(b'M', &f.message);
        if let Some(d) = &f.detail {
            put(b'D', d);
        }
        if let Some(h) = f.hint {
            put(b'H', h);
        }
        put(b'F', site.file(self.at));
        put(b'L', &line);
        put(b'R', site.1);
        fields.put_u8(0);
        message(b'E', &fields)
    }
}

fn message(tag: u8, body: &[u8]) -> BytesMut {
    let mut out = BytesMut::with_capacity(body.len() + 5);
    out.put_u8(tag);
    out.put_i32(body.len() as i32 + 4);
    out.put_slice(body);
    out
}

/// `AuthenticationSASLContinue` (11) or the SASL request (10) with `data`.
fn auth(code: i32, data: &[u8]) -> BytesMut {
    let mut body = BytesMut::with_capacity(data.len() + 4);
    body.put_i32(code);
    body.put_slice(data);
    message(b'R', &body)
}

async fn exchange<S>(client: &mut S, version: i32, params: &[(String, String)], decoy: &Decoy, v: Version) -> io::Result<End>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // ProcessStartupPacket: a bad `replication` is refused before anything else.
    if let Some((_, value)) = params.iter().find(|(k, value)| k == "replication" && value != "database" && parse_bool(value).is_none()) {
        let mut f = fatal(REPLICATION, "22023", format!("invalid value for parameter \"replication\": \"{value}\""));
        f.hint = Some("Valid values are: \"false\", 0, \"true\", 1, \"database\".");
        return Ok(f.into());
    }
    let mut out = BytesMut::new();
    let unknown: Vec<&str> = params.iter().filter(|(k, _)| k.starts_with("_pq_.")).map(|(k, _)| k.as_str()).collect();
    let minor = version & 0xffff;
    if minor > v.latest_minor() || !unknown.is_empty() {
        let mut body = BytesMut::new();
        body.put_i32((version & !0xffff) | minor.min(v.latest_minor()));
        body.put_i32(unknown.len() as i32);
        for name in unknown {
            body.put_slice(name.as_bytes());
            body.put_u8(0);
        }
        out.extend_from_slice(&message(b'v', &body));
    }
    out.extend_from_slice(&auth(10, b"SCRAM-SHA-256\0\0"));
    write_all(client, &out).await?;

    // CheckSASLAuth's loop.
    let mut initial = true;
    let mut salt_sent: Option<(u8, Vec<u8>, String)> = None;
    loop {
        let tag = match client.read_u8().await {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(End::Gone),
            Err(e) => return Err(e),
        };
        if tag != b'p' {
            return Ok(fatal(EXPECTED_SASL, PROTOCOL_VIOLATION, format!("expected SASL response, got message type {tag}")).into());
        }
        // pq_getmessage: a bad length or a short read ends the exchange as a failed one.
        let Some(body) = read_body(client).await? else { return Ok(End::Refused) };
        let input = match sasl_input(&body, std::mem::take(&mut initial)) {
            Ok(input) => input,
            Err(f) => return Ok(f.into()),
        };
        // scram_exchange.
        let Some(input) = input else {
            write_all(client, &auth(11, b"")).await?;
            continue;
        };
        if input.is_empty() {
            return Ok(malformed(EMPTY, "The message is empty.").into());
        }
        if input.contains(&0) {
            return Ok(malformed(LENGTH, "Message length does not match input length.").into());
        }
        match &salt_sent {
            None => {
                let (flag, client_nonce) = match client_first(input) {
                    Ok(f) => f,
                    Err(f) => return Ok(f.into()),
                };
                let server_nonce = nonce();
                let first = format!("r={}{server_nonce},s={},i={ITERATIONS}", String::from_utf8_lossy(client_nonce), decoy.salt);
                write_all(client, &auth(11, first.as_bytes())).await?;
                salt_sent = Some((flag, client_nonce.to_vec(), server_nonce));
            }
            Some((flag, client_nonce, server_nonce)) => {
                let final_nonce = match client_final(input, *flag) {
                    Ok(n) => n,
                    Err(f) => return Ok(f.into()),
                };
                if final_nonce != [&client_nonce[..], server_nonce.as_bytes()].concat() {
                    let mut f = fatal(NONCE, PROTOCOL_VIOLATION, "invalid SCRAM response");
                    f.detail = Some("Nonce does not match.".into());
                    return Ok(f.into());
                }
                return Ok(End::Refused);
            }
        }
    }
}

/// The SASL payload of a `p` message: the first names the mechanism and may carry none.
fn sasl_input(body: &[u8], initial: bool) -> Result<Option<&[u8]>, Box<Fatal>> {
    let insufficient = |site| fatal(site, PROTOCOL_VIOLATION, "insufficient data left in message");
    let mut m = Msg { buf: body, at: 0 };
    let input = if initial {
        let mech = m.rawstring().ok_or_else(|| fatal(MSG_RAWSTRING, PROTOCOL_VIOLATION, "invalid string in message"))?;
        if mech != b"SCRAM-SHA-256" {
            return Err(fatal(INVALID_MECH, PROTOCOL_VIOLATION, "client selected an invalid SASL authentication mechanism"));
        }
        let len = m.int32().ok_or_else(|| insufficient(MSG_COPY))?;
        if len == -1 { None } else { Some(m.bytes(len).ok_or_else(|| insufficient(MSG_BYTES))?) }
    } else {
        Some(m.rest())
    };
    if m.at != body.len() {
        return Err(fatal(MSG_END, PROTOCOL_VIOLATION, "invalid message format"));
    }
    Ok(input)
}

/// One SASL message's body, bounded as Postgres bounds it; `None` where Postgres gives up.
async fn read_body<S: AsyncRead + Unpin>(client: &mut S) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    if let Err(e) = client.read_exact(&mut len).await {
        return if e.kind() == io::ErrorKind::UnexpectedEof { Ok(None) } else { Err(e) };
    }
    let len = i32::from_be_bytes(len);
    let Ok(len) = usize::try_from(len) else { return Ok(None) };
    if !(4..=MAX_SASL_MESSAGE).contains(&len) {
        return Ok(None);
    }
    let mut body = vec![0u8; len - 4];
    match client.read_exact(&mut body).await {
        Ok(_) => Ok(Some(body)),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// `pqformat.c`'s reader over one message.
struct Msg<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Msg<'a> {
    fn rawstring(&mut self) -> Option<&'a [u8]> {
        let rest = &self.buf[self.at..];
        let n = rest.iter().position(|&b| b == 0)?;
        self.at += n + 1;
        Some(&rest[..n])
    }
    fn int32(&mut self) -> Option<i32> {
        let b = self.buf.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(i32::from_be_bytes(b.try_into().ok()?))
    }
    fn bytes(&mut self, len: i32) -> Option<&'a [u8]> {
        let len = usize::try_from(len).ok()?;
        let b = self.buf.get(self.at..self.at.checked_add(len)?)?;
        self.at += len;
        Some(b)
    }
    fn rest(&mut self) -> &'a [u8] {
        let b = &self.buf[self.at..];
        self.at = self.buf.len();
        b
    }
}

/// A random server nonce: 18 bytes, base64 (24 characters, no padding).
fn nonce() -> String {
    let mut raw = [0u8; RAW_NONCE];
    getrandom::fill(&mut raw).expect("os randomness");
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    raw.chunks(3)
        .flat_map(|c| {
            let n = u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2]);
            [18, 12, 6, 0].map(|s| B64[(n >> s & 63) as usize] as char)
        })
        .collect()
}

/// The byte at `p`, or the terminator past the end (the C code reads its string's `\0`).
fn at(input: &[u8], p: usize) -> u8 {
    input.get(p).copied().unwrap_or(0)
}

/// `sanitize_char`.
fn shown(c: u8) -> String {
    if (0x21..=0x7e).contains(&c) { format!("'{}'", c as char) } else { format!("0x{c:02x}") }
}

/// `read_attr_value`: `attr=value` at `p`; the value and where the next attribute starts.
fn attr_value(input: &[u8], p: usize, attr: u8) -> Result<(&[u8], usize), Box<Fatal>> {
    if at(input, p) != attr {
        return Err(malformed(ATTR_EXPECTED, format!("Expected attribute \"{}\" but found \"{}\".", attr as char, shown(at(input, p)))));
    }
    if at(input, p + 1) != b'=' {
        return Err(malformed(ATTR_EQUALS, format!("Expected character \"=\" for attribute \"{}\".", attr as char)));
    }
    Ok(value_from(input, p + 2))
}

/// `read_any_attr`: any `x=value` at `p`; the attribute, the value and where the next starts.
fn any_attr(input: &[u8], p: usize) -> Result<(u8, &[u8], usize), Box<Fatal>> {
    let attr = at(input, p);
    if attr == 0 {
        return Err(malformed(ANY_END, "Attribute expected, but found end of string."));
    }
    if !attr.is_ascii_alphabetic() {
        return Err(malformed(ANY_INVALID, format!("Attribute expected, but found invalid character \"{}\".", shown(attr))));
    }
    if at(input, p + 1) != b'=' {
        return Err(malformed(ANY_EQUALS, format!("Expected character \"=\" for attribute \"{}\".", attr as char)));
    }
    let (value, next) = value_from(input, p + 2);
    Ok((attr, value, next))
}

fn value_from(input: &[u8], begin: usize) -> (&[u8], usize) {
    let end = input[begin..].iter().position(|&b| b == b',').map_or(input.len(), |n| begin + n);
    (&input[begin..end], if end < input.len() { end + 1 } else { end })
}

/// `read_client_first_message` without channel binding (TLS ends before the compute, so it never
/// offers SCRAM-SHA-256-PLUS): the channel-binding flag and the client's nonce.
fn client_first(input: &[u8]) -> Result<(u8, &[u8]), Box<Fatal>> {
    let flag = at(input, 0);
    let mut p = 1;
    match flag {
        b'n' | b'y' => {
            if at(input, p) != b',' {
                let site = if flag == b'n' { FIRST_COMMA_N } else { FIRST_COMMA_Y };
                return Err(malformed(site, format!("Comma expected, but found character \"{}\".", shown(at(input, p)))));
            }
            p += 1;
        }
        b'p' => return Err(malformed(FIRST_CBIND_DATA, "The client selected SCRAM-SHA-256 without channel binding, but the SCRAM message includes channel binding data.")),
        c => return Err(malformed(FIRST_FLAG, format!("Unexpected channel-binding flag \"{}\".", shown(c)))),
    }
    if at(input, p) == b'a' {
        return Err(fatal(FIRST_AUTHZID, FEATURE_NOT_SUPPORTED, "client uses authorization identity, but it is not supported"));
    }
    if at(input, p) != b',' {
        return Err(malformed(FIRST_ATTR, format!("Unexpected attribute \"{}\" in client-first-message.", shown(at(input, p)))));
    }
    p += 1;
    if at(input, p) == b'm' {
        return Err(fatal(FIRST_EXT, FEATURE_NOT_SUPPORTED, "client requires an unsupported SCRAM extension"));
    }
    let (_, next) = attr_value(input, p, b'n')?;
    let (nonce, mut p) = attr_value(input, next, b'r')?;
    if !nonce.iter().all(|&c| (0x21..=0x7e).contains(&c) && c != b',') {
        return Err(fatal(FIRST_NONCE, PROTOCOL_VIOLATION, "non-printable characters in SCRAM nonce"));
    }
    while at(input, p) != 0 {
        p = any_attr(input, p)?.2;
    }
    Ok((flag, nonce))
}

/// `read_client_final_message` without channel binding: the final nonce.
fn client_final(input: &[u8], flag: u8) -> Result<&[u8], Box<Fatal>> {
    let (binding, p) = attr_value(input, 0, b'c')?;
    if !(binding == b"biws" && flag == b'n') && !(binding == b"eSws" && flag == b'y') {
        return Err(fatal(FINAL_CBIND, PROTOCOL_VIOLATION, "unexpected SCRAM channel-binding attribute in client-final-message"));
    }
    let (nonce, mut p) = attr_value(input, p, b'r')?;
    let proof = loop {
        let (attr, value, next) = any_attr(input, p)?;
        p = next;
        if attr == b'p' {
            break value;
        }
    };
    if b64_decoded_len(proof) != Some(PROOF_LEN) {
        return Err(malformed(FINAL_PROOF, "Malformed proof in client-final-message."));
    }
    if at(input, p) != 0 {
        return Err(malformed(FINAL_GARBAGE, "Garbage found at the end of client-final-message."));
    }
    Ok(nonce)
}

/// How many bytes `pg_b64_decode` makes of `src` (into a buffer of `pg_b64_dec_len`), or `None`
/// where it fails: its rules exactly, padding quirks included.
fn b64_decoded_len(src: &[u8]) -> Option<usize> {
    let room = (src.len() * 3) >> 2;
    let (mut pos, mut end, mut n) = (0, 0, 0usize);
    for &c in src {
        if c == b'=' {
            if end == 0 {
                end = match pos {
                    2 => 1,
                    3 => 2,
                    _ => return None,
                };
            }
        } else if !(c.is_ascii_alphanumeric() || c == b'+' || c == b'/') {
            return None;
        }
        pos += 1;
        if pos == 4 {
            n += 1 + usize::from(end == 0 || end > 1) + usize::from(end == 0 || end > 2);
            if n > room {
                return None;
            }
            pos = 0;
        }
    }
    (pos == 0).then_some(n)
}

/// `parse_bool`: what `replication` takes besides `database`.
fn parse_bool(v: &str) -> Option<bool> {
    let l = v.to_ascii_lowercase();
    let prefix = |word: &str, min: usize| l.len() >= min.max(1) && word.starts_with(&l);
    match *l.as_bytes().first()? {
        b't' if prefix("true", 1) => Some(true),
        b'f' if prefix("false", 1) => Some(false),
        b'y' if prefix("yes", 1) => Some(true),
        b'n' if prefix("no", 1) => Some(false),
        b'o' if prefix("on", 2) => Some(true),
        b'o' if prefix("off", 2) => Some(false),
        b'1' if l.len() == 1 => Some(true),
        b'0' if l.len() == 1 => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_follows_postgres() {
        let proof = "A".repeat(43) + "=";
        assert_eq!(b64_decoded_len(proof.as_bytes()), Some(32));
        assert_eq!(b64_decoded_len(b"AAAA"), Some(3));
        assert_eq!(b64_decoded_len(b"AA=="), Some(1));
        assert_eq!(b64_decoded_len(b"AAA"), None);
        assert_eq!(b64_decoded_len(b"A AA"), None);
        assert_eq!(b64_decoded_len(b"=AAA"), None);
    }

    #[test]
    fn replication_values_follow_parse_bool() {
        for ok in ["true", "t", "TR", "yes", "y", "on", "off", "of", "1", "0", "no", "f"] {
            assert!(parse_bool(ok).is_some(), "{ok}");
        }
        for bad in ["", "o", "2", "truee", "database2", "nope"] {
            assert!(parse_bool(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_learned_refusal_wraps_the_name() {
        let fields = b"SFATAL\0VFATAL\0C28P01\0Mpassword authentication failed for user \"roll_abc\"\0Fauth.c\0L317\0Rauth_failed\0\0";
        let r = Refusal::learn(fields, "roll_abc").unwrap();
        assert_eq!([&r.before[..], b"roll_abc", &r.after[..]].concat(), fields);
        // A name the message has twice can't be cut out.
        assert!(Refusal::learn(fields, "a").is_none());
        assert!(Refusal::learn(b"SFATAL\0C08P01\0Mroll_abc\0\0", "roll_abc").is_none());
    }
}
