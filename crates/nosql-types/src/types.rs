//! The resolved CQL type and its structural predicates.
//!
//! Compatibility checks follow `org.apache.cassandra.db.marshal.AbstractType` and its subclasses
//! (`isCompatibleWith`, `isValueCompatibleWith`, `isSerializationCompatibleWith`), Apache-2.0.
//! ScyllaDB applies the same rules to `ALTER TYPE` / `ALTER TABLE … TYPE`.

use std::sync::Arc;

use crate::Dialect;

/// A fully resolved CQL type: user types are inlined, freezing is explicit at every level.
///
/// `varchar` is `Text` (the reference prints it as `text`). Tuples and vectors are always frozen.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CqlType {
    Ascii,
    Bigint,
    Blob,
    Boolean,
    Counter,
    Date,
    Decimal,
    Double,
    Duration,
    /// `org.apache.cassandra.db.marshal.EmptyType`: only zero-length values (compact tables).
    Empty,
    Float,
    Inet,
    Int,
    Smallint,
    Text,
    Time,
    Timestamp,
    Timeuuid,
    Tinyint,
    Uuid,
    Varint,
    List {
        elem: Arc<CqlType>,
        frozen: bool,
    },
    Set {
        elem: Arc<CqlType>,
        frozen: bool,
    },
    Map {
        key: Arc<CqlType>,
        value: Arc<CqlType>,
        frozen: bool,
    },
    Tuple(Vec<CqlType>),
    /// A user type; the flag is `frozen`.
    Udt(Arc<UdtDef>, bool),
    Vector {
        elem: Arc<CqlType>,
        dim: u32,
    },
    /// A custom marshal class this crate does not model; values are opaque bytes.
    Custom(Arc<str>),
}

/// A user-defined type's definition. Field names are kept as declared (case preserved).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UdtDef {
    pub keyspace: String,
    pub name: String,
    pub fields: Vec<(String, CqlType)>,
}

impl UdtDef {
    pub fn new(
        keyspace: impl Into<String>,
        name: impl Into<String>,
        fields: Vec<(String, CqlType)>,
    ) -> Self {
        UdtDef {
            keyspace: keyspace.into(),
            name: name.into(),
            fields,
        }
    }

    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|(n, _)| n == name)
    }
}

/// Every native type, in the order the reference lists them (`CQL3Type.Native`).
pub const NATIVE_TYPES: &[CqlType] = &[
    CqlType::Ascii,
    CqlType::Bigint,
    CqlType::Blob,
    CqlType::Boolean,
    CqlType::Counter,
    CqlType::Date,
    CqlType::Decimal,
    CqlType::Double,
    CqlType::Duration,
    CqlType::Empty,
    CqlType::Float,
    CqlType::Inet,
    CqlType::Int,
    CqlType::Smallint,
    CqlType::Text,
    CqlType::Time,
    CqlType::Timestamp,
    CqlType::Timeuuid,
    CqlType::Tinyint,
    CqlType::Uuid,
    CqlType::Varint,
];

impl CqlType {
    pub fn list(elem: CqlType, frozen: bool) -> CqlType {
        CqlType::List {
            elem: Arc::new(elem),
            frozen,
        }
    }

    pub fn set(elem: CqlType, frozen: bool) -> CqlType {
        CqlType::Set {
            elem: Arc::new(elem),
            frozen,
        }
    }

    pub fn map(key: CqlType, value: CqlType, frozen: bool) -> CqlType {
        CqlType::Map {
            key: Arc::new(key),
            value: Arc::new(value),
            frozen,
        }
    }

    pub fn vector(elem: CqlType, dim: u32) -> CqlType {
        CqlType::Vector {
            elem: Arc::new(elem),
            dim,
        }
    }

    pub fn udt(def: UdtDef, frozen: bool) -> CqlType {
        CqlType::Udt(Arc::new(def), frozen)
    }

    /// The lowercase CQL name of a native type, `None` for the others.
    pub fn native_name(&self) -> Option<&'static str> {
        Some(match self {
            CqlType::Ascii => "ascii",
            CqlType::Bigint => "bigint",
            CqlType::Blob => "blob",
            CqlType::Boolean => "boolean",
            CqlType::Counter => "counter",
            CqlType::Date => "date",
            CqlType::Decimal => "decimal",
            CqlType::Double => "double",
            CqlType::Duration => "duration",
            CqlType::Empty => "empty",
            CqlType::Float => "float",
            CqlType::Inet => "inet",
            CqlType::Int => "int",
            CqlType::Smallint => "smallint",
            CqlType::Text => "text",
            CqlType::Time => "time",
            CqlType::Timestamp => "timestamp",
            CqlType::Timeuuid => "timeuuid",
            CqlType::Tinyint => "tinyint",
            CqlType::Uuid => "uuid",
            CqlType::Varint => "varint",
            _ => return None,
        })
    }

    /// Looks up a native type by its CQL name (case-insensitive; `varchar` is `text`).
    pub fn from_native_name(name: &str) -> Option<CqlType> {
        let lower = name.to_ascii_lowercase();
        if lower == "varchar" {
            return Some(CqlType::Text);
        }
        NATIVE_TYPES
            .iter()
            .find(|t| t.native_name() == Some(lower.as_str()))
            .cloned()
    }

    pub fn is_native(&self) -> bool {
        self.native_name().is_some()
    }

    pub fn is_collection(&self) -> bool {
        matches!(
            self,
            CqlType::List { .. } | CqlType::Set { .. } | CqlType::Map { .. }
        )
    }

    pub fn is_udt(&self) -> bool {
        matches!(self, CqlType::Udt(..))
    }

    pub fn is_tuple(&self) -> bool {
        matches!(self, CqlType::Tuple(_))
    }

    pub fn is_vector(&self) -> bool {
        matches!(self, CqlType::Vector { .. })
    }

    pub fn is_counter(&self) -> bool {
        matches!(self, CqlType::Counter)
    }

    /// Stored as one cell per element / field (a non-frozen collection or UDT).
    pub fn is_multicell(&self) -> bool {
        match self {
            CqlType::List { frozen, .. }
            | CqlType::Set { frozen, .. }
            | CqlType::Map { frozen, .. } => !frozen,
            CqlType::Udt(_, frozen) => !frozen,
            _ => false,
        }
    }

    pub fn is_frozen_collection(&self) -> bool {
        self.is_collection() && !self.is_multicell()
    }

    /// Whether `frozen<…>` may wrap this type.
    pub fn supports_freezing(&self) -> bool {
        self.is_collection() || self.is_udt() || self.is_tuple() || self.is_vector()
    }

    /// The frozen form, as `AbstractType.freeze()`: collections freeze their elements too, a UDT
    /// keeps its field types.
    pub fn freeze(&self) -> CqlType {
        match self {
            CqlType::List { elem, .. } => CqlType::list(elem.freeze(), true),
            CqlType::Set { elem, .. } => CqlType::set(elem.freeze(), true),
            CqlType::Map { key, value, .. } => CqlType::map(key.freeze(), value.freeze(), true),
            CqlType::Udt(def, _) => CqlType::Udt(def.clone(), true),
            other => other.clone(),
        }
    }

    /// The non-frozen form of a top-level collection or UDT (elements stay frozen).
    pub fn unfreeze(&self) -> CqlType {
        match self {
            CqlType::List { elem, .. } => CqlType::List {
                elem: elem.clone(),
                frozen: false,
            },
            CqlType::Set { elem, .. } => CqlType::Set {
                elem: elem.clone(),
                frozen: false,
            },
            CqlType::Map { key, value, .. } => CqlType::Map {
                key: key.clone(),
                value: value.clone(),
                frozen: false,
            },
            CqlType::Udt(def, _) => CqlType::Udt(def.clone(), false),
            other => other.clone(),
        }
    }

    /// The element / value types a value of this type is made of.
    pub fn subtypes(&self) -> Vec<&CqlType> {
        match self {
            CqlType::List { elem, .. }
            | CqlType::Set { elem, .. }
            | CqlType::Vector { elem, .. } => vec![elem],
            CqlType::Map { key, value, .. } => vec![key, value],
            CqlType::Tuple(ts) => ts.iter().collect(),
            CqlType::Udt(def, _) => def.fields.iter().map(|(_, t)| t).collect(),
            _ => Vec::new(),
        }
    }

    /// True if `duration` appears anywhere inside (it may not be part of a primary key).
    pub fn references_duration(&self) -> bool {
        matches!(self, CqlType::Duration) || self.subtypes().iter().any(|t| t.references_duration())
    }

    /// True if user type `keyspace.name` appears anywhere inside.
    pub fn references_udt(&self, keyspace: &str, name: &str) -> bool {
        if let CqlType::Udt(def, _) = self
            && def.keyspace == keyspace
            && def.name == name
        {
            return true;
        }
        self.subtypes()
            .iter()
            .any(|t| t.references_udt(keyspace, name))
    }

    /// Replaces every occurrence of a user type by its new definition (after `ALTER TYPE`),
    /// keeping each occurrence's freezing.
    pub fn with_updated_udt(&self, def: &Arc<UdtDef>) -> CqlType {
        match self {
            CqlType::Udt(old, frozen) if old.keyspace == def.keyspace && old.name == def.name => {
                CqlType::Udt(def.clone(), *frozen)
            }
            CqlType::Udt(old, frozen) => {
                let fields = old
                    .fields
                    .iter()
                    .map(|(n, t)| (n.clone(), t.with_updated_udt(def)))
                    .collect();
                CqlType::Udt(
                    Arc::new(UdtDef {
                        keyspace: old.keyspace.clone(),
                        name: old.name.clone(),
                        fields,
                    }),
                    *frozen,
                )
            }
            CqlType::List { elem, frozen } => CqlType::list(elem.with_updated_udt(def), *frozen),
            CqlType::Set { elem, frozen } => CqlType::set(elem.with_updated_udt(def), *frozen),
            CqlType::Map { key, value, frozen } => CqlType::map(
                key.with_updated_udt(def),
                value.with_updated_udt(def),
                *frozen,
            ),
            CqlType::Tuple(ts) => {
                CqlType::Tuple(ts.iter().map(|t| t.with_updated_udt(def)).collect())
            }
            CqlType::Vector { elem, dim } => CqlType::vector(elem.with_updated_udt(def), *dim),
            other => other.clone(),
        }
    }

    /// Byte length of every value if it is fixed (`AbstractType.valueLengthIfFixed`).
    pub fn value_length_if_fixed(&self) -> Option<usize> {
        match self {
            CqlType::Boolean => Some(1),
            CqlType::Int | CqlType::Float => Some(4),
            CqlType::Bigint | CqlType::Double | CqlType::Timestamp => Some(8),
            CqlType::Uuid | CqlType::Timeuuid => Some(16),
            CqlType::Empty => Some(0),
            CqlType::Vector { elem, dim } => {
                elem.value_length_if_fixed().map(|l| l * *dim as usize)
            }
            _ => None,
        }
    }

    /// Types for which an empty (zero-length) value reads as null in functions and casts
    /// (`AbstractType.isEmptyValueMeaningless`). Strings, blobs, `tinyint`, `smallint`, `duration`
    /// and the compound types are not among them.
    pub fn is_empty_value_meaningless(&self) -> bool {
        matches!(
            self,
            CqlType::Boolean
                | CqlType::Timeuuid
                | CqlType::Uuid
                | CqlType::Counter
                | CqlType::Double
                | CqlType::Float
                | CqlType::Decimal
                | CqlType::Varint
                | CqlType::Bigint
                | CqlType::Int
                | CqlType::Inet
                | CqlType::Timestamp
                | CqlType::Date
                | CqlType::Time
        )
    }

    /// True for the types the reference compares by plain unsigned byte order.
    pub fn is_byte_order_comparable(&self) -> bool {
        matches!(
            self,
            CqlType::Ascii
                | CqlType::Text
                | CqlType::Blob
                | CqlType::Inet
                | CqlType::Date
                | CqlType::Time
                | CqlType::Duration
                | CqlType::Custom(_)
        )
    }

    /// Why this type cannot be a primary key column, as the reference words it, or `None`.
    pub fn primary_key_error(&self, column: &str, dialect: Dialect) -> Option<String> {
        let scylla = dialect.is_scylla();
        if self.is_counter() {
            return Some(if scylla {
                format!("counter type is not supported for PRIMARY KEY part {column}")
            } else {
                format!("counter type is not supported for PRIMARY KEY column '{column}'")
            });
        }
        if self.references_duration() {
            return Some(if scylla {
                format!("duration type is not supported for PRIMARY KEY part {column}")
            } else {
                format!("duration type is not supported for PRIMARY KEY column '{column}'")
            });
        }
        if self.is_multicell() {
            if scylla {
                return Some(if self.is_collection() {
                    format!("Invalid non-frozen collection type for PRIMARY KEY component {column}")
                } else {
                    format!(
                        "Invalid non-frozen user-defined type for PRIMARY KEY component {column}"
                    )
                });
            }
            return Some(if self.is_collection() {
                format!(
                    "Invalid non-frozen collection type {} for PRIMARY KEY column '{column}'",
                    crate::names::cql_name(self)
                )
            } else {
                format!(
                    "Invalid non-frozen user-defined type \"{}\" for PRIMARY KEY column '{column}'",
                    crate::names::cql_name(self)
                )
            });
        }
        None
    }

    /// `isCompatibleWith`: `prev` can be replaced by `self` and every value keeps both its
    /// validity and its ordering. Clustering columns may only change along this relation.
    pub fn is_compatible_with(&self, prev: &CqlType) -> bool {
        if self == prev {
            return true;
        }
        match (self, prev) {
            (CqlType::Text, CqlType::Ascii) => true,
            (CqlType::Blob, CqlType::Ascii | CqlType::Text) => true,
            (
                CqlType::List { frozen: f1, .. }
                | CqlType::Set { frozen: f1, .. }
                | CqlType::Map { frozen: f1, .. },
                _,
            ) => {
                if std::mem::discriminant(self) != std::mem::discriminant(prev)
                    || Some(*f1) != prev.collection_frozen()
                {
                    return false;
                }
                if *f1 {
                    return self.is_compatible_with_frozen(prev);
                }
                // Multi-cell: the cell path ordering must be compatible; cell values only need
                // to deserialise the same way.
                match (self, prev) {
                    (CqlType::List { elem: a, .. }, CqlType::List { elem: b, .. }) => {
                        a.is_serialization_compatible_with(b)
                    }
                    (CqlType::Set { elem: a, .. }, CqlType::Set { elem: b, .. }) => {
                        a.is_compatible_with(b)
                    }
                    (
                        CqlType::Map {
                            key: ka, value: va, ..
                        },
                        CqlType::Map {
                            key: kb, value: vb, ..
                        },
                    ) => ka.is_compatible_with(kb) && va.is_serialization_compatible_with(vb),
                    _ => false,
                }
            }
            // `TupleType.isCompatibleWith` (inherited by user types): new trailing components are
            // fine, removing is not.
            (CqlType::Tuple(_) | CqlType::Udt(..), CqlType::Tuple(_) | CqlType::Udt(..)) => {
                let new = self.subtypes();
                let old = prev.subtypes();
                new.len() >= old.len() && old.iter().zip(&new).all(|(o, n)| n.is_compatible_with(o))
            }
            _ => false,
        }
    }

    /// `isValueCompatibleWith`: every value of `prev` reads as a sensible value of `self`
    /// (regular columns may change along this relation; ordering may differ).
    pub fn is_value_compatible_with(&self, prev: &CqlType) -> bool {
        if self == prev {
            return true;
        }
        match self {
            CqlType::Blob => true,
            CqlType::Bigint => matches!(prev, CqlType::Timestamp),
            CqlType::Timestamp => matches!(prev, CqlType::Bigint),
            CqlType::Time => matches!(prev, CqlType::Bigint),
            CqlType::Date => matches!(prev, CqlType::Int),
            CqlType::Varint => matches!(prev, CqlType::Int | CqlType::Bigint | CqlType::Timestamp),
            CqlType::Uuid => matches!(prev, CqlType::Timeuuid),
            CqlType::Duration => false,
            CqlType::List { frozen, .. }
            | CqlType::Set { frozen, .. }
            | CqlType::Map { frozen, .. } => {
                if !frozen {
                    return self.is_compatible_with(prev);
                }
                if std::mem::discriminant(self) != std::mem::discriminant(prev)
                    || prev.collection_frozen() != Some(true)
                {
                    return false;
                }
                match (self, prev) {
                    (CqlType::List { elem: a, .. }, CqlType::List { elem: b, .. }) => {
                        a.is_value_compatible_with(b)
                    }
                    (CqlType::Set { .. }, CqlType::Set { .. }) => {
                        self.is_compatible_with_frozen(prev)
                    }
                    (
                        CqlType::Map {
                            key: ka, value: va, ..
                        },
                        CqlType::Map {
                            key: kb, value: vb, ..
                        },
                    ) => ka.is_compatible_with(kb) && va.is_value_compatible_with(vb),
                    _ => false,
                }
            }
            CqlType::Tuple(new) => match prev {
                CqlType::Tuple(old) => {
                    new.len() >= old.len()
                        && old
                            .iter()
                            .zip(new)
                            .all(|(o, n)| n.is_value_compatible_with(o))
                }
                _ => false,
            },
            // `UserType.isValueCompatibleWith`: same keyspace and freezing, fields compatible
            // pairwise, new fields only at the end.
            CqlType::Udt(def, frozen) => match prev {
                CqlType::Udt(odef, ofrozen) => {
                    frozen == ofrozen
                        && def.keyspace == odef.keyspace
                        && def.fields.len() >= odef.fields.len()
                        && odef
                            .fields
                            .iter()
                            .zip(&def.fields)
                            .all(|((_, o), (_, n))| n.is_compatible_with(o))
                }
                _ => false,
            },
            _ => self.is_compatible_with(prev),
        }
    }

    /// `isSerializationCompatibleWith`: value-compatible, and cells are laid out the same way.
    pub fn is_serialization_compatible_with(&self, prev: &CqlType) -> bool {
        match self {
            // A collection's "value comparator" is the element type for lists, the map value type
            // for maps and `EmptyType` for sets.
            CqlType::List { elem, .. } => {
                if !self.is_value_compatible_with(prev) {
                    return false;
                }
                match prev {
                    CqlType::List { elem: pe, .. } => elem.is_serialization_compatible_with(pe),
                    _ => false,
                }
            }
            CqlType::Set { .. } => self.is_value_compatible_with(prev),
            CqlType::Map { value, .. } => {
                if !self.is_value_compatible_with(prev) {
                    return false;
                }
                match prev {
                    CqlType::Map { value: pv, .. } => value.is_serialization_compatible_with(pv),
                    _ => false,
                }
            }
            CqlType::Blob if prev.is_collection() || prev.is_udt() => false,
            _ => {
                self.is_value_compatible_with(prev)
                    && self.value_length_if_fixed() == prev.value_length_if_fixed()
                    && self.is_multicell() == prev.is_multicell()
            }
        }
    }

    fn collection_frozen(&self) -> Option<bool> {
        match self {
            CqlType::List { frozen, .. }
            | CqlType::Set { frozen, .. }
            | CqlType::Map { frozen, .. } => Some(*frozen),
            _ => None,
        }
    }

    fn is_compatible_with_frozen(&self, prev: &CqlType) -> bool {
        match (self, prev) {
            (CqlType::List { elem: a, .. }, CqlType::List { elem: b, .. })
            | (CqlType::Set { elem: a, .. }, CqlType::Set { elem: b, .. }) => {
                a.is_compatible_with(b)
            }
            (
                CqlType::Map {
                    key: ka, value: va, ..
                },
                CqlType::Map {
                    key: kb, value: vb, ..
                },
            ) => ka.is_compatible_with(kb) && va.is_compatible_with(vb),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(fields: &[(&str, CqlType)]) -> Arc<UdtDef> {
        Arc::new(UdtDef::new(
            "ks",
            "addr",
            fields
                .iter()
                .map(|(n, t)| (n.to_string(), t.clone()))
                .collect(),
        ))
    }

    #[test]
    fn native_names_round_trip() {
        for t in NATIVE_TYPES {
            assert_eq!(
                CqlType::from_native_name(t.native_name().unwrap()).as_ref(),
                Some(t)
            );
        }
        assert_eq!(CqlType::from_native_name("VARCHAR"), Some(CqlType::Text));
        assert_eq!(CqlType::from_native_name("Int"), Some(CqlType::Int));
        assert_eq!(CqlType::from_native_name("list"), None);
    }

    #[test]
    fn freezing() {
        let t = CqlType::list(CqlType::set(CqlType::Int, false), false);
        assert!(t.is_multicell());
        let f = t.freeze();
        assert_eq!(f, CqlType::list(CqlType::set(CqlType::Int, true), true));
        assert!(!f.is_multicell());
        assert!(f.is_frozen_collection());
        assert_eq!(
            f.unfreeze(),
            CqlType::list(CqlType::set(CqlType::Int, true), false)
        );
        assert!(!CqlType::Int.supports_freezing());
        assert!(CqlType::Tuple(vec![]).supports_freezing());
        let u = CqlType::Udt(addr(&[("a", CqlType::Int)]), false);
        assert!(u.is_multicell());
        assert!(!u.freeze().is_multicell());
    }

    #[test]
    fn fixed_lengths() {
        assert_eq!(CqlType::Int.value_length_if_fixed(), Some(4));
        assert_eq!(CqlType::Smallint.value_length_if_fixed(), None);
        assert_eq!(CqlType::Date.value_length_if_fixed(), None);
        assert_eq!(
            CqlType::vector(CqlType::Float, 3).value_length_if_fixed(),
            Some(12)
        );
        assert_eq!(
            CqlType::vector(CqlType::Text, 3).value_length_if_fixed(),
            None
        );
    }

    #[test]
    fn key_errors_match_reference() {
        let c = Dialect::Cassandra;
        let s = Dialect::Scylla;
        assert_eq!(
            CqlType::Duration.primary_key_error("c", c).unwrap(),
            "duration type is not supported for PRIMARY KEY column 'c'"
        );
        assert_eq!(
            CqlType::Duration.primary_key_error("k", s).unwrap(),
            "duration type is not supported for PRIMARY KEY part k"
        );
        assert_eq!(
            CqlType::Counter.primary_key_error("c", c).unwrap(),
            "counter type is not supported for PRIMARY KEY column 'c'"
        );
        assert_eq!(
            CqlType::Counter.primary_key_error("c", s).unwrap(),
            "counter type is not supported for PRIMARY KEY part c"
        );
        let l = CqlType::list(CqlType::Int, false);
        assert_eq!(
            l.primary_key_error("c", c).unwrap(),
            "Invalid non-frozen collection type list<int> for PRIMARY KEY column 'c'"
        );
        assert_eq!(
            l.primary_key_error("c", s).unwrap(),
            "Invalid non-frozen collection type for PRIMARY KEY component c"
        );
        assert!(l.freeze().primary_key_error("c", c).is_none());
        assert!(
            CqlType::list(CqlType::Duration, true)
                .primary_key_error("c", c)
                .is_some()
        );
        assert!(CqlType::Int.primary_key_error("c", s).is_none());
    }

    #[test]
    fn compatibility() {
        assert!(CqlType::Text.is_compatible_with(&CqlType::Ascii));
        assert!(!CqlType::Ascii.is_compatible_with(&CqlType::Text));
        assert!(CqlType::Blob.is_compatible_with(&CqlType::Text));
        assert!(!CqlType::Blob.is_compatible_with(&CqlType::Int));
        assert!(CqlType::Blob.is_value_compatible_with(&CqlType::Int));
        assert!(CqlType::Varint.is_value_compatible_with(&CqlType::Int));
        assert!(CqlType::Varint.is_value_compatible_with(&CqlType::Timestamp));
        assert!(!CqlType::Varint.is_compatible_with(&CqlType::Int));
        assert!(CqlType::Bigint.is_value_compatible_with(&CqlType::Timestamp));
        assert!(CqlType::Timestamp.is_value_compatible_with(&CqlType::Bigint));
        assert!(CqlType::Date.is_value_compatible_with(&CqlType::Int));
        assert!(!CqlType::Int.is_value_compatible_with(&CqlType::Date));
        assert!(CqlType::Uuid.is_value_compatible_with(&CqlType::Timeuuid));
        assert!(!CqlType::Timeuuid.is_value_compatible_with(&CqlType::Uuid));
        assert!(CqlType::Duration.is_value_compatible_with(&CqlType::Duration));

        let fl = |t| CqlType::list(t, true);
        assert!(fl(CqlType::Text).is_compatible_with(&fl(CqlType::Ascii)));
        assert!(!fl(CqlType::Text).is_compatible_with(&CqlType::list(CqlType::Ascii, false)));
        assert!(fl(CqlType::Varint).is_value_compatible_with(&fl(CqlType::Int)));
        assert!(
            CqlType::list(CqlType::Blob, false)
                .is_compatible_with(&CqlType::list(CqlType::Text, false))
        );
        // Multi-cell list values: blob can read int cells, but fixed lengths differ.
        assert!(
            !CqlType::list(CqlType::Blob, false)
                .is_compatible_with(&CqlType::list(CqlType::Int, false))
        );
        assert!(
            CqlType::map(CqlType::Text, CqlType::Blob, false).is_compatible_with(&CqlType::map(
                CqlType::Ascii,
                CqlType::Text,
                false
            ))
        );

        let t2 = CqlType::Tuple(vec![CqlType::Int, CqlType::Text]);
        let t3 = CqlType::Tuple(vec![CqlType::Int, CqlType::Text, CqlType::Int]);
        assert!(t3.is_compatible_with(&t2));
        assert!(!t2.is_compatible_with(&t3));
        assert!(t3.is_value_compatible_with(&t2));

        let a1 = CqlType::Udt(addr(&[("a", CqlType::Int)]), true);
        let a2 = CqlType::Udt(addr(&[("a", CqlType::Int), ("b", CqlType::Text)]), true);
        assert!(a2.is_value_compatible_with(&a1));
        assert!(!a1.is_value_compatible_with(&a2));
        assert!(!CqlType::Udt(addr(&[("a", CqlType::Int)]), false).is_value_compatible_with(&a1));
        assert!(!CqlType::Blob.is_serialization_compatible_with(&a1));
        assert!(!CqlType::Blob.is_serialization_compatible_with(&CqlType::Int));
        assert!(CqlType::Blob.is_serialization_compatible_with(&CqlType::Text));
    }

    #[test]
    fn udt_references() {
        let a = addr(&[("a", CqlType::Int)]);
        let t = CqlType::map(CqlType::Int, CqlType::Udt(a.clone(), true), false);
        assert!(t.references_udt("ks", "addr"));
        assert!(!t.references_udt("ks", "other"));
        let a2 = Arc::new(UdtDef::new(
            "ks",
            "addr",
            vec![("a".into(), CqlType::Int), ("b".into(), CqlType::Int)],
        ));
        let u = t.with_updated_udt(&a2);
        assert_eq!(u, CqlType::map(CqlType::Int, CqlType::Udt(a2, true), false));
        assert!(CqlType::Tuple(vec![CqlType::list(CqlType::Duration, true)]).references_duration());
    }
}

#[cfg(test)]
mod predicate_tests {
    use super::*;

    #[test]
    fn predicates() {
        let u = Arc::new(UdtDef::new(
            "ks",
            "u",
            vec![("a".into(), CqlType::Int), ("B".into(), CqlType::Text)],
        ));
        assert_eq!(u.field_index("B"), Some(1));
        assert_eq!(u.field_index("b"), None);
        let all = [
            CqlType::list(CqlType::Int, false),
            CqlType::set(CqlType::Int, true),
            CqlType::map(CqlType::Int, CqlType::Int, false),
            CqlType::Tuple(vec![CqlType::Int]),
            CqlType::Udt(u.clone(), false),
            CqlType::vector(CqlType::Float, 2),
            CqlType::Custom("a.B".into()),
        ];
        let flags: Vec<(bool, bool, bool, bool, bool)> = all
            .iter()
            .map(|t| {
                (
                    t.is_native(),
                    t.is_collection(),
                    t.is_tuple(),
                    t.is_udt(),
                    t.is_vector(),
                )
            })
            .collect();
        assert_eq!(
            flags,
            vec![
                (false, true, false, false, false),
                (false, true, false, false, false),
                (false, true, false, false, false),
                (false, false, true, false, false),
                (false, false, false, true, false),
                (false, false, false, false, true),
                (false, false, false, false, false),
            ]
        );
        assert!(CqlType::Int.is_native());
        assert!(CqlType::Counter.is_counter());
        assert!(!CqlType::Bigint.is_counter());
        assert_eq!(CqlType::Custom("a.B".into()).native_name(), None);
        assert_eq!(all[2].subtypes().len(), 2);
        assert_eq!(all[4].subtypes(), vec![&CqlType::Int, &CqlType::Text]);
        assert!(CqlType::Int.subtypes().is_empty());
        // Empty means null for these (functions, casts), not for strings, blobs, tinyint, smallint.
        for t in [
            CqlType::Int,
            CqlType::Boolean,
            CqlType::Uuid,
            CqlType::Timestamp,
            CqlType::Date,
            CqlType::Time,
            CqlType::Decimal,
        ] {
            assert!(t.is_empty_value_meaningless(), "{t:?}");
        }
        for t in [
            CqlType::Text,
            CqlType::Blob,
            CqlType::Tinyint,
            CqlType::Smallint,
            CqlType::Duration,
            all[0].clone(),
        ] {
            assert!(!t.is_empty_value_meaningless(), "{t:?}");
        }
        for t in [
            CqlType::Text,
            CqlType::Ascii,
            CqlType::Blob,
            CqlType::Inet,
            CqlType::Date,
            CqlType::Time,
            CqlType::Duration,
        ] {
            assert!(t.is_byte_order_comparable(), "{t:?}");
        }
        for t in [
            CqlType::Int,
            CqlType::Uuid,
            CqlType::Timeuuid,
            CqlType::Float,
            CqlType::Varint,
        ] {
            assert!(!t.is_byte_order_comparable(), "{t:?}");
        }
        // A non-frozen UDT in a key.
        let ut = CqlType::Udt(u, false);
        assert_eq!(
            ut.primary_key_error("c", Dialect::Cassandra).unwrap(),
            "Invalid non-frozen user-defined type \"u\" for PRIMARY KEY column 'c'"
        );
        assert_eq!(
            ut.primary_key_error("c", Dialect::Scylla).unwrap(),
            "Invalid non-frozen user-defined type for PRIMARY KEY component c"
        );
        assert!(
            ut.freeze()
                .primary_key_error("c", Dialect::Scylla)
                .is_none()
        );
        assert_eq!(NATIVE_TYPES.len(), 21);
    }
}
