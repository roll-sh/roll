//! Function factories with loosely typed parameters: Cassandra's `FunctionFactory` and
//! `FunctionParameter` (Apache-2.0), used by the collection and masking functions.
//!
//! A [`ParamFactory`] checks the argument count against its parameters, infers each argument's
//! type in two passes (the second can use the types inferred for the other arguments, as
//! `sameAs` does), validates it, then builds the overload for those types. Messages are
//! Cassandra's, naming the factory as `system.mask_inner([text|varchar|ascii], int, int, [text])`.

use std::sync::Arc;

use nosql_types::{CqlType, Dialect};

use super::{ArgInfo, Assign, Factory, Function, test_type};
use crate::error::{Error, Result};

/// A `FunctionParameter`.
#[derive(Clone, Debug)]
pub enum Param {
    /// `fixed(types…)`: an argument assignable to one of the types (an unknown one is the first).
    Fixed(&'static [CqlType]),
    /// `string()`: `fixed(text, varchar, ascii)`.
    Str,
    /// `anyType(inferFromReceiver)`.
    Any { from_receiver: bool },
    /// `sameAs(index, preferOther, parameter)`.
    SameAs {
        index: usize,
        prefer_other: bool,
        inner: Box<Param>,
    },
    /// `anyCollection()`.
    AnyCollection,
    /// `setOrList()`.
    SetOrList,
    /// `numericSetOrList()`.
    NumericSetOrList,
    /// `anyMap()` (UDTs pass the check, as in Cassandra).
    AnyMap,
    /// `optional(parameter)`.
    Optional(Box<Param>),
}

pub const INT: &[CqlType] = &[CqlType::Int];
pub const TEXT: &[CqlType] = &[CqlType::Text];

impl Param {
    pub fn optional(self) -> Param {
        Param::Optional(Box::new(self))
    }

    fn is_optional(&self) -> bool {
        matches!(self, Param::Optional(_))
    }

    fn display(&self) -> String {
        match self {
            Param::Str => "[text|varchar|ascii]".into(),
            Param::Fixed(types) if types.len() == 1 => nosql_types::cql_name(&types[0]),
            Param::Fixed(types) => format!(
                "[{}]",
                types
                    .iter()
                    .map(nosql_types::cql_name)
                    .collect::<Vec<_>>()
                    .join("|")
            ),
            Param::Any { .. } => "any".into(),
            Param::SameAs { inner, .. } => inner.display(),
            Param::AnyCollection => "collection".into(),
            Param::SetOrList | Param::NumericSetOrList => "numeric_set_or_list".into(),
            Param::AnyMap => "map".into(),
            Param::Optional(p) => format!("[{}]", p.display()),
        }
    }

    fn infer(
        &self,
        arg: &dyn ArgInfo,
        receiver: Option<&CqlType>,
        inferred: Option<&[Option<CqlType>]>,
    ) -> Option<CqlType> {
        match self {
            Param::Fixed(types) => arg.compatible_type().or_else(|| types.first().cloned()),
            Param::Str => arg.compatible_type().or(Some(CqlType::Text)),
            Param::Any { from_receiver } => arg
                .compatible_type()
                .or_else(|| receiver.filter(|_| *from_receiver).cloned()),
            Param::SameAs {
                index,
                prefer_other,
                inner,
            } => {
                let other = inferred.and_then(|t| t.get(*index).cloned().flatten());
                if *prefer_other {
                    other.or_else(|| inner.infer(arg, receiver, inferred))
                } else {
                    inner.infer(arg, receiver, inferred).or(other)
                }
            }
            Param::Optional(p) => p.infer(arg, receiver, inferred),
            _ => arg.compatible_type(),
        }
    }

    fn validate(&self, name: &str, arg: &dyn ArgInfo, ty: &CqlType) -> Result<()> {
        let found = |what: String| {
            Error::invalid(format!(
                "Function {name} requires {what}, but found argument {} of type {}",
                arg.text(),
                nosql_types::cql_name(ty)
            ))
        };
        match self {
            Param::Str => {
                if [CqlType::Text, CqlType::Ascii]
                    .iter()
                    .all(|t| test_type(ty, t) == Assign::No)
                {
                    return Err(found(format!("an argument of type {}", self.display())));
                }
            }
            Param::Fixed(types) => {
                if types.iter().all(|t| test_type(ty, t) == Assign::No) {
                    return Err(found(format!("an argument of type {}", self.display())));
                }
            }
            Param::SameAs { inner, .. } | Param::Optional(inner) => {
                return inner.validate(name, arg, ty);
            }
            Param::Any { .. } => {}
            Param::AnyCollection => {
                if !ty.is_collection() {
                    return Err(found("a collection argument".into()));
                }
            }
            Param::SetOrList => {
                if !matches!(ty, CqlType::List { .. } | CqlType::Set { .. }) {
                    return Err(found("a set or list argument".into()));
                }
            }
            Param::NumericSetOrList => {
                let numeric = match ty {
                    CqlType::List { elem, .. } | CqlType::Set { elem, .. } => is_number(elem),
                    _ => false,
                };
                if !numeric {
                    return Err(found("a numeric set/list argument".into()));
                }
            }
            Param::AnyMap => {
                if !matches!(ty, CqlType::Map { .. } | CqlType::Udt(..)) {
                    return Err(found("a map argument".into()));
                }
            }
        }
        Ok(())
    }
}

/// Cassandra's `NumberType`s.
pub fn is_number(t: &CqlType) -> bool {
    matches!(
        t,
        CqlType::Tinyint
            | CqlType::Smallint
            | CqlType::Int
            | CqlType::Bigint
            | CqlType::Float
            | CqlType::Double
            | CqlType::Varint
            | CqlType::Decimal
            | CqlType::Counter
    )
}

/// Builds the overload for the inferred argument types (`doGetOrCreateFunction`).
pub type Build = fn(&str, &[CqlType], Option<&CqlType>) -> Result<Arc<dyn Function>>;

/// A `FunctionFactory` of `system.<name>`.
pub struct ParamFactory {
    pub name: &'static str,
    pub params: Vec<Param>,
    pub build: Build,
}

impl ParamFactory {
    pub fn new(name: &'static str, params: Vec<Param>, build: Build) -> Arc<ParamFactory> {
        Arc::new(ParamFactory {
            name,
            params,
            build,
        })
    }

    fn display(&self) -> String {
        format!(
            "system.{}({})",
            self.name,
            self.params
                .iter()
                .map(Param::display)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl Factory for ParamFactory {
    fn create(
        &self,
        args: &[&dyn ArgInfo],
        receiver: Option<&CqlType>,
        _d: Dialect,
    ) -> Result<Option<Arc<dyn Function>>> {
        let mandatory = self.params.iter().filter(|p| !p.is_optional()).count();
        if args.len() < mandatory || args.len() > self.params.len() {
            return Err(Error::invalid(format!(
                "Invalid number of arguments for function {}",
                self.display()
            )));
        }
        let mut types: Vec<Option<CqlType>> = args
            .iter()
            .zip(&self.params)
            .map(|(a, p)| p.infer(*a, receiver, None))
            .collect();
        let name = format!("system.{}", self.name);
        let mut resolved = Vec::with_capacity(args.len());
        for (i, (a, p)) in args.iter().zip(&self.params).enumerate() {
            let Some(t) = p.infer(*a, receiver, Some(&types)) else {
                return Err(Error::invalid(format!(
                    "Cannot infer type of argument {} in call to function {}: use type casts to disambiguate",
                    a.text(),
                    self.display()
                )));
            };
            p.validate(&name, *a, &t)?;
            types[i] = Some(t.clone());
            resolved.push(t);
        }
        (self.build)(self.name, &resolved, receiver).map(Some)
    }
}
