//! Conversions between the engine's types and the protocol's: column types, values, query
//! parameters, results, schema changes, events and errors.
//!
//! The engine speaks [`nosql_types::CqlType`] and its own result/error types; `nosql-proto`
//! has its wire view ([`ColType`], [`ResultBody`], [`ErrorBody`]). Result metadata follows
//! `org.apache.cassandra.transport.messages.ExecuteMessage` / `QueryMessage` (skip-metadata and
//! v5 `METADATA_CHANGED` rules) and `org.apache.cassandra.cql3.ResultSet`.

use bytes::Bytes;
use nosql_engine as eng;
use nosql_proto as proto;
use nosql_proto::event::{SchemaChangeType, SchemaTarget, StatusChangeType, TopologyChangeType};
use nosql_proto::{ColSpec, ColType, ErrorBody, ErrorDetail, ResultBody, RowsBuilder, RowsMetadata};
use nosql_types::CqlType;

/// The protocol crate's dialect for the engine's.
pub fn proto_dialect(d: nosql_types::Dialect) -> proto::Dialect {
    match d {
        nosql_types::Dialect::Scylla => proto::Dialect::Scylla,
        nosql_types::Dialect::Cassandra => proto::Dialect::Cassandra,
    }
}

pub fn engine_dialect(d: proto::Dialect) -> nosql_types::Dialect {
    match d {
        proto::Dialect::Scylla => nosql_types::Dialect::Scylla,
        proto::Dialect::Cassandra => nosql_types::Dialect::Cassandra,
    }
}

/// The wire type of a column. Reversed clustering order and freezing are invisible on the
/// wire; vectors and `EmptyType` go out as custom marshal classes.
pub fn col_type(ty: &CqlType, dialect: nosql_types::Dialect) -> ColType {
    use CqlType as T;
    match ty {
        T::Ascii => ColType::Ascii,
        T::Bigint => ColType::Bigint,
        T::Blob => ColType::Blob,
        T::Boolean => ColType::Boolean,
        T::Counter => ColType::Counter,
        T::Date => ColType::Date,
        T::Decimal => ColType::Decimal,
        T::Double => ColType::Double,
        T::Duration => ColType::Duration,
        T::Float => ColType::Float,
        T::Inet => ColType::Inet,
        T::Int => ColType::Int,
        T::Smallint => ColType::Smallint,
        T::Text => ColType::Varchar,
        T::Time => ColType::Time,
        T::Timestamp => ColType::Timestamp,
        T::Timeuuid => ColType::Timeuuid,
        T::Tinyint => ColType::Tinyint,
        T::Uuid => ColType::Uuid,
        T::Varint => ColType::Varint,
        T::List { elem, .. } => ColType::list(col_type(elem, dialect)),
        T::Set { elem, .. } => ColType::set(col_type(elem, dialect)),
        T::Map { key, value, .. } => ColType::map(col_type(key, dialect), col_type(value, dialect)),
        T::Tuple(ts) => ColType::Tuple(ts.iter().map(|t| col_type(t, dialect)).collect()),
        T::Udt(def, _) => ColType::Udt {
            keyspace: def.keyspace.clone(),
            name: def.name.clone(),
            fields: def.fields.iter().map(|(n, t)| (n.clone(), col_type(t, dialect))).collect(),
        },
        T::Vector { .. } | T::Empty => ColType::Custom(nosql_types::marshal_name(ty, false, dialect)),
        T::Custom(c) => ColType::Custom(c.to_string()),
    }
}

pub fn col_spec(c: &eng::ColumnSpec, dialect: nosql_types::Dialect) -> ColSpec {
    ColSpec::new(c.keyspace.clone(), c.table.clone(), c.name.clone(), col_type(&c.ty, dialect))
}

pub fn consistency(c: proto::Consistency) -> eng::Consistency {
    use proto::Consistency as P;
    match c {
        P::Any => eng::Consistency::Any,
        P::One => eng::Consistency::One,
        P::Two => eng::Consistency::Two,
        P::Three => eng::Consistency::Three,
        P::Quorum => eng::Consistency::Quorum,
        P::All => eng::Consistency::All,
        P::LocalQuorum => eng::Consistency::LocalQuorum,
        P::EachQuorum => eng::Consistency::EachQuorum,
        P::Serial => eng::Consistency::Serial,
        P::LocalSerial => eng::Consistency::LocalSerial,
        P::LocalOne => eng::Consistency::LocalOne,
        P::NodeLocal => eng::Consistency::NodeLocal,
    }
}

fn wire_consistency(c: eng::Consistency) -> proto::Consistency {
    proto::Consistency::from_code(c.code()).unwrap_or(proto::Consistency::One)
}

pub fn param(v: &proto::Value) -> eng::Param {
    match v {
        proto::Value::Set(b) => eng::Param::Set(b.clone()),
        proto::Value::Null => eng::Param::Null,
        proto::Value::Unset => eng::Param::Unset,
    }
}

pub fn params(p: &proto::QueryParams) -> eng::QueryParams {
    eng::QueryParams {
        consistency: consistency(p.consistency),
        values: p.values.iter().map(param).collect(),
        names: p.names.clone(),
        skip_metadata: p.skip_metadata,
        page_size: p.page_size,
        paging_state: p.paging_state.clone(),
        serial_consistency: p.serial_consistency.map(consistency),
        timestamp: p.timestamp,
        keyspace: p.keyspace.clone(),
        now_in_seconds: p.now_in_seconds,
    }
}

/// A BATCH request. ScyllaDB runs unknown batch types as logged batches (Cassandra rejects
/// them while decoding).
pub fn batch(b: &proto::Batch) -> eng::Batch {
    let kind = match b.kind {
        proto::BatchType::Unlogged => eng::BatchKind::Unlogged,
        proto::BatchType::Counter => eng::BatchKind::Counter,
        proto::BatchType::Logged | proto::BatchType::Other(_) => eng::BatchKind::Logged,
    };
    let statements = b
        .statements
        .iter()
        .map(|s| {
            let values = s.values.iter().map(param).collect();
            match &s.query {
                proto::BatchQuery::Text(text) => eng::BatchStatement::Query { text: text.clone(), values },
                proto::BatchQuery::Prepared(id) => eng::BatchStatement::Prepared { id: id.clone(), values },
            }
        })
        .collect();
    eng::Batch { kind, statements, params: params(&b.params) }
}

pub fn schema_change(c: &eng::SchemaChange) -> proto::SchemaChange {
    let change = match c.change {
        eng::ChangeType::Created => SchemaChangeType::Created,
        eng::ChangeType::Updated => SchemaChangeType::Updated,
        eng::ChangeType::Dropped => SchemaChangeType::Dropped,
    };
    let target = match c.target {
        eng::ChangeTarget::Keyspace => SchemaTarget::Keyspace,
        eng::ChangeTarget::Table => SchemaTarget::Table,
        eng::ChangeTarget::Type => SchemaTarget::Type,
        eng::ChangeTarget::Function => SchemaTarget::Function,
        eng::ChangeTarget::Aggregate => SchemaTarget::Aggregate,
    };
    proto::SchemaChange { change, target, keyspace: c.keyspace.clone(), name: c.name.clone(), arg_types: c.arg_types.clone() }
}

pub fn event(e: &eng::Event) -> Option<proto::Event> {
    Some(match e {
        eng::Event::SchemaChange(c) => proto::Event::SchemaChange(schema_change(c)),
        eng::Event::TopologyChange { change, address } => {
            let change = match *change {
                "NEW_NODE" => TopologyChangeType::NewNode,
                "REMOVED_NODE" => TopologyChangeType::RemovedNode,
                "MOVED_NODE" => TopologyChangeType::MovedNode,
                _ => return None,
            };
            proto::Event::TopologyChange { change, node: *address }
        }
        eng::Event::StatusChange { change, address } => {
            let change = match *change {
                "UP" => StatusChangeType::Up,
                "DOWN" => StatusChangeType::Down,
                _ => return None,
            };
            proto::Event::StatusChange { change, node: *address }
        }
    })
}

/// Encodes engine rows as a Rows body (cells are already serialised).
pub fn rows_body(rows: &eng::Rows, metadata: RowsMetadata) -> proto::Rows {
    let bytes: usize = rows.rows.iter().flatten().map(|c| 4 + c.as_ref().map_or(0, Bytes::len)).sum();
    let mut b = RowsBuilder::with_capacity(bytes);
    for row in &rows.rows {
        for cell in row {
            b.cell(cell.as_deref());
        }
        b.end_row();
    }
    b.finish(metadata)
}

/// How the rows' metadata goes out.
#[derive(Clone, Copy, Debug, Default)]
pub struct MetadataRule<'a> {
    /// The client asked to skip metadata.
    pub skip: bool,
    /// EXECUTE on a connection with result metadata ids (v5, or ScyllaDB's extension): the
    /// id the client holds.
    pub client_id: Option<&'a [u8]>,
    /// The result of a conditional statement (`Response::conditional`).
    pub conditional: bool,
}

/// The RESULT body of a successful response, with the metadata the client asked for.
pub fn result_body(r: &eng::ResponseBody, dialect: nosql_types::Dialect, rule: MetadataRule) -> ResultBody {
    match r {
        eng::ResponseBody::Void => ResultBody::Void,
        eng::ResponseBody::SetKeyspace(ks) => ResultBody::SetKeyspace(ks.clone()),
        eng::ResponseBody::SchemaChange(c) => ResultBody::SchemaChange(schema_change(c)),
        eng::ResponseBody::Rows(rows) => {
            let columns: Vec<ColSpec> = rows.metadata.columns.iter().map(|c| col_spec(c, dialect)).collect();
            let mut md = RowsMetadata::new(columns);
            md.paging_state = rows.paging_state.clone();
            // An LWT result always carries its metadata and never reports a change
            // (CASSANDRA-13992): its columns depend on whether it applied.
            let md = match rule.client_id {
                Some(client) if !rule.conditional => {
                    let id = rows.metadata.id();
                    if client != id {
                        md.new_metadata_id = Some(Bytes::copy_from_slice(&id));
                        md
                    } else if rule.skip {
                        md.skipped()
                    } else {
                        md
                    }
                }
                Some(_) => md,
                None if rule.skip => md.skipped(),
                None => md,
            };
            ResultBody::Rows(rows_body(rows, md))
        }
    }
}

/// The PREPARED result. `lwt_mark`: the client turned on ScyllaDB's LWT metadata mark.
pub fn prepared(p: &eng::Prepared, dialect: nosql_types::Dialect, lwt_mark: bool) -> ResultBody {
    let bind: Vec<ColSpec> = p.bind.iter().map(|c| col_spec(c, dialect)).collect();
    let mut metadata = proto::PreparedMetadata::new(bind, p.pk_indexes.clone());
    metadata.lwt = lwt_mark && p.is_lwt;
    let result_metadata = match &p.result {
        Some(m) => RowsMetadata::new(m.columns.iter().map(|c| col_spec(c, dialect)).collect()),
        None => RowsMetadata::empty(),
    };
    ResultBody::Prepared(proto::Prepared {
        id: p.id.clone(),
        result_metadata_id: Some(Bytes::copy_from_slice(&p.result_metadata_id)),
        metadata,
        result_metadata,
    })
}

/// The ERROR body of an engine error. ScyllaDB's rate-limit error becomes a configuration
/// error for clients that didn't turn the extension on.
pub fn error_body(e: &eng::Error, features: &proto::Features) -> ErrorBody {
    use eng::ErrorDetail as E;
    let detail = match &e.detail {
        E::None => ErrorDetail::None,
        E::Unavailable { consistency, required, alive } => {
            ErrorDetail::Unavailable { consistency: wire_consistency(*consistency), required: *required, alive: *alive }
        }
        E::WriteTimeout { consistency, received, block_for, write_type, contentions } => ErrorDetail::WriteTimeout {
            consistency: wire_consistency(*consistency),
            received: *received,
            block_for: *block_for,
            write_type: write_type.clone(),
            contentions: *contentions,
        },
        E::ReadTimeout { consistency, received, block_for, data_present } => ErrorDetail::ReadTimeout {
            consistency: wire_consistency(*consistency),
            received: *received,
            block_for: *block_for,
            data_present: *data_present,
        },
        E::ReadFailure { consistency, received, block_for, num_failures, reasons, data_present } => ErrorDetail::ReadFailure {
            consistency: wire_consistency(*consistency),
            received: *received,
            block_for: *block_for,
            num_failures: *num_failures,
            reasons: reasons.clone(),
            data_present: *data_present,
        },
        E::WriteFailure { consistency, received, block_for, num_failures, reasons, write_type } => ErrorDetail::WriteFailure {
            consistency: wire_consistency(*consistency),
            received: *received,
            block_for: *block_for,
            num_failures: *num_failures,
            reasons: reasons.clone(),
            write_type: write_type.clone(),
        },
        E::FunctionFailure { keyspace, function, arg_types } => {
            ErrorDetail::FunctionFailure { keyspace: keyspace.clone(), function: function.clone(), arg_types: arg_types.clone() }
        }
        E::AlreadyExists { keyspace, table } => ErrorDetail::AlreadyExists { keyspace: keyspace.clone(), table: table.clone() },
        E::Unprepared { id } => ErrorDetail::Unprepared { id: id.clone() },
        E::CasWriteUnknown { consistency, received, block_for } => {
            ErrorDetail::CasWriteUnknown { consistency: wire_consistency(*consistency), received: *received, block_for: *block_for }
        }
        E::RateLimit { op_type, rejected_by_coordinator } => match features.rate_limit_code {
            Some(code) => {
                let detail = ErrorDetail::RateLimit { op_type: *op_type, rejected_by_coordinator: *rejected_by_coordinator };
                return ErrorBody::with(code, e.message.clone(), detail);
            }
            None => return ErrorBody::new(proto::code::CONFIG_ERROR, e.message.clone()),
        },
    };
    let code = match e.code {
        eng::ErrorCode::RateLimit => proto::code::CONFIG_ERROR,
        c => c.code(),
    };
    ErrorBody::with(code, e.message.clone(), detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn types() {
        let d = nosql_types::Dialect::Cassandra;
        assert_eq!(col_type(&CqlType::Text, d), ColType::Varchar);
        let m = CqlType::map(CqlType::Int, CqlType::list(CqlType::Text, true), false);
        assert_eq!(col_type(&m, d), ColType::map(ColType::Int, ColType::list(ColType::Varchar)));
        let v = CqlType::Vector { elem: Arc::new(CqlType::Float), dim: 3 };
        // Cassandra's `TypeParser.stringifyVectorParameters` spacing.
        assert_eq!(
            col_type(&v, d),
            ColType::Custom("org.apache.cassandra.db.marshal.VectorType(org.apache.cassandra.db.marshal.FloatType , 3)".into())
        );
        let udt = CqlType::udt(nosql_types::UdtDef::new("ks", "u", vec![("a".into(), CqlType::Int)]), true);
        assert_eq!(col_type(&udt, d), ColType::Udt { keyspace: "ks".into(), name: "u".into(), fields: vec![("a".into(), ColType::Int)] });
        assert_eq!(col_type(&CqlType::Tuple(vec![CqlType::Int, CqlType::Blob]), d), ColType::Tuple(vec![ColType::Int, ColType::Blob]));
    }

    #[test]
    fn consistency_levels_map_one_to_one() {
        for c in proto::Consistency::ALL_LEVELS {
            let e = consistency(c);
            assert_eq!(e.code(), c.code());
            assert_eq!(wire_consistency(e), c);
        }
    }

    #[test]
    fn errors() {
        let f = proto::Features::default();
        let e = eng::Error::already_exists("Cannot add existing keyspace \"ks\"", "ks", "");
        let b = error_body(&e, &f);
        assert_eq!(b.code, proto::code::ALREADY_EXISTS);
        assert_eq!(b.detail, ErrorDetail::AlreadyExists { keyspace: "ks".into(), table: "".into() });
        let rl = eng::Error {
            detail: eng::ErrorDetail::RateLimit { op_type: 1, rejected_by_coordinator: true },
            ..eng::Error::new(eng::ErrorCode::RateLimit, "Per-partition rate limit reached")
        };
        assert_eq!(error_body(&rl, &f).code, proto::code::CONFIG_ERROR);
        let on = proto::Features { rate_limit_code: Some(proto::code::SCYLLA_RATE_LIMIT), ..Default::default() };
        let b = error_body(&rl, &on);
        assert_eq!((b.code, b.detail), (0xF000, ErrorDetail::RateLimit { op_type: 1, rejected_by_coordinator: true }));
        let u = eng::Error::unprepared(&[1, 2]);
        assert_eq!(error_body(&u, &f).detail, ErrorDetail::Unprepared { id: Bytes::from_static(&[1, 2]) });
    }

    fn rows() -> eng::ResponseBody {
        eng::ResponseBody::Rows(eng::Rows {
            metadata: eng::ResultMetadata { columns: vec![eng::ColumnSpec::new("ks", "t", "k", CqlType::Int)] },
            rows: vec![vec![Some(Bytes::from_static(&[0, 0, 0, 1]))], vec![None]],
            paging_state: Some(Bytes::from_static(b"p")),
        })
    }

    #[test]
    fn metadata_rules() {
        let d = nosql_types::Dialect::Cassandra;
        let ResultBody::Rows(r) = result_body(&rows(), d, MetadataRule::default()) else { panic!() };
        assert_eq!((r.row_count, r.metadata.columns.len(), r.metadata.no_metadata), (2, 1, false));
        assert_eq!(r.to_vec(), vec![vec![Some(Bytes::from_static(&[0, 0, 0, 1]))], vec![None]]);
        assert_eq!(r.metadata.paging_state.as_deref(), Some(&b"p"[..]));
        let ResultBody::Rows(r) = result_body(&rows(), d, MetadataRule { skip: true, ..Default::default() }) else { panic!() };
        assert!(r.metadata.no_metadata);
        assert_eq!(r.metadata.column_count, 1);
        // v5: a stale id gets the new one and full metadata; the current one honours skip.
        let eng::ResponseBody::Rows(er) = rows() else { panic!() };
        let id = er.metadata.id();
        let ResultBody::Rows(r) = result_body(&rows(), d, MetadataRule { skip: true, client_id: Some(b"stale"), ..Default::default() }) else { panic!() };
        assert_eq!(r.metadata.new_metadata_id.as_deref(), Some(&id[..]));
        assert!(!r.metadata.no_metadata);
        let ResultBody::Rows(r) = result_body(&rows(), d, MetadataRule { skip: true, client_id: Some(&id), ..Default::default() }) else { panic!() };
        assert!(r.metadata.no_metadata && r.metadata.new_metadata_id.is_none());
    }
}
