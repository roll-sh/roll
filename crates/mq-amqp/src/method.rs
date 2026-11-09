//! AMQP 0-9-1 methods (with RabbitMQ's extensions: confirms, basic.nack, exchange-to-exchange
//! bindings, connection.blocked and update-secret), generated from a field table so both
//! directions are always in sync.

use bytes::{BufMut, Bytes, BytesMut};
use mq_core::value::{DecodeError, DecodeResult, FieldTable, get_longstr, get_shortstr, get_u8, get_u16, get_u32, get_u64, put_longstr, put_shortstr};

pub const FRAME_METHOD: u8 = 1;
pub const FRAME_HEADER: u8 = 2;
pub const FRAME_BODY: u8 = 3;
pub const FRAME_HEARTBEAT: u8 = 8;
pub const FRAME_END: u8 = 0xCE;
pub const PROTOCOL_HEADER: &[u8; 8] = b"AMQP\x00\x00\x09\x01";
pub const CLASS_BASIC: u16 = 60;

/// Field encoder; consecutive bits share an octet as the spec requires.
pub struct Enc<'a> {
    pub out: &'a mut BytesMut,
    bit_at: Option<(usize, u8)>,
}

impl<'a> Enc<'a> {
    pub fn new(out: &'a mut BytesMut) -> Self {
        Enc { out, bit_at: None }
    }
    fn end_bits(&mut self) {
        self.bit_at = None;
    }
    pub fn bit(&mut self, v: &bool) {
        match &mut self.bit_at {
            Some((at, n)) if *n < 8 => {
                if *v {
                    self.out[*at] |= 1 << *n;
                }
                *n += 1;
            }
            _ => {
                let at = self.out.len();
                self.out.put_u8(*v as u8);
                self.bit_at = Some((at, 1));
            }
        }
    }
    pub fn octet(&mut self, v: &u8) {
        self.end_bits();
        self.out.put_u8(*v);
    }
    pub fn short(&mut self, v: &u16) {
        self.end_bits();
        self.out.put_u16(*v);
    }
    pub fn long(&mut self, v: &u32) {
        self.end_bits();
        self.out.put_u32(*v);
    }
    pub fn longlong(&mut self, v: &u64) {
        self.end_bits();
        self.out.put_u64(*v);
    }
    pub fn shortstr(&mut self, v: &str) {
        self.end_bits();
        put_shortstr(self.out, v);
    }
    pub fn longstr(&mut self, v: &Bytes) {
        self.end_bits();
        put_longstr(self.out, v);
    }
    pub fn table(&mut self, v: &FieldTable) {
        self.end_bits();
        v.encode(self.out);
    }
}

pub struct Dec<'a> {
    pub buf: &'a mut Bytes,
    bits: Option<(u8, u8)>,
}

impl<'a> Dec<'a> {
    pub fn new(buf: &'a mut Bytes) -> Self {
        Dec { buf, bits: None }
    }
    pub fn bit(&mut self) -> DecodeResult<bool> {
        match &mut self.bits {
            Some((byte, n)) if *n < 8 => {
                let v = *byte & (1 << *n) != 0;
                *n += 1;
                Ok(v)
            }
            _ => {
                let b = get_u8(self.buf)?;
                self.bits = Some((b, 1));
                Ok(b & 1 != 0)
            }
        }
    }
    pub fn octet(&mut self) -> DecodeResult<u8> {
        self.bits = None;
        get_u8(self.buf)
    }
    pub fn short(&mut self) -> DecodeResult<u16> {
        self.bits = None;
        get_u16(self.buf)
    }
    pub fn long(&mut self) -> DecodeResult<u32> {
        self.bits = None;
        get_u32(self.buf)
    }
    pub fn longlong(&mut self) -> DecodeResult<u64> {
        self.bits = None;
        get_u64(self.buf)
    }
    pub fn shortstr(&mut self) -> DecodeResult<String> {
        self.bits = None;
        get_shortstr(self.buf)
    }
    pub fn longstr(&mut self) -> DecodeResult<Bytes> {
        self.bits = None;
        get_longstr(self.buf)
    }
    pub fn table(&mut self) -> DecodeResult<FieldTable> {
        self.bits = None;
        FieldTable::decode(self.buf)
    }
}

macro_rules! ty {
    (bit) => { bool };
    (octet) => { u8 };
    (short) => { u16 };
    (long) => { u32 };
    (longlong) => { u64 };
    (shortstr) => { String };
    (longstr) => { Bytes };
    (table) => { FieldTable };
}

macro_rules! methods {
    ($( $name:ident = ($class:expr, $method:expr, $text:expr) { $($field:ident : $ty:ident),* $(,)? } ),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq)]
        pub enum Method {
            $( $name { $($field: ty!($ty)),* } ),*
        }

        impl Method {
            pub fn ids(&self) -> (u16, u16) {
                match self { $( Method::$name { .. } => ($class, $method) ),* }
            }

            pub fn name(&self) -> &'static str {
                match self { $( Method::$name { .. } => $text ),* }
            }

            #[allow(unused_variables)]
            pub fn encode_args(&self, e: &mut Enc) {
                match self { $( Method::$name { $($field),* } => { $( e.$ty($field); )* } ),* }
            }

            pub fn decode(class: u16, method: u16, d: &mut Dec) -> DecodeResult<Method> {
                match (class, method) {
                    $( ($class, $method) => Ok(Method::$name { $($field: d.$ty()?),* }), )*
                    _ => Err(DecodeError("unknown method")),
                }
            }
        }
    };
}

methods! {
    ConnectionStart = (10, 10, "connection.start") { version_major: octet, version_minor: octet, server_properties: table, mechanisms: longstr, locales: longstr },
    ConnectionStartOk = (10, 11, "connection.start_ok") { client_properties: table, mechanism: shortstr, response: longstr, locale: shortstr },
    ConnectionSecure = (10, 20, "connection.secure") { challenge: longstr },
    ConnectionSecureOk = (10, 21, "connection.secure_ok") { response: longstr },
    ConnectionTune = (10, 30, "connection.tune") { channel_max: short, frame_max: long, heartbeat: short },
    ConnectionTuneOk = (10, 31, "connection.tune_ok") { channel_max: short, frame_max: long, heartbeat: short },
    ConnectionOpen = (10, 40, "connection.open") { virtual_host: shortstr, capabilities: shortstr, insist: bit },
    ConnectionOpenOk = (10, 41, "connection.open_ok") { known_hosts: shortstr },
    ConnectionClose = (10, 50, "connection.close") { reply_code: short, reply_text: shortstr, class_id: short, method_id: short },
    ConnectionCloseOk = (10, 51, "connection.close_ok") {},
    ConnectionBlocked = (10, 60, "connection.blocked") { reason: shortstr },
    ConnectionUnblocked = (10, 61, "connection.unblocked") {},
    ConnectionUpdateSecret = (10, 70, "connection.update_secret") { new_secret: longstr, reason: shortstr },
    ConnectionUpdateSecretOk = (10, 71, "connection.update_secret_ok") {},

    ChannelOpen = (20, 10, "channel.open") { out_of_band: shortstr },
    ChannelOpenOk = (20, 11, "channel.open_ok") { channel_id: longstr },
    ChannelFlow = (20, 20, "channel.flow") { active: bit },
    ChannelFlowOk = (20, 21, "channel.flow_ok") { active: bit },
    ChannelClose = (20, 40, "channel.close") { reply_code: short, reply_text: shortstr, class_id: short, method_id: short },
    ChannelCloseOk = (20, 41, "channel.close_ok") {},

    AccessRequest = (30, 10, "access.request") { realm: shortstr, exclusive: bit, passive: bit, active: bit, write: bit, read: bit },
    AccessRequestOk = (30, 11, "access.request_ok") { ticket: short },

    ExchangeDeclare = (40, 10, "exchange.declare") { ticket: short, exchange: shortstr, kind: shortstr, passive: bit, durable: bit, auto_delete: bit, internal: bit, nowait: bit, arguments: table },
    ExchangeDeclareOk = (40, 11, "exchange.declare_ok") {},
    ExchangeDelete = (40, 20, "exchange.delete") { ticket: short, exchange: shortstr, if_unused: bit, nowait: bit },
    ExchangeDeleteOk = (40, 21, "exchange.delete_ok") {},
    ExchangeBind = (40, 30, "exchange.bind") { ticket: short, destination: shortstr, source: shortstr, routing_key: shortstr, nowait: bit, arguments: table },
    ExchangeBindOk = (40, 31, "exchange.bind_ok") {},
    ExchangeUnbind = (40, 40, "exchange.unbind") { ticket: short, destination: shortstr, source: shortstr, routing_key: shortstr, nowait: bit, arguments: table },
    ExchangeUnbindOk = (40, 51, "exchange.unbind_ok") {},

    QueueDeclare = (50, 10, "queue.declare") { ticket: short, queue: shortstr, passive: bit, durable: bit, exclusive: bit, auto_delete: bit, nowait: bit, arguments: table },
    QueueDeclareOk = (50, 11, "queue.declare_ok") { queue: shortstr, message_count: long, consumer_count: long },
    QueueBind = (50, 20, "queue.bind") { ticket: short, queue: shortstr, exchange: shortstr, routing_key: shortstr, nowait: bit, arguments: table },
    QueueBindOk = (50, 21, "queue.bind_ok") {},
    QueuePurge = (50, 30, "queue.purge") { ticket: short, queue: shortstr, nowait: bit },
    QueuePurgeOk = (50, 31, "queue.purge_ok") { message_count: long },
    QueueDelete = (50, 40, "queue.delete") { ticket: short, queue: shortstr, if_unused: bit, if_empty: bit, nowait: bit },
    QueueDeleteOk = (50, 41, "queue.delete_ok") { message_count: long },
    QueueUnbind = (50, 50, "queue.unbind") { ticket: short, queue: shortstr, exchange: shortstr, routing_key: shortstr, arguments: table },
    QueueUnbindOk = (50, 51, "queue.unbind_ok") {},

    BasicQos = (60, 10, "basic.qos") { prefetch_size: long, prefetch_count: short, global: bit },
    BasicQosOk = (60, 11, "basic.qos_ok") {},
    BasicConsume = (60, 20, "basic.consume") { ticket: short, queue: shortstr, consumer_tag: shortstr, no_local: bit, no_ack: bit, exclusive: bit, nowait: bit, arguments: table },
    BasicConsumeOk = (60, 21, "basic.consume_ok") { consumer_tag: shortstr },
    BasicCancel = (60, 30, "basic.cancel") { consumer_tag: shortstr, nowait: bit },
    BasicCancelOk = (60, 31, "basic.cancel_ok") { consumer_tag: shortstr },
    BasicPublish = (60, 40, "basic.publish") { ticket: short, exchange: shortstr, routing_key: shortstr, mandatory: bit, immediate: bit },
    BasicReturn = (60, 50, "basic.return") { reply_code: short, reply_text: shortstr, exchange: shortstr, routing_key: shortstr },
    BasicDeliver = (60, 60, "basic.deliver") { consumer_tag: shortstr, delivery_tag: longlong, redelivered: bit, exchange: shortstr, routing_key: shortstr },
    BasicGet = (60, 70, "basic.get") { ticket: short, queue: shortstr, no_ack: bit },
    BasicGetOk = (60, 71, "basic.get_ok") { delivery_tag: longlong, redelivered: bit, exchange: shortstr, routing_key: shortstr, message_count: long },
    BasicGetEmpty = (60, 72, "basic.get_empty") { cluster_id: shortstr },
    BasicAck = (60, 80, "basic.ack") { delivery_tag: longlong, multiple: bit },
    BasicReject = (60, 90, "basic.reject") { delivery_tag: longlong, requeue: bit },
    BasicRecoverAsync = (60, 100, "basic.recover_async") { requeue: bit },
    BasicRecover = (60, 110, "basic.recover") { requeue: bit },
    BasicRecoverOk = (60, 111, "basic.recover_ok") {},
    BasicNack = (60, 120, "basic.nack") { delivery_tag: longlong, multiple: bit, requeue: bit },

    ConfirmSelect = (85, 10, "confirm.select") { nowait: bit },
    ConfirmSelectOk = (85, 11, "confirm.select_ok") {},

    TxSelect = (90, 10, "tx.select") {},
    TxSelectOk = (90, 11, "tx.select_ok") {},
    TxCommit = (90, 20, "tx.commit") {},
    TxCommitOk = (90, 21, "tx.commit_ok") {},
    TxRollback = (90, 30, "tx.rollback") {},
    TxRollbackOk = (90, 31, "tx.rollback_ok") {},
}

impl Method {
    /// Whether content (header + body frames) follows this method.
    pub fn has_content(&self) -> bool {
        matches!(self, Method::BasicPublish { .. } | Method::BasicReturn { .. } | Method::BasicDeliver { .. } | Method::BasicGetOk { .. })
    }

    /// Append a complete method frame.
    pub fn frame(&self, channel: u16, out: &mut BytesMut) {
        let start = out.len();
        out.put_u8(FRAME_METHOD);
        out.put_u16(channel);
        out.put_u32(0);
        let (c, m) = self.ids();
        out.put_u16(c);
        out.put_u16(m);
        self.encode_args(&mut Enc::new(out));
        let size = (out.len() - start - 7) as u32;
        out[start + 3..start + 7].copy_from_slice(&size.to_be_bytes());
        out.put_u8(FRAME_END);
    }

    pub fn parse(mut payload: Bytes) -> DecodeResult<Method> {
        let class = get_u16(&mut payload)?;
        let method = get_u16(&mut payload)?;
        Method::decode(class, method, &mut Dec::new(&mut payload))
    }
}

/// Append a content header and body frames.
pub fn content_frames(channel: u16, props_raw: &[u8], body: &[u8], frame_max: u32, out: &mut BytesMut) {
    out.reserve(22 + props_raw.len() + body.len() + 8 * (1 + body.len() / frame_max.max(4096) as usize));
    out.put_u8(FRAME_HEADER);
    out.put_u16(channel);
    out.put_u32((12 + props_raw.len()) as u32);
    out.put_u16(CLASS_BASIC);
    out.put_u16(0);
    out.put_u64(body.len() as u64);
    out.put_slice(props_raw);
    out.put_u8(FRAME_END);
    let max = (frame_max.max(4096) - 8) as usize;
    for chunk in body.chunks(max) {
        out.put_u8(FRAME_BODY);
        out.put_u16(channel);
        out.put_u32(chunk.len() as u32);
        out.put_slice(chunk);
        out.put_u8(FRAME_END);
    }
}

pub fn heartbeat_frame(out: &mut BytesMut) {
    out.put_slice(&[FRAME_HEARTBEAT, 0, 0, 0, 0, 0, 0, FRAME_END]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Buf;

    fn roundtrip(m: Method) {
        let mut b = BytesMut::new();
        m.frame(3, &mut b);
        let mut f = b.freeze();
        assert_eq!(f.get_u8(), FRAME_METHOD);
        assert_eq!(f.get_u16(), 3);
        let size = f.get_u32() as usize;
        let payload = f.split_to(size);
        assert_eq!(f.get_u8(), FRAME_END);
        assert_eq!(Method::parse(payload).unwrap(), m);
    }

    #[test]
    fn bits_pack() {
        roundtrip(Method::QueueDeclare {
            ticket: 0,
            queue: "q".into(),
            passive: false,
            durable: true,
            exclusive: false,
            auto_delete: true,
            nowait: true,
            arguments: FieldTable::new(),
        });
        roundtrip(Method::BasicNack { delivery_tag: 9, multiple: true, requeue: false });
        roundtrip(Method::ExchangeDeclare {
            ticket: 0,
            exchange: "x".into(),
            kind: "topic".into(),
            passive: true,
            durable: false,
            auto_delete: true,
            internal: false,
            nowait: true,
            arguments: [("alternate-exchange", "ae")].into_iter().collect(),
        });
        roundtrip(Method::ConnectionCloseOk {});
        roundtrip(Method::BasicDeliver { consumer_tag: "c".into(), delivery_tag: 1 << 40, redelivered: true, exchange: "".into(), routing_key: "rk".into() });
    }

    #[test]
    fn declare_bits_wire_layout() {
        // passive=1 durable=1 exclusive=0 auto_delete=0 nowait=1 → 0b10011.
        let m = Method::QueueDeclare { ticket: 0, queue: "".into(), passive: true, durable: true, exclusive: false, auto_delete: false, nowait: true, arguments: FieldTable::new() };
        let mut b = BytesMut::new();
        m.frame(0, &mut b);
        // header(7) + class/method(4) + ticket(2) + shortstr(1) → bits octet.
        assert_eq!(b[14], 0b10011);
    }
}
