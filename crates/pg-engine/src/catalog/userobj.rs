//! Catalog objects of kinds Postgres ships rows for, made by DDL: access methods (pg_am),
//! operator families and classes (pg_opfamily, pg_opclass) and their members (pg_amop,
//! pg_amproc), casts (pg_cast), conversions (pg_conversion), procedural languages
//! (pg_language) and transforms (pg_transform). Lookups here see the built-in rows and the
//! made ones alike.

use super::static_rows::{oid_at, rows, text_at};
use super::system::SysTable;
use super::{Catalog, PG_CATALOG_OID};
use crate::error::{Error, Result, code};
use crate::types::Value;
use serde::{Deserialize, Serialize};

/// The bootstrap superuser, owner of the built-in objects.
const OWNER: u32 = 10;

/// CREATE ACCESS METHOD (pg_am).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessMethod {
    pub oid: u32,
    pub name: String,
    /// The handler function (pg_proc oid).
    pub handler: u32,
    /// 'i' index, 't' table.
    pub kind: char,
}

/// CREATE OPERATOR FAMILY (or the family CREATE OPERATOR CLASS makes): pg_opfamily.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpFamily {
    pub oid: u32,
    pub method: u32,
    pub name: String,
    pub schema: u32,
    pub owner: u32,
}

/// CREATE OPERATOR CLASS: pg_opclass.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpClass {
    pub oid: u32,
    pub method: u32,
    pub name: String,
    pub schema: u32,
    pub owner: u32,
    pub family: u32,
    /// opcintype
    pub intype: u32,
    pub default: bool,
    /// opckeytype (0: the input type)
    pub keytype: u32,
}

/// An operator (pg_amop) or support function (pg_amproc) of an operator family.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpMember {
    pub oid: u32,
    pub family: u32,
    /// A support function rather than an operator.
    pub func: bool,
    pub left: u32,
    pub right: u32,
    /// amopstrategy / amprocnum
    pub number: i16,
    /// The operator or function.
    pub object: u32,
    /// amopsortfamily: an ordering operator's btree family (0: a search operator).
    pub sortfamily: u32,
    /// `ref_is_hard`: the member is part of the class `refclass` (or, with 0, of the
    /// family); else it belongs to the family loosely.
    pub hard: bool,
    pub refclass: u32,
    /// The operator's or function's input types (members of other types depend on theirs).
    #[serde(default)]
    pub inputs: Vec<u32>,
}

/// CREATE TYPE base types and shell types (pg_type rows of typtype 'b' with their array
/// types, or 'p' for a shell).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaseTypeDef {
    pub ty: std::sync::Arc<crate::types::BaseType>,
    pub owner: u32,
    pub input: u32,
    pub output: u32,
    pub receive: u32,
    pub send: u32,
    pub typmod_in: u32,
    pub typmod_out: u32,
    pub analyze: u32,
    pub subscript: u32,
    /// typelem
    pub element: u32,
    pub delimiter: char,
    pub storage: char,
    pub default: Option<String>,
}

impl BaseTypeDef {
    pub fn oid(&self) -> u32 {
        self.ty.oid
    }

    pub fn is_shell(&self) -> bool {
        self.ty.is_shell()
    }

    pub fn data_type(&self) -> crate::types::DataType {
        crate::types::DataType::Base(self.ty.clone())
    }
}

/// CREATE CAST: pg_cast.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cast {
    pub oid: u32,
    pub source: u32,
    pub target: u32,
    /// The cast function (0: binary coercible or I/O).
    pub func: u32,
    /// 'e' explicit, 'a' assignment, 'i' implicit.
    pub context: char,
    /// 'f' function, 'b' binary coercible, 'i' I/O conversion.
    pub method: char,
    /// The binary-coercible casts the function's argument and result need.
    #[serde(default)]
    pub incast: u32,
    #[serde(default)]
    pub outcast: u32,
}

/// CREATE CONVERSION: pg_conversion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversion {
    pub oid: u32,
    pub name: String,
    pub schema: u32,
    pub owner: u32,
    pub for_encoding: i32,
    pub to_encoding: i32,
    pub proc: u32,
    pub default: bool,
}

/// CREATE LANGUAGE: pg_language.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Language {
    pub oid: u32,
    pub name: String,
    pub owner: u32,
    pub trusted: bool,
    pub handler: u32,
    pub inline: u32,
    pub validator: u32,
}

/// CREATE FOREIGN DATA WRAPPER: pg_foreign_data_wrapper.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForeignDataWrapper {
    pub oid: u32,
    pub name: String,
    pub owner: u32,
    pub handler: u32,
    pub validator: u32,
    /// `name=value` options (None: NULL).
    pub options: Option<Vec<String>>,
    /// Where its current row version lies in the catalog's heap (each change appends one).
    #[serde(default)]
    pub heap_order: u64,
}

/// CREATE SERVER: pg_foreign_server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForeignServer {
    pub oid: u32,
    pub name: String,
    pub owner: u32,
    pub fdw: u32,
    pub kind: Option<String>,
    pub version: Option<String>,
    pub options: Option<Vec<String>>,
    #[serde(default)]
    pub heap_order: u64,
}

/// CREATE USER MAPPING: pg_user_mapping (user 0: PUBLIC).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserMapping {
    pub oid: u32,
    pub user: u32,
    pub server: u32,
    pub options: Option<Vec<String>>,
}

/// CREATE TRANSFORM: pg_transform.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transform {
    pub oid: u32,
    pub ty: u32,
    pub lang: u32,
    pub fromsql: u32,
    pub tosql: u32,
}

/// The built-in index access methods' numbers of strategies (0: any) and support functions,
/// their options-parsing support function, and whether they take ordering operators and
/// a storage type other than the indexed one.
pub struct AmTraits {
    pub strategies: i32,
    pub support: i32,
    pub opts_proc: i32,
    pub can_order_by_op: bool,
    pub storage: bool,
}

/// The traits of an index access method by its handler (a made access method behaves as
/// its handler's).
pub fn am_traits(handler: u32) -> Option<AmTraits> {
    let t = |strategies, support, opts_proc, can_order_by_op, storage| AmTraits {
        strategies,
        support,
        opts_proc,
        can_order_by_op,
        storage,
    };
    Some(match handler {
        330 => t(5, 6, 5, false, false),      // bthandler
        331 => t(1, 3, 3, false, false),      // hashhandler
        332 => t(0, 12, 10, true, true),      // gisthandler
        333 => t(0, 7, 7, false, true),       // ginhandler
        334 => t(0, 7, 6, true, true),        // spghandler
        335 => t(0, 15, 5, false, true),      // brinhandler
        _ => return None,
    })
}

/// An access method as pg_am has it.
#[derive(Clone, Debug)]
pub struct AmInfo {
    pub oid: u32,
    pub name: String,
    pub handler: u32,
    pub kind: char,
}

/// An operator family as pg_opfamily has it.
#[derive(Clone, Debug)]
pub struct FamilyInfo {
    pub oid: u32,
    pub method: u32,
    pub name: String,
    pub schema: u32,
    pub owner: u32,
}

/// An operator class as pg_opclass has it.
#[derive(Clone, Debug)]
pub struct ClassInfo {
    pub oid: u32,
    pub method: u32,
    pub name: String,
    pub schema: u32,
    pub owner: u32,
    pub family: u32,
    pub intype: u32,
    pub default: bool,
    pub keytype: u32,
}

/// A member of an operator family as pg_amop / pg_amproc have it.
#[derive(Clone, Debug)]
pub struct MemberInfo {
    pub oid: u32,
    pub family: u32,
    pub func: bool,
    pub left: u32,
    pub right: u32,
    pub number: i16,
    pub object: u32,
    pub purpose: char,
    pub method: u32,
    pub sortfamily: u32,
}

fn char_at(r: &[Value], i: usize) -> char {
    match r.get(i) {
        Some(Value::Char(c)) => *c as char,
        Some(Value::Text(t)) => t.chars().next().unwrap_or(' '),
        _ => ' ',
    }
}

fn int_at(r: &[Value], i: usize) -> i64 {
    match r.get(i) {
        Some(Value::Int2(v)) => *v as i64,
        Some(Value::Int4(v)) => *v as i64,
        Some(Value::Int8(v)) => *v,
        _ => 0,
    }
}

fn bool_at(r: &[Value], i: usize) -> bool {
    matches!(r.get(i), Some(Value::Bool(true)))
}

impl Catalog {
    /// Every access method: Postgres's and the made ones, in oid order.
    pub fn access_methods_all(&self) -> Vec<AmInfo> {
        let mut out: Vec<AmInfo> = rows(SysTable::PgAm)
            .iter()
            .map(|r| AmInfo {
                oid: oid_at(r, 0),
                name: text_at(r, 1).to_string(),
                handler: oid_at(r, 2),
                kind: char_at(r, 3),
            })
            .collect();
        out.extend(self.access_methods.values().map(|a| AmInfo {
            oid: a.oid,
            name: a.name.clone(),
            handler: a.handler,
            kind: a.kind,
        }));
        out
    }

    pub fn access_method(&self, oid: u32) -> Option<AmInfo> {
        self.access_method_where(|r| oid_at(r, 0) == oid, |a| a.oid == oid)
    }

    pub fn access_method_by_name(&self, name: &str) -> Option<AmInfo> {
        self.access_method_where(|r| text_at(r, 1) == name, |a| a.name == name)
    }

    /// The first of `access_methods_all` a test picks, without listing them all (a built-in
    /// one's pg_am row, else a made one).
    fn access_method_where(
        &self,
        builtin: impl Fn(&[Value]) -> bool,
        made: impl Fn(&AccessMethod) -> bool,
    ) -> Option<AmInfo> {
        if let Some(r) = rows(SysTable::PgAm).iter().find(|r| builtin(r)) {
            return Some(AmInfo {
                oid: oid_at(r, 0),
                name: text_at(r, 1).to_string(),
                handler: oid_at(r, 2),
                kind: char_at(r, 3),
            });
        }
        self.access_methods.values().find(|a| made(a)).map(|a| AmInfo {
            oid: a.oid,
            name: a.name.clone(),
            handler: a.handler,
            kind: a.kind,
        })
    }

    /// get_index_am_oid: an index access method by name.
    pub fn index_am(&self, name: &str) -> Result<AmInfo> {
        match self.access_method_by_name(name) {
            Some(a) if a.kind == 'i' => Ok(a),
            Some(_) => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("access method \"{name}\" is not of type INDEX"),
            )),
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("access method \"{name}\" does not exist"),
            )),
        }
    }

    /// The built-in index access method a (possibly made) one behaves as, by name.
    pub fn am_builtin_name(&self, name: &str) -> String {
        match self.access_methods.values().find(|a| a.name == name) {
            Some(a) => rows(SysTable::PgAm)
                .iter()
                .find(|r| oid_at(r, 2) == a.handler)
                .map(|r| text_at(r, 1).to_string())
                .unwrap_or_else(|| name.to_string()),
            None => name.to_string(),
        }
    }

    /// Every operator family: Postgres's and the made ones.
    pub fn opfamilies_all(&self) -> Vec<FamilyInfo> {
        let mut out: Vec<FamilyInfo> = rows(SysTable::PgOpfamily)
            .iter()
            .map(|r| FamilyInfo {
                oid: oid_at(r, 0),
                method: oid_at(r, 1),
                name: text_at(r, 2).to_string(),
                schema: oid_at(r, 3),
                owner: oid_at(r, 4),
            })
            .collect();
        out.extend(self.opfamilies.values().map(|f| FamilyInfo {
            oid: f.oid,
            method: f.method,
            name: f.name.clone(),
            schema: f.schema,
            owner: f.owner,
        }));
        out
    }

    pub fn opfamily_info(&self, oid: u32) -> Option<FamilyInfo> {
        if let Some(f) = self.opfamilies.get(&oid) {
            return Some(FamilyInfo {
                oid,
                method: f.method,
                name: f.name.clone(),
                schema: f.schema,
                owner: f.owner,
            });
        }
        rows(SysTable::PgOpfamily)
            .iter()
            .find(|r| oid_at(r, 0) == oid)
            .map(|r| FamilyInfo {
                oid,
                method: oid_at(r, 1),
                name: text_at(r, 2).to_string(),
                schema: oid_at(r, 3),
                owner: OWNER,
            })
    }

    /// Every operator class: Postgres's and the made ones.
    pub fn opclasses_all(&self) -> Vec<ClassInfo> {
        let mut out: Vec<ClassInfo> = rows(SysTable::PgOpclass)
            .iter()
            .map(|r| ClassInfo {
                oid: oid_at(r, 0),
                method: oid_at(r, 1),
                name: text_at(r, 2).to_string(),
                schema: oid_at(r, 3),
                owner: oid_at(r, 4),
                family: oid_at(r, 5),
                intype: oid_at(r, 6),
                default: bool_at(r, 7),
                keytype: oid_at(r, 8),
            })
            .collect();
        out.extend(self.opclasses.values().map(class_info));
        out
    }

    /// A sort of made type `ty` (FinishSortSupportFunction): its ordering operator's btree
    /// family must have a comparison function (FUNCTION 1) for (`ty`, `ty`), which the sort
    /// calls. The family of `ty`'s `<` / `>` (strategy 1 / 5) whose comparison is missing,
    /// when no family with such an operator has one (None: none is missing, or `ty` has no
    /// made ordering operator).
    pub fn sort_support_missing(&self, ty: u32) -> Option<u32> {
        let btree = |fam: u32| {
            self.opfamilies.get(&fam).map(|f| f.method).or_else(|| {
                crate::catalog::system::OPFAMILIES.iter().find(|x| x.0 == fam).map(|x| x.1)
            }) == Some(403)
        };
        let mut missing = None;
        for m in self.opmembers.values() {
            if m.func || m.left != ty || m.right != ty || !matches!(m.number, 1 | 5) || !btree(m.family) {
                continue;
            }
            let cmp = self
                .opmembers
                .values()
                .any(|f| f.func && f.family == m.family && f.number == 1 && f.left == ty && f.right == ty);
            if cmp {
                return None;
            }
            missing.get_or_insert(m.family);
        }
        missing
    }

    pub fn opclass_info(&self, oid: u32) -> Option<ClassInfo> {
        if let Some(c) = self.opclasses.get(&oid) {
            return Some(class_info(c));
        }
        // (The built-in ones by oid, read once: this runs for every index column of
        // pg_attribute's rows.)
        static BUILTIN: std::sync::OnceLock<std::collections::HashMap<u32, ClassInfo>> =
            std::sync::OnceLock::new();
        let builtin = BUILTIN.get_or_init(|| {
            let mut m = std::collections::HashMap::new();
            for c in Catalog::default().opclasses_all() {
                m.entry(c.oid).or_insert(c);
            }
            m
        });
        builtin.get(&oid).cloned()
    }

    /// Every operator (`func` false) or support function of operator families.
    pub fn opmembers_all(&self, func: bool) -> Vec<MemberInfo> {
        let mut out: Vec<MemberInfo> = if func {
            rows(SysTable::PgAmproc)
                .iter()
                .map(|r| MemberInfo {
                    oid: oid_at(r, 0),
                    family: oid_at(r, 1),
                    func: true,
                    left: oid_at(r, 2),
                    right: oid_at(r, 3),
                    number: int_at(r, 4) as i16,
                    object: oid_at(r, 5),
                    purpose: ' ',
                    method: 0,
                    sortfamily: 0,
                })
                .collect()
        } else {
            rows(SysTable::PgAmop)
                .iter()
                .map(|r| MemberInfo {
                    oid: oid_at(r, 0),
                    family: oid_at(r, 1),
                    func: false,
                    left: oid_at(r, 2),
                    right: oid_at(r, 3),
                    number: int_at(r, 4) as i16,
                    purpose: char_at(r, 5),
                    object: oid_at(r, 6),
                    method: oid_at(r, 7),
                    sortfamily: oid_at(r, 8),
                })
                .collect()
        };
        out.retain(|m| !self.opmembers_removed.contains(&m.oid));
        for m in self.opmembers.values().filter(|m| m.func == func) {
            out.push(self.member_info(m));
        }
        out
    }

    fn member_info(&self, m: &OpMember) -> MemberInfo {
        MemberInfo {
            oid: m.oid,
            family: m.family,
            func: m.func,
            left: m.left,
            right: m.right,
            number: m.number,
            object: m.object,
            purpose: if m.sortfamily != 0 { 'o' } else { 's' },
            method: self.opfamily_info(m.family).map_or(0, |f| f.method),
            sortfamily: m.sortfamily,
        }
    }

    /// A member by its pg_amop (`func` false) or pg_amproc oid.
    pub fn opmember_info(&self, func: bool, oid: u32) -> Option<MemberInfo> {
        if let Some(m) = self.opmembers.get(&oid).filter(|m| m.func == func) {
            return Some(self.member_info(m));
        }
        self.opmembers_all(func).into_iter().find(|m| m.oid == oid)
    }

    /// The schema a qualified name's schema part names (None: unqualified), and the bare
    /// name; `missing` says the schema doesn't exist.
    fn split_name<'n>(&self, parts: &'n [String]) -> Result<(Option<Option<u32>>, &'n str)> {
        match parts {
            [n] => Ok((None, n.as_str())),
            [s, n] | [_, s, n] => Ok((Some(self.schema_by_name(s).map(|x| x.oid)), n.as_str())),
            _ => Err(Error::new(
                code::SYNTAX_ERROR,
                format!(
                    "improper qualified name (too many dotted names): {}",
                    parts.join(".")
                ),
            )),
        }
    }

    /// OpfamilynameGetOpfid / get_opfamily_oid: a family of access method `am` by
    /// (possibly qualified) name. Err for a missing schema (unless `missing_ok`), Ok(None)
    /// when there's no such family.
    pub fn find_opfamily(
        &self,
        am: u32,
        parts: &[String],
        search_path: &[String],
    ) -> Result<Option<FamilyInfo>> {
        let (schema, name) = self.split_name(parts)?;
        let all = self.opfamilies_all();
        let find = |s: u32| {
            all.iter()
                .find(|f| f.method == am && f.schema == s && f.name == name)
                .cloned()
        };
        match schema {
            Some(Some(s)) => Ok(find(s)),
            Some(None) => Err(Error::new(
                code::UNDEFINED_SCHEMA,
                format!("schema \"{}\" does not exist", parts[parts.len() - 2]),
            )),
            None => Ok(self
                .effective_path(search_path)
                .into_iter()
                .find_map(find)),
        }
    }

    /// OpclassnameGetOpcid / get_opclass_oid.
    pub fn find_opclass(
        &self,
        am: u32,
        parts: &[String],
        search_path: &[String],
    ) -> Result<Option<ClassInfo>> {
        let (schema, name) = self.split_name(parts)?;
        let all = self.opclasses_all();
        let find = |s: u32| {
            all.iter()
                .find(|c| c.method == am && c.schema == s && c.name == name)
                .cloned()
        };
        match schema {
            Some(Some(s)) => Ok(find(s)),
            Some(None) => Err(Error::new(
                code::UNDEFINED_SCHEMA,
                format!("schema \"{}\" does not exist", parts[parts.len() - 2]),
            )),
            None => Ok(self
                .effective_path(search_path)
                .into_iter()
                .find_map(find)),
        }
    }

    /// Whether a family's or class's name resolves to it through the search path
    /// (OpfamilyIsVisible / OpclassIsVisible).
    pub fn opfamily_visible(&self, f: &FamilyInfo, search_path: &[String]) -> bool {
        let all = self.opfamilies_all();
        self.effective_path(search_path).into_iter().find(|s| {
            all.iter()
                .any(|x| x.method == f.method && x.schema == *s && x.name == f.name)
        }) == Some(f.schema)
    }

    pub fn opclass_visible(&self, c: &ClassInfo, search_path: &[String]) -> bool {
        let all = self.opclasses_all();
        self.effective_path(search_path).into_iter().find(|s| {
            all.iter()
                .any(|x| x.method == c.method && x.schema == *s && x.name == c.name)
        }) == Some(c.schema)
    }

    /// GetDefaultOpClass: the default operator class of access method `am` for a key of
    /// type `ty` (0 if none): a made one for the type, else Postgres's.
    pub fn default_opclass(&self, am: u32, ty: &crate::types::DataType) -> u32 {
        let oid = super::system::type_oid(self, ty);
        if let Some(c) = self
            .opclasses
            .values()
            .find(|c| c.method == am && c.default && c.intype == oid)
        {
            return c.oid;
        }
        // (GetDefaultOpClass: a class for a polymorphic type the type is of: btree_gin's
        // enum_ops is for anyenum.)
        use crate::types::DataType as T;
        let poly = match ty {
            T::Enum(..) => Some(3500),
            T::Array(_) => Some(2277),
            T::Range(_) => Some(3831),
            _ => None,
        };
        if let Some(p) = poly
            && super::system::default_opclass(am, ty) == 0
            && let Some(c) = self.opclasses.values().find(|c| c.method == am && c.default && c.intype == p)
        {
            return c.oid;
        }
        super::system::default_opclass(am, ty)
    }

    /// The hash support function (FUNCTION 2 for the class's input type) of hash
    /// partition key column `k` of `t`, when its operator class is a made one and the
    /// function a made function.
    pub fn hash_support_fn(
        &self,
        t: &super::Table,
        k: usize,
    ) -> Option<std::sync::Arc<super::Function>> {
        self.functions.get(&self.hash_support_oid(t, k)?).cloned()
    }

    /// The comparison function (FUNCTION 1 for the class's input type) of range or list
    /// partition key column `k` when its operator class is a made btree one whose function
    /// is a made one.
    pub fn btree_support_function(&self, t: &super::Table, k: usize) -> Option<u32> {
        let key = t.partkey.as_ref()?;
        if key.strategy == super::PartStrategy::Hash {
            return None;
        }
        let name = key.cols.get(k)?.opclass.as_ref()?;
        let c = self
            .opclasses
            .values()
            .find(|c| c.method == 403 && &c.name == name)?;
        let m = self.opmembers.values().find(|m| {
            m.func && m.family == c.family && m.number == 1 && m.left == c.intype && m.right == c.intype
        })?;
        self.functions.contains_key(&m.object).then_some(m.object)
    }

    /// The hash support function (FUNCTION 2 for the class's input type) of hash partition
    /// key column `k` when its operator class is a made one: a made or a built-in function.
    pub fn hash_support_oid(&self, t: &super::Table, k: usize) -> Option<u32> {
        let key = t.partkey.as_ref()?;
        if key.strategy != super::PartStrategy::Hash {
            return None;
        }
        let name = key.cols.get(k)?.opclass.as_ref()?;
        let c = self
            .opclasses
            .values()
            .find(|c| c.method == 405 && &c.name == name)?;
        let m = self.opmembers.values().find(|m| {
            m.func && m.family == c.family && m.number == 2 && m.left == c.intype && m.right == c.intype
        })?;
        Some(m.object)
    }

    /// Every cast: Postgres's (as pg_cast rows: oid, source, target, func, context,
    /// method) and the made ones.
    pub fn casts_all(&self) -> Vec<Cast> {
        let mut out: Vec<Cast> = rows(SysTable::PgCast)
            .iter()
            .map(|r| Cast {
                oid: oid_at(r, 0),
                source: oid_at(r, 1),
                target: oid_at(r, 2),
                func: oid_at(r, 3),
                context: char_at(r, 4),
                method: char_at(r, 5),
                incast: 0,
                outcast: 0,
            })
            .collect();
        out.extend(self.casts.values().map(|c| (**c).clone()));
        out
    }

    /// The made cast from `source` to `target`.
    pub fn user_cast(&self, source: u32, target: u32) -> Option<&Cast> {
        self.casts
            .values()
            .map(|c| &**c)
            .find(|c| c.source == source && c.target == target)
    }

    /// Every conversion: Postgres's and the made ones.
    pub fn conversions_all(&self) -> Vec<Conversion> {
        let mut out: Vec<Conversion> = rows(SysTable::PgConversion)
            .iter()
            .map(|r| Conversion {
                oid: oid_at(r, 0),
                name: text_at(r, 1).to_string(),
                schema: oid_at(r, 2),
                owner: oid_at(r, 3),
                for_encoding: int_at(r, 4) as i32,
                to_encoding: int_at(r, 5) as i32,
                proc: oid_at(r, 6),
                default: bool_at(r, 7),
            })
            .collect();
        out.extend(self.conversions.values().map(|c| (**c).clone()));
        out
    }

    /// FindConversionByName.
    pub fn find_conversion(&self, parts: &[String], search_path: &[String]) -> Result<Option<Conversion>> {
        let (schema, name) = self.split_name(parts)?;
        let all = self.conversions_all();
        let find = |s: u32| all.iter().find(|c| c.schema == s && c.name == name).cloned();
        match schema {
            Some(Some(s)) => Ok(find(s)),
            Some(None) => Err(Error::new(
                code::UNDEFINED_SCHEMA,
                format!("schema \"{}\" does not exist", parts[parts.len() - 2]),
            )),
            None => Ok(self
                .effective_path(search_path)
                .into_iter()
                .find_map(find)),
        }
    }

    /// Every procedural language: (oid, name, owner, is PL, trusted, handler, inline,
    /// validator).
    pub fn languages_all(&self) -> Vec<Language> {
        let mut out: Vec<Language> = [
            (12, "internal", false, false),
            (13, "c", false, false),
            (14, "sql", false, true),
            (13681, "plpgsql", true, true),
        ]
        .into_iter()
        .map(|(oid, name, pl, trusted)| {
            let (handler, inline, validator) = if pl {
                (13678, 13679, 13680)
            } else {
                (0, 0, 2234 + oid)
            };
            Language {
                oid,
                name: name.to_string(),
                owner: OWNER,
                trusted,
                handler,
                inline,
                validator,
            }
        })
        .collect();
        out.extend(self.languages.values().map(|l| (**l).clone()));
        out
    }

    pub fn language_by_name(&self, name: &str) -> Option<Language> {
        self.languages_all().into_iter().find(|l| l.name == name)
    }

    pub fn language(&self, oid: u32) -> Option<Language> {
        self.languages_all().into_iter().find(|l| l.oid == oid)
    }
}

fn class_info(c: &std::sync::Arc<OpClass>) -> ClassInfo {
    ClassInfo {
        oid: c.oid,
        method: c.method,
        name: c.name.clone(),
        schema: c.schema,
        owner: c.owner,
        family: c.family,
        intype: c.intype,
        default: c.default,
        keytype: c.keytype,
    }
}

/// Whether an object is one of Postgres's own (in pg_catalog).
pub fn builtin_schema(schema: u32) -> bool {
    schema == PG_CATALOG_OID
}
