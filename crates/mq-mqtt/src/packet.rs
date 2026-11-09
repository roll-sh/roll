//! MQTT 3.1, 3.1.1 and 5.0 packet codec. Decoding never panics: malformed input becomes a
//! [`DecodeError`] the connection turns into a reason code (v5) or a close (v3).

use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const V31: u8 = 3;
pub const V311: u8 = 4;
pub const V5: u8 = 5;

/// v5 reason codes (and the v3 CONNACK return codes they map to).
pub mod rc {
    pub const SUCCESS: u8 = 0x00;
    pub const GRANTED_QOS1: u8 = 0x01;
    pub const DISCONNECT_WITH_WILL: u8 = 0x04;
    pub const NO_MATCHING_SUBSCRIBERS: u8 = 0x10;
    pub const NO_SUBSCRIPTION_EXISTED: u8 = 0x11;
    pub const UNSPECIFIED: u8 = 0x80;
    pub const MALFORMED: u8 = 0x81;
    pub const PROTOCOL_ERROR: u8 = 0x82;
    pub const IMPLEMENTATION_SPECIFIC: u8 = 0x83;
    pub const UNSUPPORTED_PROTOCOL_VERSION: u8 = 0x84;
    pub const CLIENT_ID_NOT_VALID: u8 = 0x85;
    pub const BAD_USERNAME_OR_PASSWORD: u8 = 0x86;
    pub const NOT_AUTHORIZED: u8 = 0x87;
    pub const SERVER_UNAVAILABLE: u8 = 0x88;
    pub const SERVER_SHUTTING_DOWN: u8 = 0x8B;
    pub const BAD_AUTHENTICATION_METHOD: u8 = 0x8C;
    pub const KEEP_ALIVE_TIMEOUT: u8 = 0x8D;
    pub const SESSION_TAKEN_OVER: u8 = 0x8E;
    pub const TOPIC_FILTER_INVALID: u8 = 0x8F;
    pub const TOPIC_NAME_INVALID: u8 = 0x90;
    pub const TOPIC_ALIAS_INVALID: u8 = 0x94;
    pub const PACKET_TOO_LARGE: u8 = 0x95;
    pub const QUOTA_EXCEEDED: u8 = 0x97;
    pub const QOS_NOT_SUPPORTED: u8 = 0x9B;
    pub const SHARED_SUBSCRIPTIONS_NOT_SUPPORTED: u8 = 0x9E;

    /// v3/v4 CONNACK return code for a v5 connect reason code.
    pub fn connack_v3(code: u8) -> u8 {
        match code {
            SUCCESS => 0,
            UNSUPPORTED_PROTOCOL_VERSION => 1,
            CLIENT_ID_NOT_VALID => 2,
            BAD_USERNAME_OR_PASSWORD => 4,
            NOT_AUTHORIZED | QUOTA_EXCEEDED => 5,
            _ => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Malformed(&'static str),
    Protocol(&'static str),
    /// Remaining length over the allowed maximum.
    TooLarge(usize),
    UnsupportedVersion(u8),
}

impl DecodeError {
    pub fn reason_code(&self) -> u8 {
        match self {
            DecodeError::Malformed(_) => rc::MALFORMED,
            DecodeError::Protocol(_) => rc::PROTOCOL_ERROR,
            DecodeError::TooLarge(_) => rc::PACKET_TOO_LARGE,
            DecodeError::UnsupportedVersion(_) => rc::UNSUPPORTED_PROTOCOL_VERSION,
        }
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Malformed(m) => write!(f, "malformed packet: {m}"),
            DecodeError::Protocol(m) => write!(f, "protocol error: {m}"),
            DecodeError::TooLarge(n) => write!(f, "packet of {n} bytes exceeds the maximum packet size"),
            DecodeError::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
        }
    }
}

impl std::error::Error for DecodeError {}

type Result<T> = std::result::Result<T, DecodeError>;

/// v5 properties. Only the ones present are encoded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Props {
    pub payload_format: Option<u8>,
    pub message_expiry: Option<u32>,
    pub content_type: Option<String>,
    pub response_topic: Option<String>,
    pub correlation_data: Option<Bytes>,
    pub subscription_ids: Vec<u32>,
    pub session_expiry: Option<u32>,
    pub assigned_client_id: Option<String>,
    pub server_keep_alive: Option<u16>,
    pub auth_method: Option<String>,
    pub auth_data: Option<Bytes>,
    pub request_problem_info: Option<u8>,
    pub will_delay: Option<u32>,
    pub request_response_info: Option<u8>,
    pub response_info: Option<String>,
    pub server_reference: Option<String>,
    pub reason_string: Option<String>,
    pub receive_maximum: Option<u16>,
    pub topic_alias_maximum: Option<u16>,
    pub topic_alias: Option<u16>,
    pub maximum_qos: Option<u8>,
    pub retain_available: Option<u8>,
    pub user: Vec<(String, String)>,
    pub maximum_packet_size: Option<u32>,
    pub wildcard_sub_available: Option<u8>,
    pub sub_id_available: Option<u8>,
    pub shared_sub_available: Option<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Will {
    pub topic: String,
    pub payload: Bytes,
    pub qos: u8,
    pub retain: bool,
    pub props: Props,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Connect {
    pub version: u8,
    pub clean_start: bool,
    pub keep_alive: u16,
    pub props: Props,
    pub client_id: String,
    pub will: Option<Will>,
    pub username: Option<String>,
    pub password: Option<Bytes>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Publish {
    pub dup: bool,
    pub qos: u8,
    pub retain: bool,
    pub topic: String,
    pub packet_id: u16,
    pub props: Props,
    pub payload: Bytes,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubOpts {
    pub qos: u8,
    pub no_local: bool,
    pub retain_as_published: bool,
    pub retain_handling: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Packet {
    Connect(Box<Connect>),
    ConnAck { session_present: bool, code: u8, props: Props },
    Publish(Publish),
    PubAck { id: u16, code: u8, props: Props },
    PubRec { id: u16, code: u8, props: Props },
    PubRel { id: u16, code: u8, props: Props },
    PubComp { id: u16, code: u8, props: Props },
    Subscribe { id: u16, props: Props, filters: Vec<(String, SubOpts)> },
    SubAck { id: u16, props: Props, codes: Vec<u8> },
    Unsubscribe { id: u16, props: Props, filters: Vec<String> },
    UnsubAck { id: u16, props: Props, codes: Vec<u8> },
    PingReq,
    PingResp,
    Disconnect { code: u8, props: Props },
    Auth { code: u8, props: Props },
}

// ── primitives ──

fn need(b: &Bytes, n: usize) -> Result<()> {
    if b.remaining() < n { Err(DecodeError::Malformed("truncated packet")) } else { Ok(()) }
}

fn get_u8(b: &mut Bytes) -> Result<u8> {
    need(b, 1)?;
    Ok(b.get_u8())
}

fn get_u16(b: &mut Bytes) -> Result<u16> {
    need(b, 2)?;
    Ok(b.get_u16())
}

fn get_u32(b: &mut Bytes) -> Result<u32> {
    need(b, 4)?;
    Ok(b.get_u32())
}

fn get_bin(b: &mut Bytes) -> Result<Bytes> {
    let n = get_u16(b)? as usize;
    need(b, n)?;
    Ok(b.split_to(n))
}

fn get_str(b: &mut Bytes) -> Result<String> {
    let raw = get_bin(b)?;
    let s = String::from_utf8(raw.to_vec()).map_err(|_| DecodeError::Malformed("invalid UTF-8 string"))?;
    if s.contains('\0') {
        return Err(DecodeError::Malformed("string contains U+0000"));
    }
    Ok(s)
}

fn get_varint(b: &mut Bytes) -> Result<u32> {
    let mut v: u32 = 0;
    for i in 0..4 {
        let byte = get_u8(b)?;
        v |= ((byte & 0x7f) as u32) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(DecodeError::Malformed("variable byte integer too long"))
}

fn put_varint(out: &mut BytesMut, mut v: u32) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v > 0 {
            byte |= 0x80;
        }
        out.put_u8(byte);
        if v == 0 {
            break;
        }
    }
}

fn varint_len(v: u32) -> usize {
    match v {
        0..=127 => 1,
        128..=16_383 => 2,
        16_384..=2_097_151 => 3,
        _ => 4,
    }
}

fn put_bin(out: &mut BytesMut, b: &[u8]) {
    let n = b.len().min(u16::MAX as usize);
    out.put_u16(n as u16);
    out.put_slice(&b[..n]);
}

// ── properties ──

impl Props {
    pub fn is_empty(&self) -> bool {
        *self == Props::default()
    }

    fn decode(b: &mut Bytes) -> Result<Props> {
        let len = get_varint(b)? as usize;
        need(b, len)?;
        let mut p = b.split_to(len);
        let mut out = Props::default();
        while p.has_remaining() {
            let id = get_varint(&mut p)?;
            match id {
                0x01 => out.payload_format = Some(get_u8(&mut p)?),
                0x02 => out.message_expiry = Some(get_u32(&mut p)?),
                0x03 => out.content_type = Some(get_str(&mut p)?),
                0x08 => out.response_topic = Some(get_str(&mut p)?),
                0x09 => out.correlation_data = Some(get_bin(&mut p)?),
                0x0B => {
                    let v = get_varint(&mut p)?;
                    if v == 0 {
                        return Err(DecodeError::Protocol("subscription identifier 0"));
                    }
                    out.subscription_ids.push(v);
                }
                0x11 => out.session_expiry = Some(get_u32(&mut p)?),
                0x12 => out.assigned_client_id = Some(get_str(&mut p)?),
                0x13 => out.server_keep_alive = Some(get_u16(&mut p)?),
                0x15 => out.auth_method = Some(get_str(&mut p)?),
                0x16 => out.auth_data = Some(get_bin(&mut p)?),
                0x17 => out.request_problem_info = Some(get_u8(&mut p)?),
                0x18 => out.will_delay = Some(get_u32(&mut p)?),
                0x19 => out.request_response_info = Some(get_u8(&mut p)?),
                0x1A => out.response_info = Some(get_str(&mut p)?),
                0x1C => out.server_reference = Some(get_str(&mut p)?),
                0x1F => out.reason_string = Some(get_str(&mut p)?),
                0x21 => out.receive_maximum = Some(get_u16(&mut p)?),
                0x22 => out.topic_alias_maximum = Some(get_u16(&mut p)?),
                0x23 => out.topic_alias = Some(get_u16(&mut p)?),
                0x24 => out.maximum_qos = Some(get_u8(&mut p)?),
                0x25 => out.retain_available = Some(get_u8(&mut p)?),
                0x26 => {
                    let k = get_str(&mut p)?;
                    let v = get_str(&mut p)?;
                    out.user.push((k, v));
                }
                0x27 => out.maximum_packet_size = Some(get_u32(&mut p)?),
                0x28 => out.wildcard_sub_available = Some(get_u8(&mut p)?),
                0x29 => out.sub_id_available = Some(get_u8(&mut p)?),
                0x2A => out.shared_sub_available = Some(get_u8(&mut p)?),
                _ => return Err(DecodeError::Malformed("unknown property")),
            }
        }
        Ok(out)
    }

    fn encode(&self, out: &mut BytesMut) {
        let mut p = BytesMut::new();
        let byte = |p: &mut BytesMut, id: u8, v: Option<u8>| {
            if let Some(v) = v {
                p.put_u8(id);
                p.put_u8(v);
            }
        };
        let u16p = |p: &mut BytesMut, id: u8, v: Option<u16>| {
            if let Some(v) = v {
                p.put_u8(id);
                p.put_u16(v);
            }
        };
        let u32p = |p: &mut BytesMut, id: u8, v: Option<u32>| {
            if let Some(v) = v {
                p.put_u8(id);
                p.put_u32(v);
            }
        };
        let strp = |p: &mut BytesMut, id: u8, v: &Option<String>| {
            if let Some(v) = v {
                p.put_u8(id);
                put_bin(p, v.as_bytes());
            }
        };
        let binp = |p: &mut BytesMut, id: u8, v: &Option<Bytes>| {
            if let Some(v) = v {
                p.put_u8(id);
                put_bin(p, v);
            }
        };
        byte(&mut p, 0x01, self.payload_format);
        u32p(&mut p, 0x02, self.message_expiry);
        strp(&mut p, 0x03, &self.content_type);
        strp(&mut p, 0x08, &self.response_topic);
        binp(&mut p, 0x09, &self.correlation_data);
        for id in &self.subscription_ids {
            p.put_u8(0x0B);
            put_varint(&mut p, *id);
        }
        u32p(&mut p, 0x11, self.session_expiry);
        strp(&mut p, 0x12, &self.assigned_client_id);
        u16p(&mut p, 0x13, self.server_keep_alive);
        strp(&mut p, 0x15, &self.auth_method);
        binp(&mut p, 0x16, &self.auth_data);
        byte(&mut p, 0x17, self.request_problem_info);
        u32p(&mut p, 0x18, self.will_delay);
        byte(&mut p, 0x19, self.request_response_info);
        strp(&mut p, 0x1A, &self.response_info);
        strp(&mut p, 0x1C, &self.server_reference);
        strp(&mut p, 0x1F, &self.reason_string);
        u16p(&mut p, 0x21, self.receive_maximum);
        u16p(&mut p, 0x22, self.topic_alias_maximum);
        u16p(&mut p, 0x23, self.topic_alias);
        byte(&mut p, 0x24, self.maximum_qos);
        byte(&mut p, 0x25, self.retain_available);
        for (k, v) in &self.user {
            p.put_u8(0x26);
            put_bin(&mut p, k.as_bytes());
            put_bin(&mut p, v.as_bytes());
        }
        u32p(&mut p, 0x27, self.maximum_packet_size);
        byte(&mut p, 0x28, self.wildcard_sub_available);
        byte(&mut p, 0x29, self.sub_id_available);
        byte(&mut p, 0x2A, self.shared_sub_available);
        put_varint(out, p.len() as u32);
        out.put_slice(&p);
    }
}

// ── framing ──

/// Try to take one packet off the front of `buf`. `Ok(None)` means more bytes are needed;
/// packets whose remaining length exceeds `max` fail before they are buffered.
pub fn decode(buf: &mut BytesMut, version: u8, max: u32) -> Result<Option<Packet>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let mut len: usize = 0;
    let mut i = 1;
    loop {
        if i > 4 {
            return Err(DecodeError::Malformed("remaining length too long"));
        }
        let Some(&byte) = buf.get(i) else { return Ok(None) };
        len |= ((byte & 0x7f) as usize) << (7 * (i - 1));
        i += 1;
        if byte & 0x80 == 0 {
            break;
        }
    }
    if len > max as usize {
        return Err(DecodeError::TooLarge(len));
    }
    if buf.len() < i + len {
        buf.reserve(i + len - buf.len());
        return Ok(None);
    }
    let first = buf[0];
    let mut frame = buf.split_to(i + len).freeze();
    frame.advance(i);
    parse(first, frame, version).map(Some)
}

fn parse(first: u8, mut b: Bytes, version: u8) -> Result<Packet> {
    let kind = first >> 4;
    let flags = first & 0x0f;
    let v5 = version == V5;
    let fixed_flags = |want: u8| if flags == want { Ok(()) } else { Err(DecodeError::Malformed("invalid fixed header flags")) };
    let ack = |b: &mut Bytes| -> Result<(u16, u8, Props)> {
        let id = get_u16(b)?;
        let code = if v5 && b.has_remaining() { get_u8(b)? } else { rc::SUCCESS };
        let props = if v5 && b.has_remaining() { Props::decode(b)? } else { Props::default() };
        Ok((id, code, props))
    };
    let p = match kind {
        1 => {
            fixed_flags(0)?;
            Packet::Connect(Box::new(parse_connect(&mut b)?))
        }
        2 => {
            let ack_flags = get_u8(&mut b)?;
            let code = get_u8(&mut b)?;
            let props = if v5 && b.has_remaining() { Props::decode(&mut b)? } else { Props::default() };
            Packet::ConnAck { session_present: ack_flags & 1 == 1, code, props }
        }
        3 => {
            let qos = (flags >> 1) & 3;
            if qos == 3 {
                return Err(DecodeError::Malformed("QoS 3"));
            }
            let topic = get_str(&mut b)?;
            let packet_id = if qos > 0 { get_u16(&mut b)? } else { 0 };
            if qos > 0 && packet_id == 0 {
                return Err(DecodeError::Protocol("packet identifier 0"));
            }
            let props = if v5 { Props::decode(&mut b)? } else { Props::default() };
            Packet::Publish(Publish { dup: flags & 8 != 0, qos, retain: flags & 1 != 0, topic, packet_id, props, payload: b })
        }
        4 => {
            let (id, code, props) = ack(&mut b)?;
            Packet::PubAck { id, code, props }
        }
        5 => {
            let (id, code, props) = ack(&mut b)?;
            Packet::PubRec { id, code, props }
        }
        6 => {
            fixed_flags(2)?;
            let (id, code, props) = ack(&mut b)?;
            Packet::PubRel { id, code, props }
        }
        7 => {
            let (id, code, props) = ack(&mut b)?;
            Packet::PubComp { id, code, props }
        }
        8 => {
            fixed_flags(2)?;
            let id = get_u16(&mut b)?;
            let props = if v5 { Props::decode(&mut b)? } else { Props::default() };
            let mut filters = Vec::new();
            while b.has_remaining() {
                let f = get_str(&mut b)?;
                let o = get_u8(&mut b)?;
                let reserved = if v5 { o & 0xC0 } else { o & 0xFC };
                if reserved != 0 || o & 3 == 3 || (o >> 4) & 3 == 3 {
                    return Err(DecodeError::Malformed("invalid subscription options"));
                }
                let opts = SubOpts {
                    qos: o & 3,
                    no_local: v5 && o & 4 != 0,
                    retain_as_published: v5 && o & 8 != 0,
                    retain_handling: if v5 { (o >> 4) & 3 } else { 0 },
                };
                filters.push((f, opts));
            }
            if filters.is_empty() {
                return Err(DecodeError::Protocol("SUBSCRIBE without topic filters"));
            }
            Packet::Subscribe { id, props, filters }
        }
        9 => {
            let id = get_u16(&mut b)?;
            let props = if v5 { Props::decode(&mut b)? } else { Props::default() };
            Packet::SubAck { id, props, codes: b.to_vec() }
        }
        10 => {
            fixed_flags(2)?;
            let id = get_u16(&mut b)?;
            let props = if v5 { Props::decode(&mut b)? } else { Props::default() };
            let mut filters = Vec::new();
            while b.has_remaining() {
                filters.push(get_str(&mut b)?);
            }
            if filters.is_empty() {
                return Err(DecodeError::Protocol("UNSUBSCRIBE without topic filters"));
            }
            Packet::Unsubscribe { id, props, filters }
        }
        11 => {
            let id = get_u16(&mut b)?;
            let props = if v5 { Props::decode(&mut b)? } else { Props::default() };
            Packet::UnsubAck { id, props, codes: b.to_vec() }
        }
        12 => {
            fixed_flags(0)?;
            Packet::PingReq
        }
        13 => Packet::PingResp,
        14 => {
            fixed_flags(0)?;
            let code = if v5 && b.has_remaining() { get_u8(&mut b)? } else { rc::SUCCESS };
            let props = if v5 && b.has_remaining() { Props::decode(&mut b)? } else { Props::default() };
            Packet::Disconnect { code, props }
        }
        15 if v5 => {
            let code = if b.has_remaining() { get_u8(&mut b)? } else { rc::SUCCESS };
            let props = if b.has_remaining() { Props::decode(&mut b)? } else { Props::default() };
            Packet::Auth { code, props }
        }
        _ => return Err(DecodeError::Malformed("unknown packet type")),
    };
    Ok(p)
}

fn parse_connect(b: &mut Bytes) -> Result<Connect> {
    let name = get_bin(b)?;
    let version = get_u8(b)?;
    match (&name[..], version) {
        (b"MQIsdp", V31) | (b"MQTT", V311 | V5) => {}
        (b"MQIsdp" | b"MQTT", v) => return Err(DecodeError::UnsupportedVersion(v)),
        _ => return Err(DecodeError::Protocol("invalid protocol name")),
    }
    let flags = get_u8(b)?;
    if flags & 1 != 0 && version != V31 {
        return Err(DecodeError::Malformed("reserved connect flag set"));
    }
    let keep_alive = get_u16(b)?;
    let v5 = version == V5;
    let props = if v5 { Props::decode(b)? } else { Props::default() };
    let client_id = get_str(b)?;
    let will = if flags & 0x04 != 0 {
        let will_props = if v5 { Props::decode(b)? } else { Props::default() };
        let topic = get_str(b)?;
        let payload = get_bin(b)?;
        let qos = (flags >> 3) & 3;
        if qos == 3 {
            return Err(DecodeError::Malformed("will QoS 3"));
        }
        Some(Will { topic, payload, qos, retain: flags & 0x20 != 0, props: will_props })
    } else {
        if flags & 0x38 != 0 {
            return Err(DecodeError::Malformed("will flags without a will"));
        }
        None
    };
    let username = if flags & 0x80 != 0 { Some(get_str(b)?) } else { None };
    let password = if flags & 0x40 != 0 { Some(get_bin(b)?) } else { None };
    Ok(Connect { version, clean_start: flags & 0x02 != 0, keep_alive, props, client_id, will, username, password })
}

// ── encoding ──

impl Packet {
    /// Encode for a connection speaking `version`.
    pub fn encode(&self, version: u8, out: &mut BytesMut) {
        let v5 = version == V5;
        let mut body = BytesMut::new();
        let ack = |body: &mut BytesMut, id: u16, code: u8, props: &Props| {
            body.put_u16(id);
            if v5 && (code != rc::SUCCESS || !props.is_empty()) {
                body.put_u8(code);
                if !props.is_empty() {
                    props.encode(body);
                }
            }
        };
        let first = match self {
            Packet::Connect(c) => {
                let name: &[u8] = if c.version == V31 { b"MQIsdp" } else { b"MQTT" };
                put_bin(&mut body, name);
                body.put_u8(c.version);
                let mut flags = 0u8;
                if c.clean_start {
                    flags |= 0x02;
                }
                if let Some(w) = &c.will {
                    flags |= 0x04 | (w.qos << 3) | if w.retain { 0x20 } else { 0 };
                }
                if c.password.is_some() {
                    flags |= 0x40;
                }
                if c.username.is_some() {
                    flags |= 0x80;
                }
                body.put_u8(flags);
                body.put_u16(c.keep_alive);
                if c.version == V5 {
                    c.props.encode(&mut body);
                }
                put_bin(&mut body, c.client_id.as_bytes());
                if let Some(w) = &c.will {
                    if c.version == V5 {
                        w.props.encode(&mut body);
                    }
                    put_bin(&mut body, w.topic.as_bytes());
                    put_bin(&mut body, &w.payload);
                }
                if let Some(u) = &c.username {
                    put_bin(&mut body, u.as_bytes());
                }
                if let Some(p) = &c.password {
                    put_bin(&mut body, p);
                }
                0x10
            }
            Packet::ConnAck { session_present, code, props } => {
                body.put_u8(*session_present as u8);
                body.put_u8(*code);
                if v5 {
                    props.encode(&mut body);
                }
                0x20
            }
            Packet::Publish(p) => {
                put_bin(&mut body, p.topic.as_bytes());
                if p.qos > 0 {
                    body.put_u16(p.packet_id);
                }
                if v5 {
                    p.props.encode(&mut body);
                }
                body.put_slice(&p.payload);
                0x30 | (p.dup as u8) << 3 | p.qos << 1 | p.retain as u8
            }
            Packet::PubAck { id, code, props } => {
                ack(&mut body, *id, *code, props);
                0x40
            }
            Packet::PubRec { id, code, props } => {
                ack(&mut body, *id, *code, props);
                0x50
            }
            Packet::PubRel { id, code, props } => {
                ack(&mut body, *id, *code, props);
                0x62
            }
            Packet::PubComp { id, code, props } => {
                ack(&mut body, *id, *code, props);
                0x70
            }
            Packet::Subscribe { id, props, filters } => {
                body.put_u16(*id);
                if v5 {
                    props.encode(&mut body);
                }
                for (f, o) in filters {
                    put_bin(&mut body, f.as_bytes());
                    body.put_u8(o.qos | (o.no_local as u8) << 2 | (o.retain_as_published as u8) << 3 | o.retain_handling << 4);
                }
                0x82
            }
            Packet::SubAck { id, props, codes } => {
                body.put_u16(*id);
                if v5 {
                    props.encode(&mut body);
                }
                body.put_slice(codes);
                0x90
            }
            Packet::Unsubscribe { id, props, filters } => {
                body.put_u16(*id);
                if v5 {
                    props.encode(&mut body);
                }
                for f in filters {
                    put_bin(&mut body, f.as_bytes());
                }
                0xA2
            }
            Packet::UnsubAck { id, props, codes } => {
                body.put_u16(*id);
                if v5 {
                    props.encode(&mut body);
                    body.put_slice(codes);
                }
                0xB0
            }
            Packet::PingReq => 0xC0,
            Packet::PingResp => 0xD0,
            Packet::Disconnect { code, props } => {
                // Always with a property length: some clients (rumqttc) can't decode the
                // reason-code-only short form.
                if v5 && (*code != rc::SUCCESS || !props.is_empty()) {
                    body.put_u8(*code);
                    props.encode(&mut body);
                }
                0xE0
            }
            Packet::Auth { code, props } => {
                body.put_u8(*code);
                props.encode(&mut body);
                0xF0
            }
        };
        out.reserve(1 + varint_len(body.len() as u32) + body.len());
        out.put_u8(first);
        put_varint(out, body.len() as u32);
        out.put_slice(&body);
    }

    pub fn to_bytes(&self, version: u8) -> Bytes {
        let mut b = BytesMut::new();
        self.encode(version, &mut b);
        b.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(p: Packet, version: u8) {
        let mut buf = BytesMut::new();
        p.encode(version, &mut buf);
        let whole = buf.clone();
        // Byte-by-byte arrival must give the same packet.
        let mut partial = BytesMut::new();
        for (i, byte) in whole.iter().enumerate() {
            partial.put_u8(*byte);
            let r = decode(&mut partial, version, u32::MAX).unwrap();
            if i + 1 < whole.len() {
                assert!(r.is_none());
            } else {
                assert_eq!(r.unwrap(), p);
            }
        }
        assert_eq!(decode(&mut buf, version, u32::MAX).unwrap().unwrap(), p);
        assert!(buf.is_empty());
    }

    #[test]
    fn connect_roundtrip() {
        for version in [V31, V311, V5] {
            let props = if version == V5 {
                Props { session_expiry: Some(60), receive_maximum: Some(5), user: vec![("a".into(), "b".into())], ..Default::default() }
            } else {
                Props::default()
            };
            roundtrip(
                Packet::Connect(Box::new(Connect {
                    version,
                    clean_start: true,
                    keep_alive: 30,
                    props: props.clone(),
                    client_id: "c1".into(),
                    will: Some(Will { topic: "w/t".into(), payload: Bytes::from_static(b"bye"), qos: 1, retain: true, props }),
                    username: Some("guest".into()),
                    password: Some(Bytes::from_static(b"guest")),
                })),
                version,
            );
        }
    }

    #[test]
    fn publish_and_acks_roundtrip() {
        let props = Props {
            payload_format: Some(1),
            message_expiry: Some(10),
            content_type: Some("text/plain".into()),
            response_topic: Some("r/t".into()),
            correlation_data: Some(Bytes::from_static(&[0, 1, 2])),
            subscription_ids: vec![1, 300],
            topic_alias: Some(3),
            user: vec![("k".into(), "v".into()), ("k".into(), "w".into())],
            ..Default::default()
        };
        roundtrip(
            Packet::Publish(Publish { dup: true, qos: 1, retain: true, topic: "a/b".into(), packet_id: 7, props, payload: Bytes::from_static(b"hi") }),
            V5,
        );
        roundtrip(
            Packet::Publish(Publish { dup: false, qos: 0, retain: false, topic: "a".into(), packet_id: 0, props: Props::default(), payload: Bytes::new() }),
            V311,
        );
        roundtrip(Packet::PubAck { id: 9, code: 0, props: Props::default() }, V311);
        roundtrip(Packet::PubAck { id: 9, code: rc::NO_MATCHING_SUBSCRIBERS, props: Props::default() }, V5);
        roundtrip(Packet::PingReq, V311);
        roundtrip(Packet::Disconnect { code: 0, props: Props::default() }, V5);
        roundtrip(Packet::Disconnect { code: rc::SESSION_TAKEN_OVER, props: Props::default() }, V5);
    }

    #[test]
    fn subscribe_roundtrip() {
        let o = SubOpts { qos: 1, no_local: true, retain_as_published: true, retain_handling: 2 };
        roundtrip(
            Packet::Subscribe { id: 3, props: Props { subscription_ids: vec![42], ..Default::default() }, filters: vec![("a/+".into(), o)] },
            V5,
        );
        roundtrip(Packet::Subscribe { id: 3, props: Props::default(), filters: vec![("a/#".into(), SubOpts { qos: 1, ..Default::default() })] }, V311);
        roundtrip(Packet::SubAck { id: 3, props: Props::default(), codes: vec![0, 1, 0x80] }, V311);
        roundtrip(Packet::Unsubscribe { id: 4, props: Props::default(), filters: vec!["a".into(), "b".into()] }, V5);
        roundtrip(Packet::UnsubAck { id: 4, props: Props::default(), codes: vec![0, 0x11] }, V5);
        roundtrip(Packet::ConnAck { session_present: true, code: 0, props: Props { maximum_qos: Some(1), ..Default::default() } }, V5);
    }

    #[test]
    fn rejects_bad_input() {
        let mut b = BytesMut::from(&[0x30u8, 0xff, 0xff, 0xff, 0xff, 0x01][..]);
        assert!(matches!(decode(&mut b, V311, u32::MAX), Err(DecodeError::Malformed(_))));
        let mut b = BytesMut::from(&[0x30u8, 0x80, 0x80, 0x01][..]);
        assert_eq!(decode(&mut b, V311, 1000), Err(DecodeError::TooLarge(16384)));
        // QoS 3.
        let mut b = BytesMut::from(&[0x36u8, 0x03, 0x00, 0x01, b'a'][..]);
        assert!(decode(&mut b, V311, u32::MAX).is_err());
        // Truncated string inside a complete frame.
        let mut b = BytesMut::from(&[0x30u8, 0x02, 0x00, 0x05][..]);
        assert!(decode(&mut b, V311, u32::MAX).is_err());
        // Unsupported protocol level.
        let mut b = BytesMut::new();
        Packet::Connect(Box::new(Connect {
            version: 6,
            clean_start: true,
            keep_alive: 0,
            props: Props::default(),
            client_id: "x".into(),
            will: None,
            username: None,
            password: None,
        }))
        .encode(V311, &mut b);
        assert_eq!(decode(&mut b, V311, u32::MAX), Err(DecodeError::UnsupportedVersion(6)));
        // Unknown v5 property.
        let mut b = BytesMut::from(&[0xE0u8, 0x03, 0x00, 0x01, 0x7f][..]);
        assert!(decode(&mut b, V5, u32::MAX).is_err());
    }
}
