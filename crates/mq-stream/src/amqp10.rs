//! A self-contained AMQP 1.0 message codec and RabbitMQ's AMQP 1.0 ⇄ 0-9-1 conversions
//! (`mc_amqpl`).
//!
//! Stream clients publish AMQP 1.0 encoded messages. We keep those bytes verbatim for stream
//! consumers and derive the 0-9-1 view other protocols see ([`convert_in`]); messages that came
//! in over AMQP 0-9-1 are encoded to AMQP 1.0 when a stream consumer reads them
//! ([`encode_message`]). Both directions write straight into caller buffers and borrow from the
//! input, so the publish and deliver paths don't allocate per message.

use crate::proto::{Malformed, R};
use bytes::{BufMut, BytesMut};
use mq_core::message::Message;
use mq_core::value::{FieldTable, FieldValue};

const HEADER: u64 = 0x70;
const DELIVERY_ANNOTATIONS: u64 = 0x71;
const MESSAGE_ANNOTATIONS: u64 = 0x72;
const PROPERTIES: u64 = 0x73;
const APPLICATION_PROPERTIES: u64 = 0x74;
const DATA: u64 = 0x75;
const AMQP_SEQUENCE: u64 = 0x76;
const AMQP_VALUE: u64 = 0x77;
const FOOTER: u64 = 0x78;
/// The 0-9-1 `type` of a message whose body is encoded AMQP 1.0 sections.
pub const AMQP10_TYPE: &str = "amqp-1.0";
/// Nesting bound for conversions of hostile input.
const MAX_DEPTH: usize = 32;

/// A decoded AMQP 1.0 value borrowing from the encoded message.
#[derive(Clone, Copy, Debug)]
pub enum Val<'a> {
    Null,
    Bool(bool),
    Ubyte(u8),
    Ushort(u16),
    Uint(u32),
    Ulong(u64),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Decimal,
    Char,
    Timestamp(i64),
    Uuid(&'a [u8]),
    Binary(&'a [u8]),
    Str(&'a [u8]),
    Sym(&'a [u8]),
    List(Seq<'a>),
    Map(Seq<'a>),
    Array(Arr<'a>),
    Described(Desc<'a>, &'a [u8]),
}

/// Items of a list (or keys and values of a map) encoded back to back.
#[derive(Clone, Copy, Debug)]
pub struct Seq<'a> {
    pub count: u32,
    pub body: &'a [u8],
}

/// Array elements sharing one constructor.
#[derive(Clone, Copy, Debug)]
pub struct Arr<'a> {
    pub count: u32,
    /// `None` for described element types (no 0-9-1 equivalent).
    pub ctor: Option<u8>,
    pub body: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub enum Desc<'a> {
    Code(u64),
    Sym(&'a [u8]),
}

/// Decoder over an encoded value stream.
pub struct Cur<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Cur { b, pos: 0 }
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.b.len()
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Malformed)?;
        let s = self.b.get(self.pos..end).ok_or(Malformed)?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }

    fn be<const N: usize>(&mut self) -> R<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Malformed)
    }

    fn u32(&mut self) -> R<u32> {
        self.be().map(u32::from_be_bytes)
    }

    /// The next value (constructor and data). Iterative, so hostile nesting can't recurse.
    pub fn value(&mut self) -> R<Val<'a>> {
        let ctor = self.u8()?;
        if ctor != 0 {
            return self.data(ctor);
        }
        let desc = self.descriptor()?;
        let start = self.pos;
        loop {
            match self.u8()? {
                0 => {
                    self.descriptor()?;
                }
                c => {
                    self.data(c)?;
                    break;
                }
            }
        }
        Ok(Val::Described(desc, &self.b[start..self.pos]))
    }

    /// A descriptor: a ulong code or a symbol (`data(0)` is malformed, so no recursion).
    fn descriptor(&mut self) -> R<Desc<'a>> {
        let ctor = self.u8()?;
        Ok(match self.data(ctor)? {
            Val::Ulong(c) => Desc::Code(c),
            Val::Sym(s) => Desc::Sym(s),
            _ => Desc::Code(u64::MAX),
        })
    }

    /// The data of a value whose constructor is `ctor` (array elements share one).
    pub fn data(&mut self, ctor: u8) -> R<Val<'a>> {
        Ok(match ctor {
            0x40 => Val::Null,
            0x41 => Val::Bool(true),
            0x42 => Val::Bool(false),
            0x56 => Val::Bool(self.u8()? != 0),
            0x50 => Val::Ubyte(self.u8()?),
            0x51 => Val::Byte(self.u8()? as i8),
            0x60 => Val::Ushort(u16::from_be_bytes(self.be()?)),
            0x61 => Val::Short(i16::from_be_bytes(self.be()?)),
            0x70 => Val::Uint(self.u32()?),
            0x52 => Val::Uint(self.u8()? as u32),
            0x43 => Val::Uint(0),
            0x71 => Val::Int(i32::from_be_bytes(self.be()?)),
            0x54 => Val::Int(self.u8()? as i8 as i32),
            0x80 => Val::Ulong(u64::from_be_bytes(self.be()?)),
            0x53 => Val::Ulong(self.u8()? as u64),
            0x44 => Val::Ulong(0),
            0x81 => Val::Long(i64::from_be_bytes(self.be()?)),
            0x55 => Val::Long(self.u8()? as i8 as i64),
            0x72 => Val::Float(f32::from_be_bytes(self.be()?)),
            0x82 => Val::Double(f64::from_be_bytes(self.be()?)),
            0x74 => self.take(4).map(|_| Val::Decimal)?,
            0x84 => self.take(8).map(|_| Val::Decimal)?,
            0x94 => self.take(16).map(|_| Val::Decimal)?,
            0x73 => self.take(4).map(|_| Val::Char)?,
            0x83 => Val::Timestamp(i64::from_be_bytes(self.be()?)),
            0x98 => Val::Uuid(self.take(16)?),
            0xa0 => {
                let n = self.u8()? as usize;
                Val::Binary(self.take(n)?)
            }
            0xb0 => {
                let n = self.u32()? as usize;
                Val::Binary(self.take(n)?)
            }
            0xa1 => {
                let n = self.u8()? as usize;
                Val::Str(self.take(n)?)
            }
            0xb1 => {
                let n = self.u32()? as usize;
                Val::Str(self.take(n)?)
            }
            0xa3 => {
                let n = self.u8()? as usize;
                Val::Sym(self.take(n)?)
            }
            0xb3 => {
                let n = self.u32()? as usize;
                Val::Sym(self.take(n)?)
            }
            0x45 => Val::List(Seq { count: 0, body: &[] }),
            0xc0 | 0xc1 => {
                let n = self.u8()? as usize;
                let mut c = Cur::new(self.take(n)?);
                let count = c.u8()? as u32;
                let s = Seq { count, body: &c.b[c.pos..] };
                if ctor == 0xc0 { Val::List(s) } else { Val::Map(s) }
            }
            0xd0 | 0xd1 => {
                let n = self.u32()? as usize;
                let mut c = Cur::new(self.take(n)?);
                let count = c.u32()?;
                let s = Seq { count, body: &c.b[c.pos..] };
                if ctor == 0xd0 { Val::List(s) } else { Val::Map(s) }
            }
            0xe0 | 0xf0 => {
                let n = if ctor == 0xe0 { self.u8()? as usize } else { self.u32()? as usize };
                let mut c = Cur::new(self.take(n)?);
                let count = if ctor == 0xe0 { c.u8()? as u32 } else { c.u32()? };
                let mut el = c.u8()?;
                let mut described = false;
                if el == 0 {
                    c.descriptor()?;
                    el = c.u8()?;
                    described = true;
                }
                Val::Array(Arr { count, ctor: (!described).then_some(el), body: &c.b[c.pos..] })
            }
            _ => return Err(Malformed),
        })
    }
}

impl<'a> Seq<'a> {
    pub fn iter(&self) -> SeqIter<'a> {
        SeqIter { cur: Cur::new(self.body), left: self.count }
    }

    /// Map entries.
    pub fn pairs(&self) -> PairIter<'a> {
        PairIter(self.iter())
    }
}

pub struct SeqIter<'a> {
    cur: Cur<'a>,
    left: u32,
}

impl<'a> Iterator for SeqIter<'a> {
    type Item = R<Val<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let v = self.cur.value();
        if v.is_err() {
            self.left = 0;
        }
        Some(v)
    }
}

pub struct PairIter<'a>(SeqIter<'a>);

impl<'a> Iterator for PairIter<'a> {
    type Item = R<(Val<'a>, Val<'a>)>;
    fn next(&mut self) -> Option<Self::Item> {
        let k = self.0.next()?;
        let v = self.0.next().unwrap_or(Err(Malformed));
        Some(k.and_then(|k| v.map(|v| (k, v))))
    }
}

impl<'a> Arr<'a> {
    fn iter(&self) -> impl Iterator<Item = R<Val<'a>>> + 'a {
        let (mut cur, ctor) = (Cur::new(self.body), self.ctor);
        (0..self.count).map_while(move |_| ctor.map(|c| cur.data(c)))
    }
}

fn section_code(d: Desc<'_>) -> u64 {
    match d {
        Desc::Code(c) => c,
        Desc::Sym(s) => match s {
            b"amqp:header:list" => HEADER,
            b"amqp:delivery-annotations:map" => DELIVERY_ANNOTATIONS,
            b"amqp:message-annotations:map" => MESSAGE_ANNOTATIONS,
            b"amqp:properties:list" => PROPERTIES,
            b"amqp:application-properties:map" => APPLICATION_PROPERTIES,
            b"amqp:data:binary" => DATA,
            b"amqp:amqp-sequence:list" => AMQP_SEQUENCE,
            b"amqp:amqp-value:*" => AMQP_VALUE,
            b"amqp:footer:map" => FOOTER,
            _ => u64::MAX,
        },
    }
}

/// The sections of an encoded message.
#[derive(Default)]
pub struct Sections<'a> {
    pub header: Option<Seq<'a>>,
    pub annotations: Option<Seq<'a>>,
    pub properties: Option<Seq<'a>>,
    pub app_properties: Option<Seq<'a>>,
    /// Data section contents: (offset, len) of the first, and how many there are.
    pub data: Option<(usize, usize)>,
    pub data_count: usize,
    /// Whether the body has amqp-value / amqp-sequence sections.
    pub non_data: bool,
    /// Byte range of the body sections (and footer).
    pub body: (usize, usize),
}

/// Split an encoded message into its sections.
pub fn sections(msg: &[u8]) -> R<Sections<'_>> {
    let mut s = Sections { body: (msg.len(), msg.len()), ..Default::default() };
    let mut c = Cur::new(msg);
    while !c.at_end() {
        let start = c.pos();
        if c.u8()? != 0 {
            return Err(Malformed);
        }
        let code = match c.value()? {
            Val::Ulong(code) => code,
            Val::Sym(sym) => section_code(Desc::Sym(sym)),
            _ => return Err(Malformed),
        };
        let vstart = c.pos();
        let v = c.value()?;
        let is_body = matches!(code, DATA | AMQP_SEQUENCE | AMQP_VALUE | FOOTER);
        if is_body {
            if s.body.0 == msg.len() {
                s.body.0 = start;
            }
            s.body.1 = c.pos();
        }
        match (code, v) {
            (HEADER, Val::List(l)) => s.header = Some(l),
            (MESSAGE_ANNOTATIONS, Val::Map(m)) => s.annotations = Some(m),
            (PROPERTIES, Val::List(l)) => s.properties = Some(l),
            (APPLICATION_PROPERTIES, Val::Map(m)) => s.app_properties = Some(m),
            (DATA, Val::Binary(b)) => {
                if s.data_count == 0 {
                    // The content follows the constructor and length.
                    let off = c.pos() - b.len();
                    debug_assert!(off >= vstart);
                    s.data = Some((off, b.len()));
                }
                s.data_count += 1;
            }
            (AMQP_SEQUENCE | AMQP_VALUE, _) => s.non_data = true,
            (DELIVERY_ANNOTATIONS | FOOTER, _) => {}
            _ => return Err(Malformed),
        }
    }
    Ok(s)
}

/// Whether `b` is a run of body sections (and an optional footer), as an AMQP 1.0 body stored
/// in a 0-9-1 message of type `amqp-1.0`.
fn is_body_sections(b: &[u8]) -> bool {
    let Ok(s) = sections(b) else { return false };
    !b.is_empty() && s.body.0 == 0 && s.header.is_none() && s.annotations.is_none() && s.properties.is_none() && s.app_properties.is_none()
}

// ── AMQP 1.0 → 0-9-1 ──

/// Where the 0-9-1 body of a converted message is.
#[derive(Debug, PartialEq, Eq)]
pub enum InBody {
    /// `len` bytes at `offset` of the message (one data section, or the encoded body sections).
    Slice { offset: usize, len: usize },
    /// Several data sections, concatenated into the caller's body buffer.
    Concat,
}

/// The 0-9-1 view of an AMQP 1.0 message: properties are written to `props`.
#[derive(Debug)]
pub struct In<'a> {
    pub exchange: &'a str,
    pub routing_key: &'a str,
    pub body: InBody,
}

fn short(b: &[u8]) -> Option<&str> {
    if b.len() <= 255 { std::str::from_utf8(b).ok() } else { None }
}

/// `utf8`/`symbol` values of at most 255 bytes (`unwrap_shortstr`).
fn shortstr<'a>(v: &Val<'a>) -> Option<&'a str> {
    match *v {
        Val::Str(s) | Val::Sym(s) => short(s),
        _ => None,
    }
}

fn unconvertible(v: &Val<'_>) -> bool {
    matches!(v, Val::Char | Val::Uuid(_) | Val::Described(..) | Val::Decimal)
}

fn is_internal_header(k: &[u8]) -> bool {
    k.starts_with(b"x-basic-") || matches!(k, b"x-routing-key" | b"x-exchange" | b"x-death")
}

/// Append a 0-9-1 field value (tag and data). False (and nothing written) if it has no 0-9-1
/// equivalent.
fn put_field(out: &mut BytesMut, v: Val<'_>, depth: usize) -> bool {
    let at = out.len();
    let ok = match v {
        Val::Null => {
            out.put_u8(b'V');
            true
        }
        Val::Bool(b) => {
            out.put_slice(&[b't', b as u8]);
            true
        }
        Val::Ubyte(x) => {
            out.put_slice(&[b'B', x]);
            true
        }
        Val::Byte(x) => {
            out.put_u8(b'b');
            out.put_i8(x);
            true
        }
        Val::Ushort(x) => {
            out.put_u8(b'u');
            out.put_u16(x);
            true
        }
        Val::Short(x) => {
            out.put_u8(b's');
            out.put_i16(x);
            true
        }
        Val::Uint(x) => {
            out.put_u8(b'i');
            out.put_u32(x);
            true
        }
        Val::Int(x) => {
            out.put_u8(b'I');
            out.put_i32(x);
            true
        }
        Val::Ulong(x) => {
            out.put_u8(b'l');
            out.put_u64(x);
            true
        }
        Val::Long(x) => {
            out.put_u8(b'l');
            out.put_i64(x);
            true
        }
        Val::Float(x) => {
            out.put_u8(b'f');
            out.put_f32(x);
            true
        }
        Val::Double(x) => {
            out.put_u8(b'd');
            out.put_f64(x);
            true
        }
        Val::Timestamp(ms) => {
            out.put_u8(b'T');
            out.put_u64((ms / 1000).max(0) as u64);
            true
        }
        Val::Str(b) | Val::Sym(b) | Val::Binary(b) => {
            out.put_u8(b'S');
            out.put_u32(b.len() as u32);
            out.put_slice(b);
            true
        }
        Val::List(_) | Val::Array(_) if depth < MAX_DEPTH => {
            out.put_u8(b'A');
            let len_at = out.len();
            out.put_u32(0);
            let mut each = |e: R<Val<'_>>| match e {
                Ok(e) if !unconvertible(&e) => put_field(out, e, depth + 1),
                Ok(_) => true,
                Err(_) => false,
            };
            let ok = match v {
                Val::List(l) => l.iter().all(&mut each),
                Val::Array(a) => a.ctor.is_some() && a.iter().all(&mut each),
                _ => false,
            };
            patch_len(out, len_at);
            ok
        }
        Val::Map(m) if depth < MAX_DEPTH => {
            out.put_u8(b'F');
            let len_at = out.len();
            out.put_u32(0);
            let ok = put_table_entries(out, m, depth + 1, |_| true);
            patch_len(out, len_at);
            ok
        }
        _ => false,
    };
    if !ok {
        out.truncate(at);
    }
    ok
}

fn patch_len(out: &mut BytesMut, at: usize) {
    let n = (out.len() - at - 4) as u32;
    out[at..at + 4].copy_from_slice(&n.to_be_bytes());
}

/// Table entries for a map whose keys are strings or symbols of at most 255 bytes; `keep`
/// filters keys. Returns false if the map is malformed.
fn put_table_entries(out: &mut BytesMut, m: Seq<'_>, depth: usize, keep: impl Fn(&[u8]) -> bool) -> bool {
    for kv in m.pairs() {
        let Ok((k, v)) = kv else { return false };
        let key = match k {
            Val::Str(k) | Val::Sym(k) if k.len() <= 255 => k,
            _ => continue,
        };
        if unconvertible(&v) || !keep(key) {
            continue;
        }
        put_entry(out, key, v, depth);
    }
    true
}

fn put_entry(out: &mut BytesMut, key: &[u8], v: Val<'_>, depth: usize) -> bool {
    let at = out.len();
    out.put_u8(key.len() as u8);
    out.put_slice(key);
    if put_field(out, v, depth) {
        true
    } else {
        out.truncate(at);
        false
    }
}

fn put_short(out: &mut BytesMut, s: &[u8]) {
    let n = s.len().min(255);
    out.put_u8(n as u8);
    out.put_slice(&s[..n]);
}

fn list_fields<'a, const N: usize>(l: Option<Seq<'a>>) -> [Val<'a>; N] {
    let mut f = [Val::Null; N];
    if let Some(l) = l {
        for (i, v) in l.iter().take(N).enumerate() {
            match v {
                Ok(v) => f[i] = v,
                Err(_) => break,
            }
        }
    }
    f
}

fn map_get<'a>(m: Option<Seq<'a>>, key: &[u8]) -> Option<Val<'a>> {
    m?.pairs().map_while(Result::ok).find(|(k, _)| matches!(k, Val::Sym(k) | Val::Str(k) if *k == key)).map(|(_, v)| v)
}

fn uuid_urn(u: &[u8]) -> String {
    let h = |r: std::ops::Range<usize>| u[r].iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!("urn:uuid:{}-{}-{}-{}-{}", h(0..4), h(4..6), h(6..8), h(8..10), h(10..16))
}

/// The 0-9-1 string for a message-id/correlation-id, or the header it overflows into.
enum Id<'a> {
    None,
    Prop(std::borrow::Cow<'a, str>),
    Header(Val<'a>),
}

fn message_id<'a>(v: Val<'a>) -> Id<'a> {
    match v {
        Val::Uuid(u) => Id::Prop(uuid_urn(u).into()),
        Val::Ulong(n) => Id::Prop(n.to_string().into()),
        Val::Binary(_) => Id::Header(v),
        Val::Str(s) => match short(s) {
            Some(s) => Id::Prop(s.into()),
            None => Id::Header(v),
        },
        _ => Id::None,
    }
}

fn url_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Convert an AMQP 1.0 message to its 0-9-1 view (RabbitMQ's `mc_amqpl:convert_from`):
/// encoded basic properties go to `props` (cleared first), several data sections are
/// concatenated into `body`. Exchange and routing key come from the `x-exchange` and
/// `x-routing-key` annotations, defaulting to `""` and `stream`.
pub fn convert_in<'a>(msg: &'a [u8], stream: &'a str, props: &mut BytesMut, body: &mut Vec<u8>) -> R<In<'a>> {
    props.clear();
    let s = sections(msg)?;
    let ma = s.annotations;
    let header: [Val<'_>; 3] = list_fields(s.header);
    let p: [Val<'_>; 13] = list_fields(s.properties);

    let exchange = map_get(ma, b"x-exchange").and_then(|v| shortstr(&v)).unwrap_or("");
    let routing_key = map_get(ma, b"x-routing-key").and_then(|v| shortstr(&v)).unwrap_or(stream);
    let (in_body, amqp_type) = if s.non_data || s.data_count == 0 {
        (InBody::Slice { offset: s.body.0, len: s.body.1 - s.body.0 }, true)
    } else if s.data_count == 1 {
        let (offset, len) = s.data.unwrap_or((0, 0));
        (InBody::Slice { offset, len }, false)
    } else {
        body.clear();
        let mut c = Cur::new(&msg[s.body.0..s.body.1]);
        while !c.at_end() {
            if let Val::Described(d, v) = c.value()?
                && section_code(d) == DATA
                && let Val::Binary(b) = Cur::new(v).value()?
            {
                body.extend_from_slice(b);
            }
        }
        (InBody::Concat, false)
    };
    let kind = if amqp_type { Some(AMQP10_TYPE) } else { map_get(ma, b"x-basic-type").and_then(|v| shortstr(&v)) };

    let flags_at = props.len();
    props.put_u16(0);
    let mut flags = 0u16;
    fn opt(flags: &mut u16, bit: u16, v: Option<&[u8]>, props: &mut BytesMut) {
        if let Some(v) = v {
            *flags |= 1 << bit;
            put_short(props, v);
        }
    }
    opt(&mut flags, 15, shortstr(&p[6]).map(str::as_bytes), props);
    opt(&mut flags, 14, shortstr(&p[7]).map(str::as_bytes), props);

    let (msg_id, corr_id) = (message_id(p[0]), message_id(p[5]));
    // Headers: x- annotations, then overflowing ids, then application properties.
    let headers_at = props.len();
    props.put_u32(0);
    if let Some(ma) = ma {
        for (k, v) in ma.pairs().map_while(Result::ok) {
            let Val::Sym(k) = k else { continue };
            match k {
                b"x-cc" if !unconvertible(&v) => {
                    put_entry(props, b"CC", v, 0);
                }
                b"x-opt-rabbitmq-received-time" => {
                    if let Val::Timestamp(ts) = v {
                        put_entry(props, b"timestamp_in_ms", Val::Long(ts), 0);
                    }
                }
                b"x-opt-deaths" => {}
                k if k.starts_with(b"x-") && k.len() <= 255 && !unconvertible(&v) && !is_internal_header(k) => {
                    put_entry(props, k, v, 0);
                }
                _ => {}
            }
        }
    }
    if let Id::Header(v) = corr_id {
        put_entry(props, b"x-correlation-id", v, 0);
    }
    if let Id::Header(v) = msg_id {
        // Binary ids keep their type (`x`), long strings become long strings.
        if let Val::Binary(b) = v {
            props.put_u8(14);
            props.put_slice(b"x-message-id");
            props.put_u8(b'x');
            props.put_u32(b.len() as u32);
            props.put_slice(b);
        } else {
            put_entry(props, b"x-message-id", v, 0);
        }
    }
    if let Some(ap) = s.app_properties {
        for (k, v) in ap.pairs().map_while(Result::ok) {
            if let Val::Str(k) = k
                && k.len() <= 255
                && !unconvertible(&v)
            {
                put_entry(props, k, v, 0);
            }
        }
    }
    if props.len() == headers_at + 4 {
        props.truncate(headers_at);
    } else {
        patch_len(props, headers_at);
        flags |= 1 << 13;
    }

    if let Val::Bool(durable) = header[0] {
        flags |= 1 << 12;
        props.put_u8(if durable { 2 } else { 1 });
    }
    if let Val::Ubyte(pr) = header[1] {
        flags |= 1 << 11;
        props.put_u8(pr);
    }
    if let Id::Prop(c) = &corr_id {
        opt(&mut flags, 10, Some(c.as_bytes()), props);
    }
    let reply_to = shortstr(&p[4]).and_then(|r| match r.strip_prefix("/queues/") {
        Some(q) => url_decode(q).map(std::borrow::Cow::Owned),
        None => Some(r.into()),
    });
    opt(&mut flags, 9, reply_to.as_deref().map(str::as_bytes), props);
    if let Val::Uint(ttl) = header[2] {
        opt(&mut flags, 8, Some(ttl.to_string().as_bytes()), props);
    }
    if let Id::Prop(m) = &msg_id {
        opt(&mut flags, 7, Some(m.as_bytes()), props);
    }
    if let Val::Timestamp(ts) = p[9] {
        flags |= 1 << 6;
        props.put_u64((ts / 1000).max(0) as u64);
    }
    opt(&mut flags, 5, kind.map(str::as_bytes), props);
    let user_id = match p[1] {
        Val::Binary(b) | Val::Str(b) => short(b),
        _ => None,
    };
    opt(&mut flags, 4, user_id.map(str::as_bytes), props);
    opt(&mut flags, 3, shortstr(&p[10]).map(str::as_bytes), props);
    props[flags_at..flags_at + 2].copy_from_slice(&flags.to_be_bytes());
    Ok(In { exchange, routing_key, body: in_body })
}

// ── 0-9-1 → AMQP 1.0 ──

fn put_described(out: &mut BytesMut, code: u8) {
    out.put_slice(&[0x00, 0x53, code]);
}

fn put_vbytes(out: &mut BytesMut, short_code: u8, long_code: u8, b: &[u8]) {
    if b.len() < 256 {
        out.put_u8(short_code);
        out.put_u8(b.len() as u8);
    } else {
        out.put_u8(long_code);
        out.put_u32(b.len() as u32);
    }
    out.put_slice(b);
}

fn put_utf8(out: &mut BytesMut, s: &[u8]) {
    put_vbytes(out, 0xa1, 0xb1, s);
}

fn put_sym(out: &mut BytesMut, s: &[u8]) {
    put_vbytes(out, 0xa3, 0xb3, s);
}

fn put_bin(out: &mut BytesMut, b: &[u8]) {
    put_vbytes(out, 0xa0, 0xb0, b);
}

/// A list32 or map32 being written: call [`Compound::end`] with the element count.
struct Compound(usize);

impl Compound {
    fn begin(out: &mut BytesMut, code: u8) -> Self {
        out.put_u8(code);
        let at = out.len();
        out.put_u64(0);
        Compound(at)
    }

    fn end(self, out: &mut BytesMut, count: u32) {
        let size = (out.len() - self.0 - 4) as u32;
        out[self.0..self.0 + 4].copy_from_slice(&size.to_be_bytes());
        out[self.0 + 4..self.0 + 8].copy_from_slice(&count.to_be_bytes());
    }
}

fn utf8_no_nul(b: &[u8]) -> bool {
    !b.contains(&0) && std::str::from_utf8(b).is_ok()
}

/// Append a 0-9-1 field value as AMQP 1.0 (`mc_amqpl:from_091`). False (and nothing written)
/// for decimals, which have no conversion.
fn put_amqp(out: &mut BytesMut, v: &FieldValue, depth: usize) -> bool {
    let at = out.len();
    let ok = match v {
        FieldValue::Bool(b) => {
            out.put_u8(if *b { 0x41 } else { 0x42 });
            true
        }
        FieldValue::I8(x) => {
            out.put_u8(0x51);
            out.put_i8(*x);
            true
        }
        FieldValue::U8(x) => {
            out.put_slice(&[0x50, *x]);
            true
        }
        FieldValue::I16(x) => {
            out.put_u8(0x61);
            out.put_i16(*x);
            true
        }
        FieldValue::U16(x) => {
            out.put_u8(0x60);
            out.put_u16(*x);
            true
        }
        FieldValue::I32(x) => {
            out.put_u8(0x71);
            out.put_i32(*x);
            true
        }
        FieldValue::U32(x) => {
            out.put_u8(0x70);
            out.put_u32(*x);
            true
        }
        FieldValue::I64(x) => {
            out.put_u8(0x81);
            out.put_i64(*x);
            true
        }
        FieldValue::F32(x) => {
            out.put_u8(0x72);
            out.put_f32(*x);
            true
        }
        FieldValue::F64(x) => {
            out.put_u8(0x82);
            out.put_f64(*x);
            true
        }
        FieldValue::Timestamp(s) => {
            out.put_u8(0x83);
            out.put_i64((*s as i64).saturating_mul(1000));
            true
        }
        FieldValue::ShortStr(s) => {
            put_utf8(out, s.as_bytes());
            true
        }
        FieldValue::LongStr(b) => {
            if utf8_no_nul(b) { put_utf8(out, b) } else { put_bin(out, b) }
            true
        }
        FieldValue::Bytes(b) => {
            put_bin(out, b);
            true
        }
        FieldValue::Void => {
            out.put_u8(0x40);
            true
        }
        FieldValue::Array(a) if depth < MAX_DEPTH => {
            let c = Compound::begin(out, 0xd0);
            let n = a.iter().filter(|e| put_amqp(out, e, depth + 1)).count();
            c.end(out, n as u32);
            true
        }
        FieldValue::Table(t) if depth < MAX_DEPTH => {
            let c = Compound::begin(out, 0xd1);
            let mut n = 0;
            for (k, v) in t.iter() {
                let kat = out.len();
                put_sym(out, k.as_bytes());
                if put_amqp(out, v, depth + 1) {
                    n += 2;
                } else {
                    out.truncate(kat);
                }
            }
            c.end(out, n);
            true
        }
        _ => false,
    };
    if !ok {
        out.truncate(at);
    }
    ok
}

fn urn_to_uuid(s: &str) -> Option<[u8; 16]> {
    let hex = s.strip_prefix("urn:uuid:")?;
    if hex.len() != 36 || [8, 13, 18, 23].iter().any(|&i| hex.as_bytes()[i] != b'-') {
        return None;
    }
    let digits: Vec<u8> = hex.bytes().filter(|&b| b != b'-').collect();
    let mut out = [0u8; 16];
    for (i, pair) in digits.chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

fn put_id(out: &mut BytesMut, id: &str) {
    match urn_to_uuid(id) {
        Some(u) => {
            out.put_u8(0x98);
            out.put_slice(&u);
        }
        None => put_utf8(out, id.as_bytes()),
    }
}

/// `uri_string:quote`: percent-encode everything but unreserved characters.
fn quote(s: &str, out: &mut Vec<u8>) {
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b);
        } else {
            out.extend_from_slice(format!("%{b:02X}").as_bytes());
        }
    }
}

/// Encode a 0-9-1 message as AMQP 1.0 (`mc_amqpl:convert_to(mc_amqp)` plus the `x-exchange`
/// and `x-routing-key` annotations RabbitMQ stores with stream messages).
pub fn encode_message(m: &Message, out: &mut BytesMut) {
    let p = &m.props;
    let empty = FieldTable::new();
    let headers = p.headers.as_ref().unwrap_or(&empty);

    put_described(out, 0x70);
    let c = Compound::begin(out, 0xd0);
    out.put_u8(if p.delivery_mode == Some(2) { 0x41 } else { 0x42 });
    let ttl = p.expiration.as_deref().and_then(|e| e.parse::<u32>().ok());
    let n = match (p.priority, ttl) {
        (None, None) => 1,
        (pr, ttl) => {
            match pr {
                Some(pr) => out.put_slice(&[0x50, pr]),
                None => out.put_u8(0x40),
            }
            match ttl {
                Some(t) => {
                    out.put_u8(0x70);
                    out.put_u32(t);
                    3
                }
                None => 2,
            }
        }
    };
    c.end(out, n);

    put_described(out, 0x72);
    let c = Compound::begin(out, 0xd1);
    let mut n = 0;
    let mut pair = |out: &mut BytesMut, k: &[u8], v: &FieldValue| {
        let at = out.len();
        put_sym(out, k);
        if put_amqp(out, v, 0) {
            n += 2;
        } else {
            out.truncate(at);
        }
    };
    for (k, v) in headers.iter() {
        if k.starts_with("x-") && k != "x-exchange" && k != "x-routing-key" {
            pair(out, k.as_bytes(), v);
        } else if k == "CC" && matches!(v, FieldValue::Array(_)) {
            pair(out, b"x-cc", v);
        }
    }
    if let Some(t) = &p.kind {
        put_sym(out, b"x-basic-type");
        put_utf8(out, t.as_bytes());
        n += 2;
    }
    put_sym(out, b"x-exchange");
    put_utf8(out, m.exchange.as_bytes());
    put_sym(out, b"x-routing-key");
    put_utf8(out, m.routing_key.as_bytes());
    c.end(out, n + 4);

    let has_props = p.message_id.is_some()
        || p.user_id.is_some()
        || p.reply_to.is_some()
        || p.correlation_id.is_some()
        || p.content_type.is_some()
        || p.content_encoding.is_some()
        || p.timestamp.is_some()
        || p.app_id.is_some();
    if has_props {
        put_described(out, 0x73);
        let c = Compound::begin(out, 0xd0);
        let (mut used, mut used_len) = (0u32, out.len());
        for i in 0..11u32 {
            let wrote = match i {
                0 => p.message_id.as_deref().map(|id| put_id(out, id)).is_some(),
                1 => p.user_id.as_deref().map(|u| put_bin(out, u.as_bytes())).is_some(),
                4 => p
                    .reply_to
                    .as_deref()
                    .map(|r| {
                        let mut q = b"/queues/".to_vec();
                        quote(r, &mut q);
                        put_utf8(out, &q);
                    })
                    .is_some(),
                5 => p.correlation_id.as_deref().map(|id| put_id(out, id)).is_some(),
                6 => p.content_type.as_deref().map(|t| put_sym(out, t.as_bytes())).is_some(),
                7 => p.content_encoding.as_deref().map(|t| put_sym(out, t.as_bytes())).is_some(),
                9 => p
                    .timestamp
                    .map(|t| {
                        out.put_u8(0x83);
                        out.put_i64((t as i64).saturating_mul(1000));
                    })
                    .is_some(),
                10 => p.app_id.as_deref().map(|a| put_utf8(out, a.as_bytes())).is_some(),
                _ => false,
            };
            if wrote {
                used = i + 1;
                used_len = out.len();
            } else {
                out.put_u8(0x40);
            }
        }
        out.truncate(used_len);
        c.end(out, used);
    }

    let sect = out.len();
    put_described(out, 0x74);
    let c = Compound::begin(out, 0xd1);
    let mut n = 0;
    for (k, v) in headers.iter() {
        if k.starts_with("x-") || matches!(v, FieldValue::Array(_) | FieldValue::Table(_)) {
            continue;
        }
        let at = out.len();
        put_utf8(out, k.as_bytes());
        if put_amqp(out, v, 0) {
            n += 2;
        } else {
            out.truncate(at);
        }
    }
    if n == 0 {
        out.truncate(sect);
    } else {
        c.end(out, n);
    }

    if p.kind.as_deref() == Some(AMQP10_TYPE) && is_body_sections(&m.body) {
        out.put_slice(&m.body);
    } else {
        put_described(out, 0x75);
        put_bin(out, &m.body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use mq_core::message::Properties;

    fn view(msg: &[u8]) -> (Properties, Vec<u8>, String, String) {
        let (mut props, mut body) = (BytesMut::new(), Vec::new());
        let v = convert_in(msg, "s1", &mut props, &mut body).unwrap();
        let p = Properties::decode(&mut props.freeze()).unwrap();
        let b = match v.body {
            InBody::Slice { offset, len } => msg[offset..offset + len].to_vec(),
            InBody::Concat => body,
        };
        (p, b, v.exchange.to_string(), v.routing_key.to_string())
    }

    #[test]
    fn data_only() {
        // data section "hi"
        let msg = [0x00, 0x53, 0x75, 0xa0, 0x02, b'h', b'i'];
        let (p, b, ex, rk) = view(&msg);
        assert_eq!((p, b, ex.as_str(), rk.as_str()), (Properties::default(), b"hi".to_vec(), "", "s1"));
    }

    #[test]
    fn roundtrip_091() {
        let mut headers = FieldTable::new();
        headers.insert("app", "v1");
        headers.insert("n", FieldValue::I64(-5));
        headers.insert("x-custom", FieldValue::I32(7));
        headers.insert("CC", FieldValue::Array(vec![FieldValue::str("rk2")]));
        headers.insert("bin", FieldValue::LongStr(Bytes::from_static(b"\x00\x01")));
        let props = Properties {
            content_type: Some("text/plain".into()),
            content_encoding: Some("gzip".into()),
            headers: Some(headers),
            delivery_mode: Some(2),
            priority: Some(3),
            correlation_id: Some("urn:uuid:6ba7b810-9dad-11d1-80b4-00c04fd430c8".into()),
            reply_to: Some("amq.rabbitmq.reply-to/x y".into()),
            expiration: Some("60000".into()),
            message_id: Some("m1".into()),
            timestamp: Some(1_700_000_000),
            kind: Some("order".into()),
            user_id: Some("guest".into()),
            app_id: Some("app1".into()),
            cluster_id: None,
        };
        let m = Message::new("ex", "rk", props, Bytes::from_static(b"payload"));
        let mut out = BytesMut::new();
        encode_message(&m, &mut out);
        let (p, b, ex, rk) = view(&out);
        assert_eq!((b.as_slice(), ex.as_str(), rk.as_str()), (&b"payload"[..], "ex", "rk"));
        assert_eq!(p.content_type.as_deref(), Some("text/plain"));
        assert_eq!(p.content_encoding.as_deref(), Some("gzip"));
        assert_eq!((p.delivery_mode, p.priority), (Some(2), Some(3)));
        assert_eq!(p.correlation_id.as_deref(), Some("urn:uuid:6ba7b810-9dad-11d1-80b4-00c04fd430c8"));
        assert_eq!(p.reply_to.as_deref(), Some("amq.rabbitmq.reply-to/x y"));
        assert_eq!(p.expiration.as_deref(), Some("60000"));
        assert_eq!(p.message_id.as_deref(), Some("m1"));
        assert_eq!(p.timestamp, Some(1_700_000_000));
        assert_eq!(p.kind.as_deref(), Some("order"));
        assert_eq!(p.user_id.as_deref(), Some("guest"));
        assert_eq!(p.app_id.as_deref(), Some("app1"));
        let h = p.headers.unwrap();
        assert_eq!(h.get("app").and_then(|v| v.as_str()), Some("v1"));
        assert_eq!(h.get("n").and_then(|v| v.as_i64()), Some(-5));
        assert_eq!(h.get("x-custom").and_then(|v| v.as_i64()), Some(7));
        assert_eq!(h.get("CC").and_then(|v| v.as_array()).map(|a| a.len()), Some(1));
        assert_eq!(h.get("bin").and_then(|v| v.as_str()), Some("\u{0}\u{1}"));
        assert!(h.get("x-exchange").is_none() && h.get("x-basic-type").is_none());
    }

    #[test]
    fn amqp_value_body_and_ids() {
        let mut msg = BytesMut::new();
        // properties: message-id ulong 42, correlation-id binary
        put_described(&mut msg, 0x73);
        let c = Compound::begin(&mut msg, 0xd0);
        msg.put_slice(&[0x80, 0, 0, 0, 0, 0, 0, 0, 42]);
        msg.put_slice(&[0x40, 0x40, 0x40, 0x40]);
        put_bin(&mut msg, b"\x01\x02");
        c.end(&mut msg, 6);
        // amqp-value "hello"
        let body_at = msg.len();
        put_described(&mut msg, 0x77);
        put_utf8(&mut msg, b"hello");
        let (p, b, _, _) = view(&msg);
        assert_eq!(p.message_id.as_deref(), Some("42"));
        assert!(p.correlation_id.is_none());
        assert_eq!(p.kind.as_deref(), Some(AMQP10_TYPE));
        assert_eq!(b, msg[body_at..].to_vec());
        let h = p.headers.unwrap();
        assert!(matches!(h.get("x-correlation-id"), Some(FieldValue::LongStr(_))));
        // Back to AMQP 1.0: the encoded body sections are restored, not wrapped in data.
        let m = Message::new("", "s1", Properties { kind: Some(AMQP10_TYPE.into()), ..Default::default() }, Bytes::from(b));
        let mut out = BytesMut::new();
        encode_message(&m, &mut out);
        assert!(out.ends_with(&msg[body_at..]));
    }

    #[test]
    fn malformed_is_an_error() {
        let (mut p, mut b) = (BytesMut::new(), Vec::new());
        assert!(convert_in(&[0x00, 0x53], "s", &mut p, &mut b).is_err());
        assert!(convert_in(&[0x00, 0x53, 0x75, 0xb0, 0xff, 0xff, 0xff, 0xff], "s", &mut p, &mut b).is_err());
        // Deep nesting never recurses unboundedly: chained descriptors, nested lists.
        let mut chain = vec![0x00, 0x53, 0x75];
        for _ in 0..100_000 {
            chain.extend_from_slice(&[0x00, 0x53, 0x01]);
        }
        chain.push(0x40);
        assert!(convert_in(&chain, "s", &mut p, &mut b).is_err());
        let mut inner = vec![0x45];
        for _ in 0..200 {
            let mut l = vec![0xd0];
            l.extend_from_slice(&(inner.len() as u32 + 4).to_be_bytes());
            l.extend_from_slice(&1u32.to_be_bytes());
            l.extend_from_slice(&inner);
            inner = l;
        }
        let mut deep = vec![0x00, 0x53, 0x74, 0xd1];
        deep.extend_from_slice(&(inner.len() as u32 + 7).to_be_bytes());
        deep.extend_from_slice(&2u32.to_be_bytes());
        deep.extend_from_slice(&[0xa1, 1, b'k']);
        deep.extend_from_slice(&inner);
        let (p2, _, _, _) = view(&deep);
        assert!(p2.headers.is_none_or(|h| h.get("k").is_none()));
    }
}
