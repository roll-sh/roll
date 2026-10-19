//! AMQP 1.0 frames and performatives (spec part 2), the SASL frames (part 5.3) and the
//! delivery states and terminus definitions they carry.

use crate::types::{CodecError, CodecResult, Value};
use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const FRAME_AMQP: u8 = 0;
pub const FRAME_SASL: u8 = 1;

/// Descriptor codes (`amqp:*:list` and friends).
pub mod code {
    pub const OPEN: u64 = 0x10;
    pub const BEGIN: u64 = 0x11;
    pub const ATTACH: u64 = 0x12;
    pub const FLOW: u64 = 0x13;
    pub const TRANSFER: u64 = 0x14;
    pub const DISPOSITION: u64 = 0x15;
    pub const DETACH: u64 = 0x16;
    pub const END: u64 = 0x17;
    pub const CLOSE: u64 = 0x18;
    pub const ERROR: u64 = 0x1d;
    pub const RECEIVED: u64 = 0x23;
    pub const ACCEPTED: u64 = 0x24;
    pub const REJECTED: u64 = 0x25;
    pub const RELEASED: u64 = 0x26;
    pub const MODIFIED: u64 = 0x27;
    pub const SOURCE: u64 = 0x28;
    pub const TARGET: u64 = 0x29;
    pub const COORDINATOR: u64 = 0x30;
    pub const SASL_MECHANISMS: u64 = 0x40;
    pub const SASL_INIT: u64 = 0x41;
    pub const SASL_CHALLENGE: u64 = 0x42;
    pub const SASL_RESPONSE: u64 = 0x43;
    pub const SASL_OUTCOME: u64 = 0x44;
    pub const HEADER: u64 = 0x70;
    pub const DELIVERY_ANNOTATIONS: u64 = 0x71;
    pub const MESSAGE_ANNOTATIONS: u64 = 0x72;
    pub const PROPERTIES: u64 = 0x73;
    pub const APPLICATION_PROPERTIES: u64 = 0x74;
    pub const DATA: u64 = 0x75;
    pub const AMQP_SEQUENCE: u64 = 0x76;
    pub const AMQP_VALUE: u64 = 0x77;
    pub const FOOTER: u64 = 0x78;
}

const NAMES: &[(&str, u64)] = &[
    ("amqp:open:list", code::OPEN),
    ("amqp:begin:list", code::BEGIN),
    ("amqp:attach:list", code::ATTACH),
    ("amqp:flow:list", code::FLOW),
    ("amqp:transfer:list", code::TRANSFER),
    ("amqp:disposition:list", code::DISPOSITION),
    ("amqp:detach:list", code::DETACH),
    ("amqp:end:list", code::END),
    ("amqp:close:list", code::CLOSE),
    ("amqp:error:list", code::ERROR),
    ("amqp:received:list", code::RECEIVED),
    ("amqp:accepted:list", code::ACCEPTED),
    ("amqp:rejected:list", code::REJECTED),
    ("amqp:released:list", code::RELEASED),
    ("amqp:modified:list", code::MODIFIED),
    ("amqp:source:list", code::SOURCE),
    ("amqp:target:list", code::TARGET),
    ("amqp:coordinator:list", code::COORDINATOR),
    ("amqp:sasl-mechanisms:list", code::SASL_MECHANISMS),
    ("amqp:sasl-init:list", code::SASL_INIT),
    ("amqp:sasl-challenge:list", code::SASL_CHALLENGE),
    ("amqp:sasl-response:list", code::SASL_RESPONSE),
    ("amqp:sasl-outcome:list", code::SASL_OUTCOME),
    ("amqp:header:list", code::HEADER),
    ("amqp:delivery-annotations:map", code::DELIVERY_ANNOTATIONS),
    ("amqp:message-annotations:map", code::MESSAGE_ANNOTATIONS),
    ("amqp:properties:list", code::PROPERTIES),
    ("amqp:application-properties:map", code::APPLICATION_PROPERTIES),
    ("amqp:data:binary", code::DATA),
    ("amqp:amqp-sequence:list", code::AMQP_SEQUENCE),
    ("amqp:amqp-value:*", code::AMQP_VALUE),
    ("amqp:footer:map", code::FOOTER),
];

/// Resolve a numeric or symbolic descriptor.
pub fn descriptor_code(d: &Value) -> Option<u64> {
    match d {
        Value::Ulong(c) => Some(*c),
        Value::Symbol(s) => NAMES.iter().find(|(n, _)| n == s).map(|(_, c)| *c),
        _ => None,
    }
}

fn bad(msg: impl Into<String>) -> CodecError {
    CodecError(msg.into())
}

/// Positional fields of a described list, with defaults for missing or null entries.
struct Fields(Vec<Value>);

impl Fields {
    fn from(v: Value) -> CodecResult<Self> {
        match v {
            Value::List(l) => Ok(Fields(l)),
            _ => Err(bad("performative body is not a list")),
        }
    }
    fn at(&self, i: usize) -> &Value {
        self.0.get(i).unwrap_or(&Value::Null)
    }
    fn take(&mut self, i: usize) -> Value {
        self.0.get_mut(i).map(|v| std::mem::replace(v, Value::Null)).unwrap_or(Value::Null)
    }
    fn u32(&self, i: usize) -> Option<u32> {
        self.at(i).as_u32()
    }
    fn u64(&self, i: usize) -> Option<u64> {
        self.at(i).as_u64()
    }
    fn bool(&self, i: usize, d: bool) -> bool {
        self.at(i).as_bool().unwrap_or(d)
    }
    fn string(&self, i: usize) -> Option<String> {
        self.at(i).as_str().map(String::from)
    }
    fn opt(&mut self, i: usize) -> Option<Value> {
        match self.take(i) {
            Value::Null => None,
            v => Some(v),
        }
    }
    fn symbols(&self, i: usize) -> Vec<String> {
        symbols(self.at(i))
    }
}

/// A symbol or an array of symbols (multiple="true" fields).
pub fn symbols(v: &Value) -> Vec<String> {
    match v {
        Value::Symbol(s) => vec![s.clone()],
        Value::Array(a) | Value::List(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    }
}

fn sym_array(v: &[String]) -> Value {
    if v.is_empty() { Value::Null } else { Value::Array(v.iter().map(|s| Value::sym(s.clone())).collect()) }
}

fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Value) -> Value {
    v.map(f).unwrap_or(Value::Null)
}

/// A described list with trailing nulls trimmed.
fn list(code: u64, mut fields: Vec<Value>) -> Value {
    while fields.last().is_some_and(Value::is_null) {
        fields.pop();
    }
    Value::described(code, Value::List(fields))
}

// ── error ──

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AmqpErr {
    pub condition: String,
    pub description: Option<String>,
    pub info: Option<Value>,
}

impl AmqpErr {
    pub fn new(condition: &str, description: impl Into<String>) -> Self {
        AmqpErr { condition: condition.into(), description: Some(description.into()), info: None }
    }

    pub fn to_value(&self) -> Value {
        list(code::ERROR, vec![Value::sym(&self.condition), opt(self.description.clone(), Value::String), self.info.clone().unwrap_or(Value::Null)])
    }

    fn from_value(v: Value) -> Option<Self> {
        let (_, body) = described(v).ok()?;
        let mut f = Fields::from(body).ok()?;
        Some(AmqpErr { condition: f.string(0).unwrap_or_default(), description: f.string(1), info: f.opt(2) })
    }
}

impl std::fmt::Display for AmqpErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.condition, self.description.as_deref().unwrap_or(""))
    }
}

fn described(v: Value) -> CodecResult<(u64, Value)> {
    match v {
        Value::Described(d, body) => Ok((descriptor_code(&d).ok_or_else(|| bad("unknown descriptor"))?, *body)),
        _ => Err(bad("expected a described value")),
    }
}

fn opt_err(v: Value) -> Option<AmqpErr> {
    if v.is_null() { None } else { AmqpErr::from_value(v) }
}

// ── terminus ──

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Source {
    pub address: Option<String>,
    pub durable: u32,
    pub expiry_policy: Option<String>,
    pub timeout: u32,
    pub dynamic: bool,
    pub dynamic_node_properties: Option<Value>,
    pub distribution_mode: Option<String>,
    pub filter: Option<Vec<(Value, Value)>>,
    pub default_outcome: Option<Value>,
    pub outcomes: Vec<String>,
    pub capabilities: Vec<String>,
}

impl Source {
    fn from_value(v: Value) -> CodecResult<Option<Self>> {
        if v.is_null() {
            return Ok(None);
        }
        let (c, body) = described(v)?;
        if c != code::SOURCE {
            return Err(bad("expected a source"));
        }
        let mut f = Fields::from(body)?;
        Ok(Some(Source {
            address: f.string(0),
            durable: f.u32(1).unwrap_or(0),
            expiry_policy: f.string(2),
            timeout: f.u32(3).unwrap_or(0),
            dynamic: f.bool(4, false),
            dynamic_node_properties: f.opt(5),
            distribution_mode: f.string(6),
            filter: match f.take(7) {
                Value::Map(m) => Some(m),
                _ => None,
            },
            default_outcome: f.opt(8),
            outcomes: f.symbols(9),
            capabilities: f.symbols(10),
        }))
    }

    pub fn to_value(&self) -> Value {
        list(
            code::SOURCE,
            vec![
                opt(self.address.clone(), Value::String),
                if self.durable == 0 { Value::Null } else { Value::Uint(self.durable) },
                opt(self.expiry_policy.clone(), Value::Symbol),
                if self.timeout == 0 { Value::Null } else { Value::Uint(self.timeout) },
                if self.dynamic { Value::Bool(true) } else { Value::Null },
                self.dynamic_node_properties.clone().unwrap_or(Value::Null),
                opt(self.distribution_mode.clone(), Value::Symbol),
                opt(self.filter.clone(), Value::Map),
                self.default_outcome.clone().unwrap_or(Value::Null),
                sym_array(&self.outcomes),
                sym_array(&self.capabilities),
            ],
        )
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Target {
    pub address: Option<String>,
    pub durable: u32,
    pub expiry_policy: Option<String>,
    pub timeout: u32,
    pub dynamic: bool,
    pub dynamic_node_properties: Option<Value>,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TargetKind {
    Target(Target),
    /// A transaction coordinator (not supported).
    Coordinator,
}

impl Target {
    fn from_value(v: Value) -> CodecResult<Option<TargetKind>> {
        if v.is_null() {
            return Ok(None);
        }
        let (c, body) = described(v)?;
        if c == code::COORDINATOR {
            return Ok(Some(TargetKind::Coordinator));
        }
        if c != code::TARGET {
            return Err(bad("expected a target"));
        }
        let mut f = Fields::from(body)?;
        Ok(Some(TargetKind::Target(Target {
            address: f.string(0),
            durable: f.u32(1).unwrap_or(0),
            expiry_policy: f.string(2),
            timeout: f.u32(3).unwrap_or(0),
            dynamic: f.bool(4, false),
            dynamic_node_properties: f.opt(5),
            capabilities: f.symbols(6),
        })))
    }

    pub fn to_value(&self) -> Value {
        list(
            code::TARGET,
            vec![
                opt(self.address.clone(), Value::String),
                if self.durable == 0 { Value::Null } else { Value::Uint(self.durable) },
                opt(self.expiry_policy.clone(), Value::Symbol),
                if self.timeout == 0 { Value::Null } else { Value::Uint(self.timeout) },
                if self.dynamic { Value::Bool(true) } else { Value::Null },
                self.dynamic_node_properties.clone().unwrap_or(Value::Null),
                sym_array(&self.capabilities),
            ],
        )
    }
}

// ── delivery states ──

#[derive(Clone, Debug, PartialEq)]
pub enum DeliveryState {
    Received { section_number: u32, section_offset: u64 },
    Accepted,
    Rejected(Option<AmqpErr>),
    Released,
    Modified { delivery_failed: bool, undeliverable_here: bool, message_annotations: Option<Value> },
    /// Anything else (transactional states): kept so it can be reported.
    Other(u64),
}

impl DeliveryState {
    pub fn from_value(v: Value) -> CodecResult<Option<Self>> {
        if v.is_null() {
            return Ok(None);
        }
        let (c, body) = described(v)?;
        let mut f = match body {
            Value::List(l) => Fields(l),
            _ => Fields(vec![]),
        };
        Ok(Some(match c {
            code::RECEIVED => DeliveryState::Received { section_number: f.u32(0).unwrap_or(0), section_offset: f.u64(1).unwrap_or(0) },
            code::ACCEPTED => DeliveryState::Accepted,
            code::REJECTED => DeliveryState::Rejected(opt_err(f.take(0))),
            code::RELEASED => DeliveryState::Released,
            code::MODIFIED => DeliveryState::Modified {
                delivery_failed: f.bool(0, false),
                undeliverable_here: f.bool(1, false),
                message_annotations: f.opt(2),
            },
            c => DeliveryState::Other(c),
        }))
    }

    pub fn to_value(&self) -> Value {
        match self {
            DeliveryState::Received { section_number, section_offset } => {
                list(code::RECEIVED, vec![Value::Uint(*section_number), Value::Ulong(*section_offset)])
            }
            DeliveryState::Accepted => list(code::ACCEPTED, vec![]),
            DeliveryState::Rejected(e) => list(code::REJECTED, vec![opt(e.as_ref(), AmqpErr::to_value)]),
            DeliveryState::Released => list(code::RELEASED, vec![]),
            DeliveryState::Modified { delivery_failed, undeliverable_here, message_annotations } => list(
                code::MODIFIED,
                vec![Value::Bool(*delivery_failed), Value::Bool(*undeliverable_here), message_annotations.clone().unwrap_or(Value::Null)],
            ),
            DeliveryState::Other(c) => list(*c, vec![]),
        }
    }
}

// ── performatives ──

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Open {
    pub container_id: String,
    pub hostname: Option<String>,
    pub max_frame_size: u32,
    pub channel_max: u16,
    pub idle_time_out: Option<u32>,
    pub offered_capabilities: Vec<String>,
    pub desired_capabilities: Vec<String>,
    pub properties: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Begin {
    pub remote_channel: Option<u16>,
    pub next_outgoing_id: u32,
    pub incoming_window: u32,
    pub outgoing_window: u32,
    pub handle_max: u32,
    pub offered_capabilities: Vec<String>,
    pub desired_capabilities: Vec<String>,
    pub properties: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Attach {
    pub name: String,
    pub handle: u32,
    /// false = sender, true = receiver.
    pub role: bool,
    pub snd_settle_mode: u8,
    pub rcv_settle_mode: u8,
    pub source: Option<Source>,
    pub target: Option<TargetKind>,
    pub unsettled: Option<Value>,
    pub incomplete_unsettled: bool,
    pub initial_delivery_count: Option<u32>,
    pub max_message_size: Option<u64>,
    pub offered_capabilities: Vec<String>,
    pub desired_capabilities: Vec<String>,
    pub properties: Option<Value>,
}

pub const ROLE_SENDER: bool = false;
pub const ROLE_RECEIVER: bool = true;
pub const SND_UNSETTLED: u8 = 0;
pub const SND_SETTLED: u8 = 1;
pub const SND_MIXED: u8 = 2;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Flow {
    pub next_incoming_id: Option<u32>,
    pub incoming_window: u32,
    pub next_outgoing_id: u32,
    pub outgoing_window: u32,
    pub handle: Option<u32>,
    pub delivery_count: Option<u32>,
    pub link_credit: Option<u32>,
    pub available: Option<u32>,
    pub drain: bool,
    pub echo: bool,
    pub properties: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Transfer {
    pub handle: u32,
    pub delivery_id: Option<u32>,
    pub delivery_tag: Option<Bytes>,
    pub message_format: Option<u32>,
    pub settled: Option<bool>,
    pub more: bool,
    pub rcv_settle_mode: Option<u8>,
    pub state: Option<DeliveryState>,
    pub resume: bool,
    pub aborted: bool,
    pub batchable: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Disposition {
    pub role: bool,
    pub first: u32,
    pub last: Option<u32>,
    pub settled: bool,
    pub state: Option<DeliveryState>,
    pub batchable: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Detach {
    pub handle: u32,
    pub closed: bool,
    pub error: Option<AmqpErr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Performative {
    Open(Open),
    Begin(Begin),
    Attach(Box<Attach>),
    Flow(Flow),
    Transfer(Transfer),
    Disposition(Disposition),
    Detach(Detach),
    End(Option<AmqpErr>),
    Close(Option<AmqpErr>),
}

impl Performative {
    pub fn name(&self) -> &'static str {
        match self {
            Performative::Open(_) => "open",
            Performative::Begin(_) => "begin",
            Performative::Attach(_) => "attach",
            Performative::Flow(_) => "flow",
            Performative::Transfer(_) => "transfer",
            Performative::Disposition(_) => "disposition",
            Performative::Detach(_) => "detach",
            Performative::End(_) => "end",
            Performative::Close(_) => "close",
        }
    }

    pub fn decode(v: Value) -> CodecResult<Self> {
        let (c, body) = described(v)?;
        let mut f = Fields::from(body)?;
        Ok(match c {
            code::OPEN => Performative::Open(Open {
                container_id: f.string(0).ok_or_else(|| bad("open.container-id is mandatory"))?,
                hostname: f.string(1),
                max_frame_size: f.u32(2).unwrap_or(u32::MAX),
                channel_max: f.u32(3).map(|v| v.min(65535) as u16).unwrap_or(65535),
                idle_time_out: f.u32(4),
                offered_capabilities: f.symbols(7),
                desired_capabilities: f.symbols(8),
                properties: f.opt(9),
            }),
            code::BEGIN => Performative::Begin(Begin {
                remote_channel: f.u32(0).map(|v| v as u16),
                next_outgoing_id: f.u32(1).unwrap_or(0),
                incoming_window: f.u32(2).unwrap_or(0),
                outgoing_window: f.u32(3).unwrap_or(0),
                handle_max: f.u32(4).unwrap_or(u32::MAX),
                offered_capabilities: f.symbols(5),
                desired_capabilities: f.symbols(6),
                properties: f.opt(7),
            }),
            code::ATTACH => Performative::Attach(Box::new(Attach {
                name: f.string(0).ok_or_else(|| bad("attach.name is mandatory"))?,
                handle: f.u32(1).ok_or_else(|| bad("attach.handle is mandatory"))?,
                role: f.bool(2, false),
                snd_settle_mode: f.u32(3).unwrap_or(SND_MIXED as u32) as u8,
                rcv_settle_mode: f.u32(4).unwrap_or(0) as u8,
                source: Source::from_value(f.take(5))?,
                target: Target::from_value(f.take(6))?,
                unsettled: f.opt(7),
                incomplete_unsettled: f.bool(8, false),
                initial_delivery_count: f.u32(9),
                max_message_size: f.u64(10).filter(|v| *v > 0),
                offered_capabilities: f.symbols(11),
                desired_capabilities: f.symbols(12),
                properties: f.opt(13),
            })),
            code::FLOW => Performative::Flow(Flow {
                next_incoming_id: f.u32(0),
                incoming_window: f.u32(1).unwrap_or(0),
                next_outgoing_id: f.u32(2).unwrap_or(0),
                outgoing_window: f.u32(3).unwrap_or(0),
                handle: f.u32(4),
                delivery_count: f.u32(5),
                link_credit: f.u32(6),
                available: f.u32(7),
                drain: f.bool(8, false),
                echo: f.bool(9, false),
                properties: f.opt(10),
            }),
            code::TRANSFER => Performative::Transfer(Transfer {
                handle: f.u32(0).ok_or_else(|| bad("transfer.handle is mandatory"))?,
                delivery_id: f.u32(1),
                delivery_tag: match f.take(2) {
                    Value::Binary(b) => Some(b),
                    _ => None,
                },
                message_format: f.u32(3),
                settled: f.at(4).as_bool(),
                more: f.bool(5, false),
                rcv_settle_mode: f.u32(6).map(|v| v as u8),
                state: DeliveryState::from_value(f.take(7))?,
                resume: f.bool(8, false),
                aborted: f.bool(9, false),
                batchable: f.bool(10, false),
            }),
            code::DISPOSITION => Performative::Disposition(Disposition {
                role: f.bool(0, false),
                first: f.u32(1).ok_or_else(|| bad("disposition.first is mandatory"))?,
                last: f.u32(2),
                settled: f.bool(3, false),
                state: DeliveryState::from_value(f.take(4))?,
                batchable: f.bool(5, false),
            }),
            code::DETACH => Performative::Detach(Detach { handle: f.u32(0).unwrap_or(0), closed: f.bool(1, false), error: opt_err(f.take(2)) }),
            code::END => Performative::End(opt_err(f.take(0))),
            code::CLOSE => Performative::Close(opt_err(f.take(0))),
            c => return Err(bad(format!("unexpected performative 0x{c:02x}"))),
        })
    }

    pub fn to_value(&self) -> Value {
        match self {
            Performative::Open(o) => list(
                code::OPEN,
                vec![
                    Value::str(&o.container_id),
                    opt(o.hostname.clone(), Value::String),
                    Value::Uint(o.max_frame_size),
                    Value::Ushort(o.channel_max),
                    opt(o.idle_time_out, Value::Uint),
                    Value::Null,
                    Value::Null,
                    sym_array(&o.offered_capabilities),
                    sym_array(&o.desired_capabilities),
                    o.properties.clone().unwrap_or(Value::Null),
                ],
            ),
            Performative::Begin(b) => list(
                code::BEGIN,
                vec![
                    opt(b.remote_channel, Value::Ushort),
                    Value::Uint(b.next_outgoing_id),
                    Value::Uint(b.incoming_window),
                    Value::Uint(b.outgoing_window),
                    Value::Uint(b.handle_max),
                    sym_array(&b.offered_capabilities),
                    sym_array(&b.desired_capabilities),
                    b.properties.clone().unwrap_or(Value::Null),
                ],
            ),
            Performative::Attach(a) => list(
                code::ATTACH,
                vec![
                    Value::str(&a.name),
                    Value::Uint(a.handle),
                    Value::Bool(a.role),
                    Value::Ubyte(a.snd_settle_mode),
                    Value::Ubyte(a.rcv_settle_mode),
                    opt(a.source.as_ref(), Source::to_value),
                    match &a.target {
                        Some(TargetKind::Target(t)) => t.to_value(),
                        Some(TargetKind::Coordinator) => list(code::COORDINATOR, vec![]),
                        None => Value::Null,
                    },
                    a.unsettled.clone().unwrap_or(Value::Null),
                    if a.incomplete_unsettled { Value::Bool(true) } else { Value::Null },
                    opt(a.initial_delivery_count, Value::Uint),
                    opt(a.max_message_size, Value::Ulong),
                    sym_array(&a.offered_capabilities),
                    sym_array(&a.desired_capabilities),
                    a.properties.clone().unwrap_or(Value::Null),
                ],
            ),
            Performative::Flow(f) => list(
                code::FLOW,
                vec![
                    opt(f.next_incoming_id, Value::Uint),
                    Value::Uint(f.incoming_window),
                    Value::Uint(f.next_outgoing_id),
                    Value::Uint(f.outgoing_window),
                    opt(f.handle, Value::Uint),
                    opt(f.delivery_count, Value::Uint),
                    opt(f.link_credit, Value::Uint),
                    opt(f.available, Value::Uint),
                    Value::Bool(f.drain),
                    Value::Bool(f.echo),
                    f.properties.clone().unwrap_or(Value::Null),
                ],
            ),
            Performative::Transfer(t) => list(
                code::TRANSFER,
                vec![
                    Value::Uint(t.handle),
                    opt(t.delivery_id, Value::Uint),
                    opt(t.delivery_tag.clone(), Value::Binary),
                    opt(t.message_format, Value::Uint),
                    opt(t.settled, Value::Bool),
                    if t.more { Value::Bool(true) } else { Value::Null },
                    opt(t.rcv_settle_mode, Value::Ubyte),
                    opt(t.state.as_ref(), DeliveryState::to_value),
                    if t.resume { Value::Bool(true) } else { Value::Null },
                    if t.aborted { Value::Bool(true) } else { Value::Null },
                    if t.batchable { Value::Bool(true) } else { Value::Null },
                ],
            ),
            Performative::Disposition(d) => list(
                code::DISPOSITION,
                vec![
                    Value::Bool(d.role),
                    Value::Uint(d.first),
                    opt(d.last, Value::Uint),
                    Value::Bool(d.settled),
                    opt(d.state.as_ref(), DeliveryState::to_value),
                    if d.batchable { Value::Bool(true) } else { Value::Null },
                ],
            ),
            Performative::Detach(d) => list(code::DETACH, vec![Value::Uint(d.handle), Value::Bool(d.closed), opt(d.error.as_ref(), AmqpErr::to_value)]),
            Performative::End(e) => list(code::END, vec![opt(e.as_ref(), AmqpErr::to_value)]),
            Performative::Close(e) => list(code::CLOSE, vec![opt(e.as_ref(), AmqpErr::to_value)]),
        }
    }
}

// ── SASL ──

#[derive(Clone, Debug, PartialEq)]
pub enum Sasl {
    Mechanisms(Vec<String>),
    Init { mechanism: String, initial_response: Option<Bytes>, hostname: Option<String> },
    Challenge(Bytes),
    Response(Bytes),
    Outcome { code: u8 },
}

pub const SASL_OK: u8 = 0;
pub const SASL_AUTH: u8 = 1;

impl Sasl {
    pub fn decode(v: Value) -> CodecResult<Self> {
        let (c, body) = described(v)?;
        let mut f = Fields::from(body)?;
        let bin = |v: Value| match v {
            Value::Binary(b) => Some(b),
            _ => None,
        };
        Ok(match c {
            code::SASL_MECHANISMS => Sasl::Mechanisms(f.symbols(0)),
            code::SASL_INIT => Sasl::Init {
                mechanism: f.string(0).ok_or_else(|| bad("sasl-init.mechanism is mandatory"))?,
                initial_response: bin(f.take(1)),
                hostname: f.string(2),
            },
            code::SASL_CHALLENGE => Sasl::Challenge(bin(f.take(0)).unwrap_or_default()),
            code::SASL_RESPONSE => Sasl::Response(bin(f.take(0)).unwrap_or_default()),
            code::SASL_OUTCOME => Sasl::Outcome { code: f.u32(0).unwrap_or(0) as u8 },
            c => return Err(bad(format!("unexpected SASL frame 0x{c:02x}"))),
        })
    }

    pub fn to_value(&self) -> Value {
        match self {
            Sasl::Mechanisms(m) => list(code::SASL_MECHANISMS, vec![Value::Array(m.iter().map(|s| Value::sym(s.clone())).collect())]),
            Sasl::Init { mechanism, initial_response, hostname } => list(
                code::SASL_INIT,
                vec![Value::sym(mechanism), opt(initial_response.clone(), Value::Binary), opt(hostname.clone(), Value::String)],
            ),
            Sasl::Challenge(b) => list(code::SASL_CHALLENGE, vec![Value::Binary(b.clone())]),
            Sasl::Response(b) => list(code::SASL_RESPONSE, vec![Value::Binary(b.clone())]),
            Sasl::Outcome { code: c } => list(code::SASL_OUTCOME, vec![Value::Ubyte(*c)]),
        }
    }
}

// ── framing ──

/// Encode one frame: 8-byte header, the performative, then the payload.
pub fn write_frame(out: &mut BytesMut, kind: u8, channel: u16, body: Option<&Value>, payload: &[u8]) {
    let at = out.len();
    out.put_u32(0);
    out.put_u8(2);
    out.put_u8(kind);
    out.put_u16(channel);
    if let Some(b) = body {
        b.encode(out);
    }
    out.put_slice(payload);
    let n = (out.len() - at) as u32;
    out[at..at + 4].copy_from_slice(&n.to_be_bytes());
}

/// A received frame: an empty body is a heartbeat.
pub struct RawFrame {
    pub kind: u8,
    pub channel: u16,
    pub body: Bytes,
}

/// Split a complete frame (`size` bytes including the header) into its parts.
pub fn parse_frame(mut f: Bytes) -> CodecResult<RawFrame> {
    if f.len() < 8 {
        return Err(bad("frame shorter than its header"));
    }
    let _size = f.get_u32();
    let doff = f.get_u8() as usize;
    let kind = f.get_u8();
    let channel = f.get_u16();
    if doff < 2 || doff * 4 > f.len() + 8 {
        return Err(bad(format!("invalid data offset {doff}")));
    }
    f.advance(doff * 4 - 8);
    Ok(RawFrame { kind, channel, body: f })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performatives_roundtrip() {
        let attach = Performative::Attach(Box::new(Attach {
            name: "l".into(),
            handle: 3,
            role: ROLE_RECEIVER,
            snd_settle_mode: SND_UNSETTLED,
            source: Some(Source {
                address: Some("/queues/q".into()),
                filter: Some(vec![(Value::sym("f"), Value::Described(Box::new(Value::sym("rabbitmq:stream-offset-spec")), Box::new(Value::str("first"))))]),
                outcomes: vec!["amqp:accepted:list".into()],
                ..Default::default()
            }),
            target: Some(TargetKind::Target(Target { address: Some("x".into()), ..Default::default() })),
            initial_delivery_count: Some(0),
            max_message_size: Some(1024),
            properties: Some(Value::Map(vec![(Value::sym("paired"), Value::Bool(true))])),
            ..Default::default()
        }));
        let p = [
            Performative::Open(Open { container_id: "c".into(), max_frame_size: 65536, channel_max: 9, idle_time_out: Some(30_000), ..Default::default() }),
            Performative::Begin(Begin { remote_channel: Some(1), next_outgoing_id: 1, incoming_window: 10, outgoing_window: 10, handle_max: 7, ..Default::default() }),
            attach,
            Performative::Flow(Flow { handle: Some(1), delivery_count: Some(4), link_credit: Some(100), drain: true, ..Default::default() }),
            Performative::Transfer(Transfer {
                handle: 1,
                delivery_id: Some(5),
                delivery_tag: Some(Bytes::from_static(b"t")),
                message_format: Some(0),
                settled: Some(false),
                more: true,
                ..Default::default()
            }),
            Performative::Disposition(Disposition {
                role: ROLE_RECEIVER,
                first: 1,
                last: Some(4),
                settled: true,
                state: Some(DeliveryState::Modified { delivery_failed: true, undeliverable_here: false, message_annotations: None }),
                ..Default::default()
            }),
            Performative::Detach(Detach { handle: 2, closed: true, error: Some(AmqpErr::new("amqp:not-found", "no queue 'q' in vhost '/'")) }),
            Performative::End(None),
            Performative::Close(Some(AmqpErr::new("amqp:connection:forced", "bye"))),
        ];
        for x in p {
            let mut b = x.to_value().to_bytes();
            let v = Value::decode(&mut b).unwrap();
            assert_eq!(Performative::decode(v).unwrap(), x);
        }
    }

    #[test]
    fn symbolic_descriptors() {
        let v = Value::Described(Box::new(Value::sym("amqp:end:list")), Box::new(Value::List(vec![])));
        assert_eq!(Performative::decode(v).unwrap(), Performative::End(None));
    }

    #[test]
    fn sasl_roundtrip() {
        for s in [
            Sasl::Mechanisms(vec!["PLAIN".into(), "ANONYMOUS".into()]),
            Sasl::Init { mechanism: "PLAIN".into(), initial_response: Some(Bytes::from_static(b"\0guest\0guest")), hostname: None },
            Sasl::Outcome { code: SASL_OK },
        ] {
            let mut b = s.to_value().to_bytes();
            assert_eq!(Sasl::decode(Value::decode(&mut b).unwrap()).unwrap(), s);
        }
    }

    #[test]
    fn frame_roundtrip() {
        let mut b = BytesMut::new();
        write_frame(&mut b, FRAME_AMQP, 3, Some(&Performative::End(None).to_value()), b"payload");
        let f = parse_frame(b.freeze()).unwrap();
        assert_eq!((f.kind, f.channel), (FRAME_AMQP, 3));
        let mut body = f.body;
        Value::decode(&mut body).unwrap();
        assert_eq!(&body[..], b"payload");
    }
}
