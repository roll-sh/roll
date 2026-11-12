//! Cassandra 5's collection functions (`CollectionFcts`, Apache-2.0): `map_keys`, `map_values`,
//! `collection_count`, and `collection_min` / `max` / `sum` / `avg`, which feed the elements of
//! one collection to the native aggregate of the element type (`min(x)` over rows and
//! `collection_min(c)` over elements behave the same). ScyllaDB has none of them.

use std::sync::Arc;

use bytes::Bytes;
use nosql_types::CqlType;

use super::aggregates::{Agg, Kind};
use super::params::{Param, ParamFactory};
use super::util::arg;
use super::{FnCtx, FnName, Function, Registry};
use crate::error::{Error, Result};

pub fn register(r: &mut Registry) {
    if r.dialect().is_scylla() {
        return;
    }
    r.add_factory(
        "map_keys",
        ParamFactory::new("map_keys", vec![Param::AnyMap], |n, t, _| {
            build(n, t, Op::Keys)
        }),
    );
    r.add_factory(
        "map_values",
        ParamFactory::new("map_values", vec![Param::AnyMap], |n, t, _| {
            build(n, t, Op::Values)
        }),
    );
    r.add_factory(
        "collection_count",
        ParamFactory::new("collection_count", vec![Param::AnyCollection], |n, t, _| {
            build(n, t, Op::Count)
        }),
    );
    for (name, param, kind) in [
        ("collection_min", Param::SetOrList, Kind::Min),
        ("collection_max", Param::SetOrList, Kind::Max),
        ("collection_sum", Param::NumericSetOrList, Kind::Sum),
        ("collection_avg", Param::NumericSetOrList, Kind::Avg),
    ] {
        let build: super::params::Build = match kind {
            Kind::Min => |n, t, _| build(n, t, Op::Agg(Kind::Min)),
            Kind::Max => |n, t, _| build(n, t, Op::Agg(Kind::Max)),
            Kind::Sum => |n, t, _| build(n, t, Op::Agg(Kind::Sum)),
            _ => |n, t, _| build(n, t, Op::Agg(Kind::Avg)),
        };
        r.add_factory(name, ParamFactory::new(name, vec![param], build));
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Keys,
    Values,
    Count,
    Agg(Kind),
}

fn build(name: &str, types: &[CqlType], op: Op) -> Result<Arc<dyn Function>> {
    let arg = types.first().cloned().unwrap_or(CqlType::Blob);
    let ret = match (op, &arg) {
        (Op::Keys, CqlType::Map { key, .. }) => CqlType::set((**key).clone(), true),
        (Op::Values, CqlType::Map { value, .. }) => CqlType::list((**value).clone(), true),
        (Op::Keys | Op::Values, other) => {
            // `anyMap` lets a UDT through; the factory then casts it to a map.
            return Err(Error::server(format!(
                "java.lang.ClassCastException: class {} cannot be cast to class org.apache.cassandra.db.marshal.MapType ({} and org.apache.cassandra.db.marshal.MapType are in unnamed module of loader 'app')",
                marshal_class(other),
                marshal_class(other)
            )));
        }
        (Op::Count, _) => CqlType::Int,
        (Op::Agg(_), CqlType::List { elem, .. } | CqlType::Set { elem, .. }) => (**elem).clone(),
        (Op::Agg(_), _) => CqlType::Blob,
    };
    Ok(Arc::new(CollectionFn {
        name: FnName::system(name),
        args: vec![arg],
        ret,
        op,
    }))
}

fn marshal_class(t: &CqlType) -> &'static str {
    match t {
        CqlType::Udt(..) => "org.apache.cassandra.db.marshal.UserType",
        _ => "org.apache.cassandra.db.marshal.AbstractType",
    }
}

#[derive(Debug)]
struct CollectionFn {
    name: FnName,
    args: Vec<CqlType>,
    ret: CqlType,
    op: Op,
}

impl Function for CollectionFn {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.ret
    }
    fn null_on_null_input(&self) -> bool {
        true
    }
    fn execute(&self, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        let Some(b) = arg(args, 0) else {
            return Ok(None);
        };
        let bad = |_| Error::invalid("Invalid collection value");
        Ok(Some(match self.op {
            Op::Keys | Op::Values => {
                let entries = nosql_types::map_entries(b).map_err(bad)?;
                let items: Vec<&[u8]> = entries
                    .iter()
                    .map(|(k, v)| match self.op {
                        Op::Keys => *k,
                        _ => *v,
                    })
                    .collect();
                nosql_types::pack_collection(items)
            }
            Op::Count => {
                let n = match &self.args[0] {
                    CqlType::Map { .. } => nosql_types::map_entries(b).map_err(bad)?.len(),
                    _ => nosql_types::collection_elements(b).map_err(bad)?.len(),
                };
                Bytes::copy_from_slice(&(n as i32).to_be_bytes())
            }
            Op::Agg(kind) => {
                let agg = Agg {
                    name: FnName::system(match kind {
                        Kind::Min => "min",
                        Kind::Max => "max",
                        Kind::Sum => "sum",
                        _ => "avg",
                    }),
                    args: vec![self.ret.clone()],
                    ret: self.ret.clone(),
                    kind,
                };
                let Some(mut acc) = agg.new_aggregate() else {
                    return Ok(None);
                };
                for e in nosql_types::collection_elements(b).map_err(bad)? {
                    acc.add(ctx, &[e.map(Bytes::copy_from_slice)])?;
                }
                return acc.compute(ctx);
            }
        }))
    }
}
