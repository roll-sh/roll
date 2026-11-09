//! EVENT bodies (spec 4.2.6) and the schema change shared with RESULT Schema_change.
//!
//! Before v4 a FUNCTION/AGGREGATE change goes out as "the keyspace was updated" — and
//! Cassandra writes an extra empty string after it (its v3 branch reuses the v1/v2 layout),
//! which we reproduce byte for byte. Follows `org.apache.cassandra.transport.Event`.

use std::net::SocketAddr;

use bytes::BytesMut;

use crate::error::DecodeError;
use crate::version::{Dialect, Version};
use crate::wire::{PutExt, Reader};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchemaChangeType {
    Created,
    Updated,
    Dropped,
}

impl SchemaChangeType {
    pub fn name(self) -> &'static str {
        match self {
            SchemaChangeType::Created => "CREATED",
            SchemaChangeType::Updated => "UPDATED",
            SchemaChangeType::Dropped => "DROPPED",
        }
    }

    fn parse(s: &str) -> Result<Self, DecodeError> {
        match s.to_ascii_uppercase().as_str() {
            "CREATED" => Ok(SchemaChangeType::Created),
            "UPDATED" => Ok(SchemaChangeType::Updated),
            "DROPPED" => Ok(SchemaChangeType::Dropped),
            _ => Err(DecodeError::InvalidEnum { value: s.into(), ty: "SchemaChange.Change" }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchemaTarget {
    Keyspace,
    Table,
    Type,
    Function,
    Aggregate,
}

impl SchemaTarget {
    pub fn name(self) -> &'static str {
        match self {
            SchemaTarget::Keyspace => "KEYSPACE",
            SchemaTarget::Table => "TABLE",
            SchemaTarget::Type => "TYPE",
            SchemaTarget::Function => "FUNCTION",
            SchemaTarget::Aggregate => "AGGREGATE",
        }
    }

    fn parse(s: &str) -> Result<Self, DecodeError> {
        match s.to_ascii_uppercase().as_str() {
            "KEYSPACE" => Ok(SchemaTarget::Keyspace),
            "TABLE" => Ok(SchemaTarget::Table),
            "TYPE" => Ok(SchemaTarget::Type),
            "FUNCTION" => Ok(SchemaTarget::Function),
            "AGGREGATE" => Ok(SchemaTarget::Aggregate),
            _ => Err(DecodeError::InvalidEnum { value: s.into(), ty: "SchemaChange.Target" }),
        }
    }
}

/// `<change_type><target><options>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SchemaChange {
    pub change: SchemaChangeType,
    pub target: SchemaTarget,
    pub keyspace: String,
    /// Table, type, function or aggregate name (`None` for a keyspace).
    pub name: Option<String>,
    /// Argument types (CQL type names) of a function or aggregate.
    pub arg_types: Option<Vec<String>>,
}

impl SchemaChange {
    pub fn keyspace(change: SchemaChangeType, keyspace: impl Into<String>) -> Self {
        SchemaChange { change, target: SchemaTarget::Keyspace, keyspace: keyspace.into(), name: None, arg_types: None }
    }

    pub fn object(change: SchemaChangeType, target: SchemaTarget, keyspace: impl Into<String>, name: impl Into<String>) -> Self {
        SchemaChange { change, target, keyspace: keyspace.into(), name: Some(name.into()), arg_types: None }
    }

    pub fn function(
        change: SchemaChangeType,
        aggregate: bool,
        keyspace: impl Into<String>,
        name: impl Into<String>,
        arg_types: Vec<String>,
    ) -> Self {
        SchemaChange {
            change,
            target: if aggregate { SchemaTarget::Aggregate } else { SchemaTarget::Function },
            keyspace: keyspace.into(),
            name: Some(name.into()),
            arg_types: Some(arg_types),
        }
    }

    pub fn encode(&self, version: Version, out: &mut BytesMut) {
        let routine = matches!(self.target, SchemaTarget::Function | SchemaTarget::Aggregate);
        if routine && version < Version::V4 {
            out.put_string(SchemaChangeType::Updated.name());
            out.put_string(SchemaTarget::Keyspace.name());
            out.put_string(&self.keyspace);
            out.put_string("");
            return;
        }
        out.put_string(self.change.name());
        out.put_string(self.target.name());
        out.put_string(&self.keyspace);
        if self.target != SchemaTarget::Keyspace {
            out.put_string(self.name.as_deref().unwrap_or_default());
        }
        if routine {
            out.put_string_list(self.arg_types.as_deref().unwrap_or_default());
        }
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let change = SchemaChangeType::parse(&r.string()?)?;
        let target = SchemaTarget::parse(&r.string()?)?;
        let keyspace = r.string()?;
        let name = if target == SchemaTarget::Keyspace { None } else { Some(r.string()?) };
        let arg_types = if matches!(target, SchemaTarget::Function | SchemaTarget::Aggregate) { Some(r.string_list()?) } else { None };
        Ok(SchemaChange { change, target, keyspace, name, arg_types })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventType {
    TopologyChange,
    StatusChange,
    SchemaChange,
    /// Cassandra (v4+), never sent.
    TraceComplete,
    /// ScyllaDB's `system.client_routes` notification.
    ClientRoutesChange,
}

impl EventType {
    pub fn name(self) -> &'static str {
        match self {
            EventType::TopologyChange => "TOPOLOGY_CHANGE",
            EventType::StatusChange => "STATUS_CHANGE",
            EventType::SchemaChange => "SCHEMA_CHANGE",
            EventType::TraceComplete => "TRACE_COMPLETE",
            EventType::ClientRoutesChange => "CLIENT_ROUTES_CHANGE",
        }
    }

    /// A REGISTER entry as the dialect accepts it: Cassandra upper-cases the name and knows
    /// `TRACE_COMPLETE`; ScyllaDB knows `CLIENT_ROUTES_CHANGE`.
    pub fn parse(s: &str, dialect: Dialect) -> Result<Self, DecodeError> {
        let all: &[EventType] = match dialect {
            Dialect::Cassandra => &[EventType::TopologyChange, EventType::StatusChange, EventType::SchemaChange, EventType::TraceComplete],
            Dialect::Scylla => &[EventType::TopologyChange, EventType::StatusChange, EventType::SchemaChange, EventType::ClientRoutesChange],
        };
        let found = match dialect {
            Dialect::Cassandra => all.iter().find(|e| e.name() == s.to_uppercase()),
            Dialect::Scylla => all.iter().find(|e| e.name() == s),
        };
        found.copied().ok_or_else(|| DecodeError::InvalidEnum { value: s.into(), ty: "Event.Type" })
    }

    /// Every entry of a REGISTER, failing on the first unknown one.
    pub fn parse_all(names: &[String], dialect: Dialect) -> Result<Vec<Self>, DecodeError> {
        names.iter().map(|n| Self::parse(n, dialect)).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TopologyChangeType {
    NewNode,
    RemovedNode,
    MovedNode,
}

impl TopologyChangeType {
    pub fn name(self) -> &'static str {
        match self {
            TopologyChangeType::NewNode => "NEW_NODE",
            TopologyChangeType::RemovedNode => "REMOVED_NODE",
            TopologyChangeType::MovedNode => "MOVED_NODE",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusChangeType {
    Up,
    Down,
}

impl StatusChangeType {
    pub fn name(self) -> &'static str {
        match self {
            StatusChangeType::Up => "UP",
            StatusChangeType::Down => "DOWN",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    TopologyChange { change: TopologyChangeType, node: SocketAddr },
    StatusChange { change: StatusChangeType, node: SocketAddr },
    SchemaChange(SchemaChange),
    ClientRoutesChange { change: String, connection_ids: Vec<String>, host_ids: Vec<String> },
}

impl Event {
    pub fn event_type(&self) -> EventType {
        match self {
            Event::TopologyChange { .. } => EventType::TopologyChange,
            Event::StatusChange { .. } => EventType::StatusChange,
            Event::SchemaChange(_) => EventType::SchemaChange,
            Event::ClientRoutesChange { .. } => EventType::ClientRoutesChange,
        }
    }

    pub fn encode(&self, version: Version, out: &mut BytesMut) {
        out.put_string(self.event_type().name());
        match self {
            Event::TopologyChange { change, node } => {
                out.put_string(change.name());
                out.put_inet(*node);
            }
            Event::StatusChange { change, node } => {
                out.put_string(change.name());
                out.put_inet(*node);
            }
            Event::SchemaChange(c) => c.encode(version, out),
            Event::ClientRoutesChange { change, connection_ids, host_ids } => {
                out.put_string(change);
                out.put_string_list(connection_ids);
                out.put_string_list(host_ids);
            }
        }
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let ty = r.string()?;
        let bad = |value: String, ty: &'static str| DecodeError::InvalidEnum { value, ty };
        Ok(match ty.as_str() {
            "TOPOLOGY_CHANGE" => {
                let c = r.string()?;
                let change = match c.as_str() {
                    "NEW_NODE" => TopologyChangeType::NewNode,
                    "REMOVED_NODE" => TopologyChangeType::RemovedNode,
                    "MOVED_NODE" => TopologyChangeType::MovedNode,
                    _ => return Err(bad(c, "TopologyChange.Change")),
                };
                Event::TopologyChange { change, node: r.inet()? }
            }
            "STATUS_CHANGE" => {
                let c = r.string()?;
                let change = match c.as_str() {
                    "UP" => StatusChangeType::Up,
                    "DOWN" => StatusChangeType::Down,
                    _ => return Err(bad(c, "StatusChange.Status")),
                };
                Event::StatusChange { change, node: r.inet()? }
            }
            "SCHEMA_CHANGE" => Event::SchemaChange(SchemaChange::decode(r)?),
            "CLIENT_ROUTES_CHANGE" => Event::ClientRoutesChange {
                change: r.string()?,
                connection_ids: r.string_list()?,
                host_ids: r.string_list()?,
            },
            _ => return Err(bad(ty, "Event.Type")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn round(e: &Event, v: Version) -> Event {
        let mut b = BytesMut::new();
        e.encode(v, &mut b);
        let mut r = Reader::new(b.freeze());
        Event::decode(&mut r).unwrap()
    }

    fn all() -> Vec<Event> {
        let node: SocketAddr = "10.0.0.7:9042".parse().unwrap();
        vec![
            Event::TopologyChange { change: TopologyChangeType::NewNode, node },
            Event::TopologyChange { change: TopologyChangeType::RemovedNode, node },
            Event::TopologyChange { change: TopologyChangeType::MovedNode, node },
            Event::StatusChange { change: StatusChangeType::Up, node },
            Event::StatusChange { change: StatusChangeType::Down, node: "[::1]:9042".parse().unwrap() },
            Event::SchemaChange(SchemaChange::keyspace(SchemaChangeType::Created, "ks")),
            Event::SchemaChange(SchemaChange::object(SchemaChangeType::Updated, SchemaTarget::Table, "ks", "t")),
            Event::SchemaChange(SchemaChange::object(SchemaChangeType::Dropped, SchemaTarget::Type, "ks", "addr")),
            Event::SchemaChange(SchemaChange::function(SchemaChangeType::Created, false, "ks", "f", vec!["int".into(), "text".into()])),
            Event::SchemaChange(SchemaChange::function(SchemaChangeType::Dropped, true, "ks", "agg", vec![])),
            Event::ClientRoutesChange { change: "UPDATE_NODES".into(), connection_ids: vec!["c1".into()], host_ids: vec!["h1".into(), "h2".into()] },
        ]
    }

    #[test]
    fn round_trips() {
        for v in [Version::V4, Version::V5] {
            for e in all() {
                assert_eq!(round(&e, v), e);
            }
        }
    }

    #[test]
    fn v3_routine_changes_become_keyspace_updates() {
        let e = Event::SchemaChange(SchemaChange::function(SchemaChangeType::Created, false, "ks", "f", vec!["int".into()]));
        let mut b = BytesMut::new();
        e.encode(Version::V3, &mut b);
        assert_eq!(&b[..], b"\x00\x0dSCHEMA_CHANGE\x00\x07UPDATED\x00\x08KEYSPACE\x00\x02ks\x00\x00");
        assert_eq!(round(&e, Version::V3), Event::SchemaChange(SchemaChange::keyspace(SchemaChangeType::Updated, "ks")));
        // Tables and types are unchanged on v3.
        for e in &all()[5..8] {
            assert_eq!(&round(e, Version::V3), e);
        }
    }

    #[test]
    fn register_names_per_dialect() {
        assert_eq!(EventType::parse("schema_change", Dialect::Cassandra), Ok(EventType::SchemaChange));
        assert_eq!(EventType::parse("TRACE_COMPLETE", Dialect::Cassandra), Ok(EventType::TraceComplete));
        assert!(EventType::parse("CLIENT_ROUTES_CHANGE", Dialect::Cassandra).is_err());
        assert_eq!(EventType::parse("CLIENT_ROUTES_CHANGE", Dialect::Scylla), Ok(EventType::ClientRoutesChange));
        let e = EventType::parse("FOO", Dialect::Scylla).unwrap_err();
        assert_eq!(e.to_error(Dialect::Scylla).message, "Invalid value 'FOO' for Event.Type");
        assert_eq!(e.to_error(Dialect::Cassandra).message, "Invalid value 'FOO' for Type");
        assert!(EventType::parse_all(&["SCHEMA_CHANGE".into(), "x".into()], Dialect::Cassandra).is_err());
    }

    #[test]
    fn malformed_events() {
        for e in all() {
            let mut b = BytesMut::new();
            e.encode(Version::V5, &mut b);
            let full = b.freeze();
            for cut in 0..full.len() {
                assert!(Event::decode(&mut Reader::new(full.slice(..cut))).is_err());
            }
        }
        for body in [
            &b"\x00\x03FOO"[..],
            b"\x00\x0fTOPOLOGY_CHANGE\x00\x01X",
            b"\x00\x0dSTATUS_CHANGE\x00\x02UP\x07",
            b"\x00\x0dSCHEMA_CHANGE\x00\x07CREATED\x00\x04VIEW",
            b"\x00\x0dSCHEMA_CHANGE\x00\x04MADE",
        ] {
            assert!(Event::decode(&mut Reader::new(Bytes::from_static(body))).is_err());
        }
    }
}
