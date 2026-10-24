//! ScyllaDB-only native functions (behaviour observed on `scylladb/scylla:2026.3.3`; ScyllaDB's
//! source is not consulted): `set_intersection(set, set)`, the elements of the first set that the
//! second also holds. Both arguments must have the same set type (a bind marker takes the other
//! argument's); the result is a frozen set of that type.

use std::sync::Arc;

use bytes::Bytes;
use nosql_types::{CqlType, Dialect};

use super::util::arg;
use super::{ArgInfo, Factory, FnCtx, FnName, Function, Registry};
use crate::error::{Error, Result};

pub fn register(r: &mut Registry) {
    if r.dialect().is_scylla() {
        r.add_factory("set_intersection", Arc::new(SetIntersectionFactory));
    }
}

/// `t` with every nested collection frozen (as values inside a collection are).
fn frozen_deep(t: &CqlType) -> CqlType {
    let inner = |e: &CqlType| {
        let e = frozen_deep(e);
        if e.is_collection() { e.freeze() } else { e }
    };
    match t {
        CqlType::List { elem, frozen } => CqlType::list(inner(elem), *frozen),
        CqlType::Set { elem, frozen } => CqlType::set(inner(elem), *frozen),
        CqlType::Map { key, value, frozen } => CqlType::map(inner(key), inner(value), *frozen),
        other => other.clone(),
    }
}

struct SetIntersectionFactory;

impl Factory for SetIntersectionFactory {
    fn create(
        &self,
        args: &[&dyn ArgInfo],
        _r: Option<&CqlType>,
        d: Dialect,
    ) -> Result<Option<Arc<dyn Function>>> {
        if args.len() != 2 {
            return Err(Error::invalid(
                "set_intersection() accepts 2 arguments only",
            ));
        }
        // ScyllaDB types a literal argument by its own inference (string keys are `text`).
        let types: Vec<Option<CqlType>> = args
            .iter()
            .map(|a| {
                a.scylla_type()
                    .or_else(|| a.compatible_type())
                    .map(|t| frozen_deep(&t))
            })
            .collect();
        if types.iter().all(Option::is_none) {
            return Err(Error::invalid(
                "set_intersection() can only be called if at least one argument type is known",
            ));
        }
        if types
            .iter()
            .flatten()
            .any(|t| !matches!(t, CqlType::Set { .. }))
        {
            return Err(Error::invalid(
                "set_intersection() can only be called if both arguments are of set type",
            ));
        }
        let frozen: Vec<CqlType> = types.iter().flatten().map(CqlType::freeze).collect();
        if frozen.len() == 2 && frozen[0] != frozen[1] {
            let names: Vec<String> = frozen
                .iter()
                .map(|t| {
                    format!(
                        "\"org.apache.cassandra.db.marshal.FrozenType({})\"",
                        nosql_types::marshal_name(&t.unfreeze(), false, d)
                    )
                })
                .collect();
            return Err(Error::invalid(format!(
                "set_intersection() can only be called if both arguments are of the same set type: [{}]",
                names.join(", ")
            )));
        }
        let ty = frozen[0].clone();
        let args = types
            .into_iter()
            .map(|t| t.unwrap_or_else(|| ty.clone()))
            .collect();
        Ok(Some(Arc::new(SetIntersection {
            name: FnName::system("set_intersection"),
            args,
            ret: ty,
        })))
    }
}

#[derive(Debug)]
struct SetIntersection {
    name: FnName,
    args: Vec<CqlType>,
    ret: CqlType,
}

impl Function for SetIntersection {
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
    fn execute(&self, _: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        let (Some(a), Some(b)) = (arg(args, 0), arg(args, 1)) else {
            return Ok(None);
        };
        let CqlType::Set { elem, .. } = &self.ret else {
            return Ok(None);
        };
        let bad = |_| Error::invalid("Invalid set value");
        let left = nosql_types::collection_elements(a).map_err(bad)?;
        let right = nosql_types::collection_elements(b).map_err(bad)?;
        let both: Vec<&[u8]> = left
            .into_iter()
            .flatten()
            .filter(|x| {
                right
                    .iter()
                    .flatten()
                    .any(|y| nosql_types::compare(elem, x, y).is_eq())
            })
            .collect();
        Ok(Some(nosql_types::pack_collection(both)))
    }
}
