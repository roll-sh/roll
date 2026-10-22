//! General native functions: blob conversions (`BytesConversionFcts`), `token`, `uuid`,
//! `toJson` / `fromJson` (`FromJsonFct`, `ToJsonFct`) and vector similarity, following Cassandra
//! (Apache-2.0) and ScyllaDB's names and messages.

use std::sync::Arc;

use bytes::Bytes;
use nosql_types::{CqlType, Dialect, NATIVE_TYPES};

use super::{ArgInfo, Factory, FnCtx, FnName, Function, Native, Registry, util::arg};
use crate::error::{Error, Result};

pub fn register(r: &mut Registry) {
    let d = r.dialect();
    for t in NATIVE_TYPES
        .iter()
        .filter(|t| !matches!(t, CqlType::Blob | CqlType::Empty))
    {
        let name = t.native_name().unwrap_or_default();
        let arg = t.clone();
        let mut names = vec![(format!("{name}asblob"), format!("blobas{name}"))];
        if name == "text" {
            names.push(("varcharasblob".into(), "blobasvarchar".into()));
        }
        if !d.is_scylla() {
            names.push((format!("{name}_as_blob"), format!("blob_as_{name}")));
            if name == "text" {
                names.push(("varchar_as_blob".into(), "blob_as_varchar".into()));
            }
        }
        for (to, from) in names {
            r.add(Arc::new(ToBlob {
                name: FnName::system(&to),
                args: vec![arg.clone()],
            }));
            r.add(Arc::new(FromBlob {
                name: FnName::system(&from),
                args: vec![CqlType::Blob],
                ty: t.clone(),
            }));
        }
    }
    r.add(Native::nondeterministic(
        "uuid",
        vec![],
        CqlType::Uuid,
        |_, _| {
            Ok(Some(Bytes::copy_from_slice(
                &nosql_types::timeuuid::random_uuid(),
            )))
        },
    ));
    r.add_factory("token", Arc::new(TokenFactory));
    r.add_factory("tojson", Arc::new(ToJsonFactory("tojson")));
    r.add_factory("fromjson", Arc::new(FromJsonFactory("fromjson")));
    // Cassandra 5's names (`ToJsonFct` / `FromJsonFct` register both).
    if !d.is_scylla() {
        r.add_factory("to_json", Arc::new(ToJsonFactory("to_json")));
        r.add_factory("from_json", Arc::new(FromJsonFactory("from_json")));
    }
    for name in [
        "similarity_cosine",
        "similarity_euclidean",
        "similarity_dot_product",
    ] {
        r.add_factory(name, Arc::new(SimilarityFactory(name)));
    }
}

#[derive(Debug)]
struct ToBlob {
    name: FnName,
    args: Vec<CqlType>,
}

impl Function for ToBlob {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &CqlType::Blob
    }
    fn null_on_null_input(&self) -> bool {
        true
    }
    fn execute(&self, _: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        Ok(arg(args, 0).cloned())
    }
}

#[derive(Debug)]
struct FromBlob {
    name: FnName,
    args: Vec<CqlType>,
    ty: CqlType,
}

impl Function for FromBlob {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.ty
    }
    fn null_on_null_input(&self) -> bool {
        true
    }
    fn execute(&self, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        let Some(v) = arg(args, 0) else {
            return Ok(None);
        };
        let check_ty = if self.ty.is_counter() {
            CqlType::Bigint
        } else {
            self.ty.clone()
        };
        if nosql_types::validate(&check_ty, v, ctx.dialect).is_err() {
            let shown = match ctx.dialect {
                Dialect::Scylla => self.name.name.clone(),
                Dialect::Cassandra => self.name.to_string(),
            };
            let hex: String = v.iter().map(|b| format!("{b:02x}")).collect();
            return Err(Error::invalid(format!(
                "In call to function {shown}, value 0x{hex} is not a valid binary representation for type {}",
                self.ty.native_name().unwrap_or_default()
            )));
        }
        Ok(Some(v.clone()))
    }
}

/// `token(…)` typed as a table's partition key (see `Registry::resolve_token`).
pub fn token_function(partition_key: &[CqlType]) -> Arc<dyn Function> {
    Arc::new(Token {
        name: FnName::system("token"),
        args: partition_key.to_vec(),
    })
}

/// `token(…)`: the partition token of its arguments, serialised as a partition key.
struct TokenFactory;

#[derive(Debug)]
struct Token {
    name: FnName,
    args: Vec<CqlType>,
}

impl Factory for TokenFactory {
    fn create(
        &self,
        args: &[&dyn ArgInfo],
        _r: Option<&CqlType>,
        _d: Dialect,
    ) -> Result<Option<Arc<dyn Function>>> {
        let types: Vec<CqlType> = args
            .iter()
            .map(|a| a.exact_type().unwrap_or(CqlType::Blob))
            .collect();
        Ok(Some(Arc::new(Token {
            name: FnName::system("token"),
            args: types,
        })))
    }
}

impl Function for Token {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &CqlType::Bigint
    }
    fn null_on_null_input(&self) -> bool {
        true
    }
    fn execute(&self, cx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        let parts: Vec<&[u8]> = args.iter().map(|a| a.as_deref().unwrap_or(&[])).collect();
        let key = nosql_types::partition_key(&parts).map_err(|e| Error::invalid(e.message))?;
        Ok(Some(Bytes::copy_from_slice(
            &crate::storage::token_of(&key, cx.dialect).to_be_bytes(),
        )))
    }
}

/// `toJson`, under the name it was called by.
struct ToJsonFactory(&'static str);

#[derive(Debug)]
struct ToJson {
    name: FnName,
    args: Vec<CqlType>,
}

impl Factory for ToJsonFactory {
    fn create(
        &self,
        args: &[&dyn ArgInfo],
        _r: Option<&CqlType>,
        d: Dialect,
    ) -> Result<Option<Arc<dyn Function>>> {
        if args.len() != 1 {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "toJson() accepts 1 argument only".to_string(),
                Dialect::Cassandra => {
                    format!("Invalid number of arguments for function system.{}(any)", self.0)
                }
            }));
        }
        let Some(ty) = args[0].exact_type() else {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "Could not infer type of argument to toJson()".to_string(),
                Dialect::Cassandra => format!(
                    "Cannot infer type of argument {} in call to function system.{}: use type casts to disambiguate",
                    args[0].text(),
                    self.0
                ),
            }));
        };
        Ok(Some(Arc::new(ToJson {
            name: FnName::system(self.0),
            args: vec![ty],
        })))
    }
}

impl Function for ToJson {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &CqlType::Text
    }
    fn execute(&self, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        let s = match arg(args, 0) {
            None => "null".to_string(),
            Some(b) => nosql_types::to_json(&self.args[0], b, ctx.dialect)
                .map_err(|e| Error::invalid(e.message))?,
        };
        Ok(Some(Bytes::from(s)))
    }
}

/// `fromJson`, under the name it was called by.
struct FromJsonFactory(&'static str);

#[derive(Debug)]
struct FromJson {
    name: FnName,
    args: Vec<CqlType>,
    ret: CqlType,
}

impl Factory for FromJsonFactory {
    fn create(
        &self,
        args: &[&dyn ArgInfo],
        receiver: Option<&CqlType>,
        d: Dialect,
    ) -> Result<Option<Arc<dyn Function>>> {
        if args.len() != 1 {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "fromJson() accepts 1 argument only".to_string(),
                Dialect::Cassandra => {
                    format!("Invalid number of arguments for function system.{}(text)", self.0)
                }
            }));
        }
        let Some(ret) = receiver else {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "fromJson() can only be called if receiver type is known".to_string(),
                Dialect::Cassandra => {
                    format!("{}() cannot be used in the selection clause of a SELECT statement", self.0)
                }
            }));
        };
        Ok(Some(Arc::new(FromJson {
            name: FnName::system(self.0),
            args: vec![CqlType::Text],
            ret: ret.clone(),
        })))
    }
}

impl Function for FromJson {
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
        let text = std::str::from_utf8(b)
            .map_err(|_| Error::invalid("Invalid UTF-8 in fromJson() argument"))?;
        let j = nosql_types::parse_json(text, ctx.dialect)
            .map_err(|e| self.failure(ctx.dialect, e.message))?;
        if matches!(j, nosql_types::Json::Null) {
            return Ok(None);
        }
        nosql_types::from_json(&self.ret, &j, ctx.dialect)
            .map(Some)
            .map_err(|e| self.failure(ctx.dialect, e.message))
    }
}

impl FromJson {
    fn failure(&self, d: Dialect, message: String) -> Error {
        match d {
            Dialect::Scylla => Error::function_failure(
                format!(
                    "execution of fromjson failed: Failed execution of function system.fromjson: marshal_exception ({message})"
                ),
                "system",
                "fromjson",
                vec!["org.apache.cassandra.db.marshal.UTF8Type".into()],
            ),
            Dialect::Cassandra => Error::function_failure(
                format!("org.apache.cassandra.serializers.MarshalException: {message}"),
                "system",
                &self.name.name,
                vec!["text".into()],
            ),
        }
    }
}

/// `similarity_*(vector<float, n>, vector<float, n>)`.
struct SimilarityFactory(&'static str);

#[derive(Debug)]
struct Similarity {
    name: FnName,
    args: Vec<CqlType>,
}

impl Factory for SimilarityFactory {
    fn create(
        &self,
        args: &[&dyn ArgInfo],
        _r: Option<&CqlType>,
        d: Dialect,
    ) -> Result<Option<Arc<dyn Function>>> {
        let name = self.0;
        let sig = format!("system.{name}(vector<float, n>, vector<float, n>)");
        if args.len() != 2 {
            return Err(Error::invalid(format!(
                "Invalid number of arguments for function {sig}"
            )));
        }
        let requires = |a: &dyn ArgInfo, ty: Option<CqlType>| {
            let shown = |t: &CqlType| nosql_types::cql_name_in(t, d);
            Error::invalid(match (d, ty) {
                (Dialect::Scylla, Some(t)) => format!(
                    "Function system.{name} requires a float vector argument, but found {} of type {}",
                    a.scylla_text(),
                    shown(&t)
                ),
                (Dialect::Scylla, None) => format!(
                    "Function system.{name} requires a float vector argument, but found {}",
                    a.scylla_text()
                ),
                (Dialect::Cassandra, t) => format!(
                    "Function system.{name} requires a float vector argument, but found argument {} of type {}",
                    a.text(),
                    t.map(|t| shown(&t)).unwrap_or_default()
                ),
            })
        };
        // `FunctionParameter.vector`: a typed argument gives its dimension, an array literal
        // its length (ScyllaDB: when its elements are numbers).
        let mut dims: Vec<Option<u32>> = Vec::with_capacity(2);
        for a in args {
            let a = *a;
            let dim = if a.is_marker() || a.is_null() {
                None
            } else if let Some(n) = a.array_len() {
                match (d, a.scylla_type()) {
                    (Dialect::Cassandra, _) => Some(n as u32),
                    (Dialect::Scylla, Some(CqlType::List { elem, .. }))
                        if matches!(
                            *elem,
                            CqlType::Int
                                | CqlType::Bigint
                                | CqlType::Varint
                                | CqlType::Smallint
                                | CqlType::Tinyint
                                | CqlType::Float
                                | CqlType::Double
                                | CqlType::Decimal
                        ) =>
                    {
                        Some(n as u32)
                    }
                    (Dialect::Scylla, Some(t)) => return Err(requires(a, Some(t))),
                    (Dialect::Scylla, None) => return Err(requires(a, None)),
                }
            } else {
                // Cassandra types a set or tuple literal by its compatible type.
                let ty = if d.is_scylla() {
                    a.scylla_type()
                } else {
                    a.exact_type().or_else(|| a.compatible_type())
                };
                match ty {
                    Some(CqlType::Vector { elem, dim }) if *elem == CqlType::Float => Some(dim),
                    Some(t) => return Err(requires(a, Some(t))),
                    None => None,
                }
            };
            dims.push(dim);
        }
        if let [Some(x), Some(y)] = dims[..]
            && x != y
        {
            return Err(Error::invalid(match d {
                Dialect::Scylla => format!(
                    "All arguments must have the same vector dimensions, but found vector<float, {x}> and vector<float, {y}>"
                ),
                Dialect::Cassandra => {
                    "All arguments must have the same vector dimensions".to_string()
                }
            }));
        }
        let Some(dim) = dims.iter().flatten().next().copied() else {
            let a = args[0];
            return Err(Error::invalid(match d {
                Dialect::Scylla => format!(
                    "Cannot infer type of argument {} for function {sig}",
                    a.scylla_text()
                ),
                Dialect::Cassandra => format!(
                    "Cannot infer type of argument {} in call to function {sig}: use type casts to disambiguate",
                    a.text()
                ),
            }));
        };
        let vt = CqlType::vector(CqlType::Float, dim);
        Ok(Some(Arc::new(Similarity {
            name: FnName::system(self.0),
            args: vec![vt.clone(), vt],
        })))
    }
}

impl Function for Similarity {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &CqlType::Float
    }
    fn null_on_null_input(&self) -> bool {
        true
    }
    fn execute(&self, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        let (Some(a), Some(b)) = (arg(args, 0), arg(args, 1)) else {
            return Ok(None);
        };
        let floats = |v: &[u8]| -> Vec<f32> {
            v.chunks_exact(4)
                .map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        };
        let (x, y) = (floats(a), floats(b));
        // ScyllaDB computes in double precision and rounds the result to float.
        if ctx.dialect.is_scylla() {
            let (x, y): (Vec<f64>, Vec<f64>) = (
                x.iter().map(|v| *v as f64).collect(),
                y.iter().map(|v| *v as f64).collect(),
            );
            let dot: f64 = x.iter().zip(&y).map(|(p, q)| p * q).sum();
            let r = match self.name.name.as_str() {
                "similarity_cosine" => {
                    // One square root of the product (exactly 0 for opposite vectors).
                    let nx: f64 = x.iter().map(|p| p * p).sum();
                    let ny: f64 = y.iter().map(|p| p * p).sum();
                    if nx == 0.0 || ny == 0.0 {
                        f64::NAN
                    } else {
                        (1.0 + dot / (nx * ny).sqrt()) / 2.0
                    }
                }
                "similarity_euclidean" => {
                    let d2: f64 = x.iter().zip(&y).map(|(p, q)| (p - q) * (p - q)).sum();
                    1.0 / (1.0 + d2)
                }
                _ => (1.0 + dot) / 2.0,
            };
            return Ok(Some(Bytes::copy_from_slice(&(r as f32).to_be_bytes())));
        }
        let dot: f32 = x.iter().zip(&y).map(|(p, q)| p * q).sum();
        let r = match self.name.name.as_str() {
            "similarity_cosine" => {
                let nx: f32 = x.iter().map(|p| p * p).sum::<f32>().sqrt();
                let ny: f32 = y.iter().map(|p| p * p).sum::<f32>().sqrt();
                if nx == 0.0 || ny == 0.0 {
                    if !ctx.dialect.is_scylla() {
                        return Err(Error::invalid(
                            "Function system.similarity_cosine doesn't support all-zero vectors.",
                        ));
                    }
                    f32::NAN
                } else {
                    (1.0 + dot / (nx * ny)) / 2.0
                }
            }
            "similarity_euclidean" => {
                let d2: f32 = x.iter().zip(&y).map(|(p, q)| (p - q) * (p - q)).sum();
                1.0 / (1.0 + d2)
            }
            _ => (1.0 + dot) / 2.0,
        };
        Ok(Some(Bytes::copy_from_slice(&r.to_be_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(d: Dialect) -> FnCtx {
        FnCtx {
            dialect: d,
            protocol_version: 4,
        }
    }

    #[test]
    fn blob_conversions() {
        let r = Registry::new(Dialect::Cassandra);
        let blob = super::super::Typed {
            ty: CqlType::Blob,
            text: "b".into(),
        };
        let f = r
            .resolve(None, "blobasint", &[&blob], None, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            f.execute(
                &ctx(Dialect::Cassandra),
                &[Some(Bytes::from_static(&[0, 0, 0, 1]))]
            )
            .unwrap()
            .unwrap()[..],
            [0, 0, 0, 1]
        );
        let e = f
            .execute(
                &ctx(Dialect::Cassandra),
                &[Some(Bytes::from_static(&[1, 2]))],
            )
            .unwrap_err();
        assert_eq!(
            e.message,
            "In call to function system.blobasint, value 0x0102 is not a valid binary representation for type int"
        );
        let e = f
            .execute(&ctx(Dialect::Scylla), &[Some(Bytes::from_static(&[1, 2]))])
            .unwrap_err();
        assert_eq!(
            e.message,
            "In call to function blobasint, value 0x0102 is not a valid binary representation for type int"
        );
        let t = r
            .resolve(None, "blobastext", &[&blob], None, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            t.execute(&ctx(Dialect::Scylla), &[Some(Bytes::from_static(&[0xff]))])
                .unwrap_err()
                .message,
            "In call to function blobastext, value 0xff is not a valid binary representation for type text"
        );
        // An empty bigint is a valid (empty) value.
        let b = r
            .resolve(None, "blobasbigint", &[&blob], None, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            b.execute(&ctx(Dialect::Scylla), &[Some(Bytes::new())])
                .unwrap(),
            Some(Bytes::new())
        );
        assert!(r.exists("int_as_blob"));
        assert!(!Registry::new(Dialect::Scylla).exists("int_as_blob"));
    }

    #[test]
    fn token_and_json() {
        let r = Registry::new(Dialect::Scylla);
        let pk = super::super::Typed {
            ty: CqlType::Int,
            text: "pk".into(),
        };
        let t = r
            .resolve(None, "token", &[&pk], None, None)
            .unwrap()
            .unwrap();
        // Captured: token(1) for an int key.
        assert_eq!(
            t.execute(
                &ctx(Dialect::Scylla),
                &[Some(Bytes::copy_from_slice(&1i32.to_be_bytes()))]
            )
            .unwrap()
            .unwrap()[..],
            (-4069959284402364209i64).to_be_bytes()
        );
        let j = r
            .resolve(None, "tojson", &[&pk], None, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            j.execute(&ctx(Dialect::Scylla), &[None]).unwrap().unwrap(),
            "null"
        );
        assert_eq!(
            r.resolve(None, "tojson", &[&pk, &pk], None, None)
                .unwrap_err()
                .message,
            "toJson() accepts 1 argument only"
        );
        let txt = super::super::Typed {
            ty: CqlType::Text,
            text: "'x'".into(),
        };
        assert_eq!(
            r.resolve(None, "fromjson", &[&txt], None, None)
                .unwrap_err()
                .message,
            "fromJson() can only be called if receiver type is known"
        );
        let f = r
            .resolve(None, "fromjson", &[&txt], Some(&CqlType::Int), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            f.execute(&ctx(Dialect::Scylla), &[Some(Bytes::from_static(b"5"))])
                .unwrap()
                .unwrap()[..],
            5i32.to_be_bytes()
        );
        let e = f
            .execute(
                &ctx(Dialect::Cassandra),
                &[Some(Bytes::from_static(b"\"x\""))],
            )
            .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::FunctionFailure);
        assert_eq!(
            e.message,
            "org.apache.cassandra.serializers.MarshalException: Unable to make int from 'x'"
        );
    }
}
