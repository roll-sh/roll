//! Object identities (`getObjectIdentityParts`): what `pg_identify_object` and
//! `pg_identify_object_as_address` return, and `pg_get_object_address`, which goes back from
//! the parts to an object's address.

use super::depend::{
    ATTRDEF, Addr, CAST, CONSTRAINT, EXTENSION, LANGUAGE, NAMESPACE, OPERATOR, PLPGSQL_EXT,
    POLICY, PROC, REL, REWRITE, STATEXT, TRIGGER, TYPE, ts_class, ts_kind,
};
use super::names::CatalogNames;
use super::system::{SysTable, domain_constraint_oid, type_oid};
use super::{Catalog, quote_ident};
use crate::error::{Error, Result, code};
use crate::types::DataType;
use crate::types::text::NameResolver;

pub const AUTHID: u32 = 1260;
/// pg_parameter_acl.
pub const PARAMETER_ACL: u32 = 6243;
pub const DATABASE: u32 = 1262;

/// An object's identity: its type, schema, name (for objects named by one), the identity
/// string, and the name and argument lists `pg_get_object_address` takes back. Only the type
/// for an object that doesn't exist.
#[derive(Default)]
pub struct Identity {
    pub kind: String,
    pub schema: Option<String>,
    pub name: Option<String>,
    pub identity: Option<String>,
    pub names: Option<Vec<String>>,
    pub args: Option<Vec<String>>,
}

fn qualify(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// `format_type_be_qualified`: user types schema-qualified; built-ins too, except those with
/// SQL-standard names.
pub fn qualified_type(cat: &Catalog, oid: u32) -> Option<String> {
    if let Some((schema, name, array)) = cat.user_type_name(oid) {
        let n = qualify(cat.schema_name(schema), name);
        return Some(if array { format!("{n}[]") } else { n });
    }
    DataType::from_oid(oid)
        .map(|t| builtin_qualified(&t))
        .or_else(|| {
            let (schema, name, array) = super::system::builtin_type_name(oid)?;
            let n = qualify(cat.schema_name(schema), &name);
            Some(if array { format!("{n}[]") } else { n })
        })
}

fn builtin_qualified(t: &DataType) -> String {
    if let DataType::Array(e) = t {
        return format!("{}[]", builtin_qualified(e));
    }
    const STANDARD: [&str; 16] = [
        "bit",
        "boolean",
        "character",
        "real",
        "double precision",
        "smallint",
        "integer",
        "bigint",
        "numeric",
        "interval",
        "time without time zone",
        "time with time zone",
        "timestamp without time zone",
        "timestamp with time zone",
        "bit varying",
        "character varying",
    ];
    let d = t.display();
    if STANDARD.contains(&d.as_str()) {
        d
    } else if d.starts_with('"') {
        format!("pg_catalog.{d}")
    } else {
        format!("pg_catalog.{}", quote_ident(&d))
    }
}

/// `format_type_be` for messages: a type's name, its schema left out.
fn type_display(cat: &Catalog, oid: u32) -> String {
    match cat.user_type_name(oid) {
        Some((_, name, array)) => {
            if array {
                format!("{}[]", quote_ident(name))
            } else {
                quote_ident(name)
            }
        }
        None => DataType::from_oid(oid)
            .map(|t| t.display())
            .or_else(|| {
                super::system::builtin_type_name(oid).map(|(_, n, a)| {
                    if a {
                        format!("{}[]", quote_ident(&n))
                    } else {
                        quote_ident(&n)
                    }
                })
            })
            .unwrap_or_else(|| oid.to_string()),
    }
}

fn qualified_data_type(cat: &Catalog, t: &DataType, domain: Option<u32>) -> String {
    let oid = domain.unwrap_or_else(|| type_oid(cat, t));
    qualified_type(cat, oid).unwrap_or_else(|| t.display())
}

/// A relation's (kind, schema, name), as `getRelationTypeDescription` names the kind.
fn relation(cat: &Catalog, oid: u32) -> Option<(&'static str, String, String)> {
    let s = |o: u32| cat.schema_name(o).to_string();
    if let Some(t) = cat.tables.get(&oid) {
        let kind = if t.is_foreign() { "foreign table" } else { "table" };
        return Some((kind, s(t.schema), t.name.clone()));
    }
    if let Some(v) = cat.views.get(&oid) {
        return Some((
            if v.materialized {
                "materialized view"
            } else {
                "view"
            },
            s(v.schema),
            v.name.clone(),
        ));
    }
    if let Some(q) = cat.sequences.get(&oid) {
        return Some(("sequence", s(q.schema), q.name.clone()));
    }
    if let Some(ix) = cat.indexes.get(&oid) {
        return Some((
            "index",
            s(cat.index_table(ix.table).map(|t| t.schema).unwrap_or(0)),
            ix.name.clone(),
        ));
    }
    if let Some(c) = cat.composite_of_rel(oid) {
        return Some(("composite type", s(c.schema), c.name.clone()));
    }
    if let Some((t, index)) = cat.toast_owner(oid) {
        return Some((
            if index { "index" } else { "toast table" },
            s(cat.toast_schema(t)),
            t.toast_relname(index),
        ));
    }
    let t = SysTable::by_oid(oid)?;
    let schema = if t.schema() == super::PG_CATALOG_OID {
        "pg_catalog"
    } else {
        "information_schema"
    };
    Some((
        if t.is_view() { "view" } else { "table" },
        schema.into(),
        t.name().to_string(),
    ))
}

fn column(cat: &Catalog, rel: u32, attnum: i32) -> Option<String> {
    let i = usize::try_from(attnum - 1).ok()?;
    if let Some(t) = cat.tables.get(&rel) {
        return t
            .columns
            .get(i)
            .filter(|c| !c.dropped)
            .map(|c| c.name.clone());
    }
    if let Some(v) = cat.views.get(&rel) {
        return v.columns.get(i).map(|c| c.0.clone());
    }
    if let Some(c) = cat.composite_of_rel(rel) {
        return c.fields.get(i).map(|f| f.0.clone());
    }
    if let Some((_, index)) = cat.toast_owner(rel) {
        return super::TOAST_COLUMNS[..if index { 2 } else { 3 }]
            .get(i)
            .map(|c| c.to_string());
    }
    None
}

/// A table-owned object (`name on schema.table`): its identity and name list.
fn on_relation(cat: &Catalog, kind: &str, name: &str, rel: u32, schema_shown: bool) -> Identity {
    let Some((_, schema, relname)) = relation(cat, rel) else {
        return Identity {
            kind: kind.into(),
            ..Identity::default()
        };
    };
    Identity {
        kind: kind.into(),
        schema: schema_shown.then(|| schema.clone()),
        name: None,
        identity: Some(format!(
            "{} on {}",
            quote_ident(name),
            qualify(&schema, &relname)
        )),
        names: Some(vec![schema, relname, name.to_string()]),
        args: Some(Vec::new()),
    }
}

/// A builtin cast's oid (as pg_cast lists it) and its source and target types.
fn cast(cat: &Catalog, oid: u32) -> Option<(String, String)> {
    if let Some(c) = cat.casts.get(&oid) {
        return Some((qualified_type(cat, c.source)?, qualified_type(cat, c.target)?));
    }
    if let Some((_, from, to)) = super::system::builtin_casts()
        .into_iter()
        .find(|c| c.0 == oid)
    {
        return Some((builtin_qualified(&from), builtin_qualified(&to)));
    }
    let r = cat
        .ranges
        .values()
        .find(|r| r.fn_oids.get(5) == Some(&oid))?;
    Some((
        qualified_type(cat, r.oid)?,
        qualified_type(cat, r.multi_oid)?,
    ))
}

/// `getObjectIdentityParts` (with `getObjectTypeDescription` for the type).
pub fn identify(cat: &Catalog, (class, oid, sub): Addr) -> Result<Identity> {
    let missing = |kind: &str| {
        Ok(Identity {
            kind: kind.into(),
            ..Identity::default()
        })
    };
    let named = |kind: &str, name: &str| Identity {
        kind: kind.into(),
        schema: None,
        name: Some(name.into()),
        identity: Some(quote_ident(name)),
        names: Some(vec![name.into()]),
        args: Some(Vec::new()),
    };
    match class {
        REL => {
            let Some((kind, schema, name)) = relation(cat, oid) else {
                return missing("relation");
            };
            if sub > 0 {
                let kind = format!("{kind} column");
                let Some(col) = column(cat, oid, sub) else {
                    return missing(&kind);
                };
                return Ok(Identity {
                    identity: Some(format!("{}.{}", qualify(&schema, &name), quote_ident(&col))),
                    names: Some(vec![schema.clone(), name.clone(), col]),
                    args: Some(Vec::new()),
                    kind,
                    schema: Some(schema),
                    name: Some(name),
                });
            }
            Ok(Identity {
                kind: kind.into(),
                identity: Some(qualify(&schema, &name)),
                names: Some(vec![schema.clone(), name.clone()]),
                args: Some(Vec::new()),
                schema: Some(schema),
                name: Some(name),
            })
        }
        TYPE => {
            let (schema, name) = if let Some((schema, name, array)) = cat.user_type_name(oid) {
                (
                    cat.schema_name(schema).to_string(),
                    if array {
                        cat.array_typname(oid, name)
                    } else {
                        name.to_string()
                    },
                )
            } else if let Some(t) = DataType::from_oid(oid) {
                ("pg_catalog".to_string(), t.typname())
            } else {
                return missing("type");
            };
            let identity = qualified_type(cat, oid);
            Ok(Identity {
                kind: "type".into(),
                schema: Some(schema),
                name: Some(name),
                names: identity.clone().map(|i| vec![i]),
                identity,
                args: Some(Vec::new()),
            })
        }
        PROC => {
            if let Some(f) = cat.functions.get(&oid) {
                let kind = if f.agg.is_some() {
                    "aggregate"
                } else if f.procedure {
                    "procedure"
                } else {
                    "function"
                };
                let schema = cat.schema_name(f.schema).to_string();
                let args: Vec<String> = (0..f.args.len())
                    .map(|i| qualified_data_type(cat, &f.args[i].1, f.arg_domain(i)))
                    .collect();
                return Ok(Identity {
                    kind: kind.into(),
                    identity: Some(format!("{}({})", qualify(&schema, &f.name), args.join(","))),
                    names: Some(vec![schema.clone(), f.name.clone()]),
                    args: Some(args),
                    schema: Some(schema),
                    name: None,
                });
            }
            let reg = crate::sql::func::registry();
            let builtin = reg
                .funcs
                .values()
                .flatten()
                .find(|f| f.oid == oid)
                .map(|f| ("function", f.name.to_string(), f.args.clone()));
            let agg = || {
                let aggs = crate::exec::agg::agg_registry();
                aggs.by_name
                    .values()
                    .flatten()
                    .find(|d| d.sig.oid == oid)
                    .map(|d| ("aggregate", d.sig.name.to_string(), d.sig.args.clone()))
            };
            let Some((kind, name, types)) = builtin.or_else(agg) else {
                return missing("routine");
            };
            let args: Vec<String> = types.iter().map(builtin_qualified).collect();
            Ok(Identity {
                kind: kind.into(),
                schema: Some("pg_catalog".into()),
                name: None,
                identity: Some(format!(
                    "{}({})",
                    qualify("pg_catalog", &name),
                    args.join(",")
                )),
                names: Some(vec!["pg_catalog".into(), name]),
                args: Some(args),
            })
        }
        NAMESPACE => match cat.schemas.get(&oid) {
            Some(s) => Ok(named("schema", &s.name)),
            None => missing("schema"),
        },
        CONSTRAINT => {
            for t in cat.tables.values() {
                if let Some(c) = t.constraints.iter().find(|c| c.oid == oid) {
                    return Ok(on_relation(cat, "table constraint", &c.name, t.oid, true));
                }
            }
            for d in cat.domains.values() {
                let not_null = (d.not_null && domain_constraint_oid(d, 63) == oid).then(|| {
                    d.not_null_name
                        .clone()
                        .unwrap_or_else(|| format!("{}_not_null", d.name))
                });
                let check = || {
                    d.checks
                        .iter()
                        .enumerate()
                        .find(|(i, _)| domain_constraint_oid(d, *i) == oid)
                        .map(|(_, c)| c.0.clone())
                };
                if let Some(name) = not_null.or_else(check) {
                    let ty = qualified_type(cat, d.oid).unwrap_or_default();
                    return Ok(Identity {
                        kind: "domain constraint".into(),
                        schema: Some(cat.schema_name(d.schema).to_string()),
                        name: None,
                        identity: Some(format!("{} on {ty}", quote_ident(&name))),
                        names: Some(vec![ty]),
                        args: Some(vec![name]),
                    });
                }
            }
            missing("constraint")
        }
        ATTRDEF => {
            for t in cat.tables.values() {
                for (_, c) in t.live_columns() {
                    if (c.default.is_some() || c.generated.is_some()) && c.def_oid == oid {
                        let schema = cat.schema_name(t.schema).to_string();
                        return Ok(Identity {
                            kind: "default value".into(),
                            identity: Some(format!(
                                "for {}.{}",
                                qualify(&schema, &t.name),
                                quote_ident(&c.name)
                            )),
                            names: Some(vec![schema, t.name.clone(), c.name.clone()]),
                            args: Some(Vec::new()),
                            ..Identity::default()
                        });
                    }
                }
            }
            missing("default value")
        }
        REWRITE => match cat.views.values().find(|v| v.rule_oid() == oid) {
            Some(v) => Ok(on_relation(cat, "rule", "_RETURN", v.oid, false)),
            None => match cat.rules.get(&oid) {
                Some(r) => Ok(on_relation(cat, "rule", &r.name, r.table, false)),
                None => missing("rule"),
            },
        },
        super::depend::EVENT_TRIGGER => match cat.event_triggers.get(&oid) {
            Some(e) => Ok(named("event trigger", &e.name)),
            None => missing("event trigger"),
        },
        TRIGGER => match cat.triggers.get(&oid) {
            Some(tg) => Ok(on_relation(cat, "trigger", &tg.name, tg.table, false)),
            None => match crate::exec::trigger::ri_trigger(cat, oid) {
                Some(r) => Ok(on_relation(
                    cat,
                    "trigger",
                    &r.trig.name,
                    r.trig.table,
                    false,
                )),
                None => missing("trigger"),
            },
        },
        POLICY => match cat.policies.get(&oid) {
            Some(p) => Ok(on_relation(cat, "policy", &p.name, p.table, false)),
            None => missing("policy"),
        },
        LANGUAGE => match cat.language(oid) {
            Some(l) => Ok(named("language", &l.name)),
            None => missing("language"),
        },
        super::depend::OPCLASS | super::depend::OPFAMILY => {
            let family = class == super::depend::OPFAMILY;
            let kind = if family { "operator family" } else { "operator class" };
            let found = if family {
                cat.opfamily_info(oid).map(|f| (f.schema, f.name, f.method))
            } else {
                cat.opclass_info(oid).map(|c| (c.schema, c.name, c.method))
            };
            let Some((ns, name, am)) = found else {
                return missing(kind);
            };
            let schema = cat.schema_name(ns).to_string();
            let am = cat.access_method(am).map(|a| a.name).unwrap_or_default();
            Ok(Identity {
                kind: kind.into(),
                identity: Some(format!("{} USING {}", qualify(&schema, &name), quote_ident(&am))),
                names: Some(vec![am, schema.clone(), name.clone()]),
                args: Some(Vec::new()),
                schema: Some(schema),
                name: Some(name),
            })
        }
        super::depend::AMOP | super::depend::AMPROC => {
            let func = class == super::depend::AMPROC;
            let kind = if func {
                "function of access method"
            } else {
                "operator of access method"
            };
            let Some(m) = cat.opmember_info(func, oid) else {
                return missing(kind);
            };
            let Some(f) = cat.opfamily_info(m.family) else {
                return missing(kind);
            };
            let schema = cat.schema_name(f.schema).to_string();
            let am = cat.access_method(f.method).map(|a| a.name).unwrap_or_default();
            let ty = |t: u32| qualified_type(cat, t).unwrap_or_else(|| t.to_string());
            let (l, r) = (ty(m.left), ty(m.right));
            Ok(Identity {
                kind: kind.into(),
                identity: Some(format!(
                    "{} {} ({l}, {r}) of {} USING {}",
                    if func { "function" } else { "operator" },
                    m.number,
                    qualify(&schema, &f.name),
                    quote_ident(&am)
                )),
                names: Some(vec![am, schema, f.name, m.number.to_string()]),
                args: Some(vec![l, r]),
                ..Identity::default()
            })
        }
        super::depend::FDW => match cat.fdws.get(&oid) {
            Some(f) => Ok(named("foreign-data wrapper", &f.name)),
            None => missing("foreign-data wrapper"),
        },
        super::depend::SERVER => match cat.servers.get(&oid) {
            Some(x) => Ok(named("server", &x.name)),
            None => missing("server"),
        },
        super::depend::USER_MAPPING => {
            let Some(m) = cat.user_mappings.get(&oid) else {
                return missing("user mapping");
            };
            let user = if m.user == 0 {
                "public".to_string()
            } else {
                cat.roles.get(&m.user).map(|r| r.name.clone()).unwrap_or_default()
            };
            let srv = cat.servers.get(&m.server).map(|x| x.name.clone()).unwrap_or_default();
            Ok(Identity {
                kind: "user mapping".into(),
                identity: Some(format!("{} on server {srv}", quote_ident(&user))),
                names: Some(vec![user]),
                args: Some(vec![srv]),
                ..Identity::default()
            })
        }
        super::depend::AM => match cat.access_method(oid) {
            Some(a) => Ok(named("access method", &a.name)),
            None => missing("access method"),
        },
        super::depend::CONVERSION => match cat.conversions_all().into_iter().find(|c| c.oid == oid) {
            Some(c) => {
                let schema = cat.schema_name(c.schema).to_string();
                Ok(Identity {
                    kind: "conversion".into(),
                    identity: Some(qualify(&schema, &c.name)),
                    names: Some(vec![schema.clone(), c.name.clone()]),
                    args: Some(Vec::new()),
                    schema: Some(schema),
                    name: Some(c.name),
                })
            }
            None => missing("conversion"),
        },
        super::depend::TRANSFORM => match cat.transforms.get(&oid) {
            Some(t) => {
                let ty = qualified_type(cat, t.ty).unwrap_or_default();
                let lang = cat.language(t.lang).map(|l| l.name).unwrap_or_default();
                Ok(Identity {
                    kind: "transform".into(),
                    identity: Some(format!("for {ty} language {lang}")),
                    names: Some(vec![ty]),
                    args: Some(vec![lang]),
                    ..Identity::default()
                })
            }
            None => missing("transform"),
        },
        EXTENSION => {
            if oid == PLPGSQL_EXT {
                return Ok(named("extension", "plpgsql"));
            }
            match cat.extensions.iter().find(|e| e.0 == oid) {
                Some(e) => Ok(named("extension", &e.1)),
                None => missing("extension"),
            }
        }
        super::depend::TSCONFIG
        | super::depend::TSDICT
        | super::depend::TSPARSER
        | super::depend::TSTEMPLATE => {
            let kind = ts_kind(class).expect("text search class");
            match cat.ts_ident(kind, oid) {
                Some((ns, name)) => {
                    let schema = cat.schema_name(ns).to_string();
                    Ok(Identity {
                        kind: kind.noun().into(),
                        identity: Some(qualify(&schema, &name)),
                        names: Some(vec![schema.clone(), name.clone()]),
                        args: Some(Vec::new()),
                        schema: Some(schema),
                        name: Some(name),
                    })
                }
                None => missing(kind.noun()),
            }
        }
        STATEXT => match cat.statistics.get(&oid) {
            Some(s) => {
                let schema = cat.schema_name(s.schema).to_string();
                Ok(Identity {
                    kind: "statistics object".into(),
                    identity: Some(qualify(&schema, &s.name)),
                    names: Some(vec![schema.clone(), s.name.clone()]),
                    args: Some(Vec::new()),
                    schema: Some(schema),
                    name: Some(s.name.clone()),
                })
            }
            None => missing("statistics object"),
        },
        OPERATOR => match cat.operators.get(&oid) {
            Some(o) => {
                let schema = cat.schema_name(o.schema).to_string();
                let left = o
                    .left
                    .as_ref()
                    .map_or("NONE".into(), |t| qualified_data_type(cat, t, None));
                let right = qualified_data_type(cat, &o.right, None);
                Ok(Identity {
                    kind: "operator".into(),
                    identity: Some(format!(
                        "{}.{}({left},{right})",
                        quote_ident(&schema),
                        o.name
                    )),
                    names: Some(vec![schema.clone(), o.name.clone()]),
                    args: Some(vec![left, right]),
                    schema: Some(schema),
                    name: None,
                })
            }
            None => {
                // A built-in one (pg_catalog's pg_operator rows).
                use super::static_rows::{oid_at, rows, text_at};
                let all = rows(SysTable::PgOperator);
                let Some(r) = all.iter().find(|r| oid_at(r, 0) == oid) else {
                    return missing("operator");
                };
                let side = |t: u32| match DataType::from_oid(t) {
                    _ if t == 0 => "NONE".to_string(),
                    Some(dt) => qualified_data_type(cat, &dt, None),
                    None => type_display(cat, t),
                };
                let (left, right) = (side(oid_at(r, 7)), side(oid_at(r, 8)));
                let name = text_at(r, 1).to_string();
                Ok(Identity {
                    kind: "operator".into(),
                    identity: Some(format!("pg_catalog.{name}({left},{right})")),
                    names: Some(vec!["pg_catalog".into(), name]),
                    args: Some(vec![left, right]),
                    schema: Some("pg_catalog".into()),
                    name: None,
                })
            }
        },
        CAST => match cast(cat, oid) {
            Some((from, to)) => Ok(Identity {
                kind: "cast".into(),
                identity: Some(format!("({from} AS {to})")),
                names: Some(vec![from]),
                args: Some(vec![to]),
                ..Identity::default()
            }),
            None => missing("cast"),
        },
        AUTHID => match cat.roles.get(&oid) {
            Some(r) => Ok(named("role", &r.name)),
            None => missing("role"),
        },
        super::depend::COLLATION => match crate::types::collation::def_by_oid(cat, oid) {
            Some(d) => {
                let schema = cat.schema_name(d.namespace).to_string();
                Ok(Identity {
                    kind: "collation".into(),
                    identity: Some(qualify(&schema, &d.name)),
                    names: Some(vec![schema.clone(), d.name.clone()]),
                    args: Some(Vec::new()),
                    schema: Some(schema),
                    name: Some(quote_ident(&d.name)),
                })
            }
            None => missing("collation"),
        },
        super::shdepend::DEFACL => match cat.default_acls.iter().find(|d| d.oid == oid) {
            Some(d) => {
                let Some(user) = cat.roles.get(&d.role).map(|r| r.name.clone()) else {
                    return missing("default acl");
                };
                let schema = (d.schema != 0).then(|| cat.schema_name(d.schema).to_string());
                let on = match d.objtype {
                    'r' => " on tables",
                    'S' => " on sequences",
                    'f' => " on functions",
                    'T' => " on types",
                    'n' => " on schemas",
                    'L' => " on large objects",
                    _ => "",
                };
                let mut identity = format!("for role {}", quote_ident(&user));
                if let Some(s) = &schema {
                    identity.push_str(&format!(" in schema {}", quote_ident(s)));
                }
                identity.push_str(on);
                let mut names = vec![user];
                names.extend(schema);
                Ok(Identity {
                    kind: "default acl".into(),
                    identity: Some(identity),
                    names: Some(names),
                    args: Some(vec![d.objtype.to_string()]),
                    ..Identity::default()
                })
            }
            None => missing("default acl"),
        },
        1261 => match cat.members.iter().find(|m| m.oid == oid) {
            Some(m) => {
                let name = |r: u32| cat.roles.get(&r).map_or_else(String::new, |x| x.name.clone());
                Ok(Identity {
                    kind: "role membership".into(),
                    identity: Some(format!(
                        "membership of role {} in role {}",
                        quote_ident(&name(m.member)),
                        quote_ident(&name(m.role))
                    )),
                    ..Identity::default()
                })
            }
            None => missing("role membership"),
        },
        1262 => missing("database"),
        2613 => {
            if oid == 0 {
                return missing("large object");
            }
            Ok(Identity {
                kind: "large object".into(),
                identity: Some(oid.to_string()),
                names: Some(vec![oid.to_string()]),
                args: Some(Vec::new()),
                ..Identity::default()
            })
        }
        1213 => match cat.tablespace_name(oid) {
            Some(n) => Ok(named("tablespace", &n)),
            None => missing("tablespace"),
        },
        super::depend::PUBLICATION => match cat.publications.get(&oid) {
            Some(p) => Ok(named("publication", &p.name)),
            None => missing("publication"),
        },
        super::depend::SUBSCRIPTION => match cat.subscriptions.get(&oid) {
            Some(x) => Ok(named("subscription", &x.name)),
            None => missing("subscription"),
        },
        super::depend::PUBLICATION_REL => {
            let found = cat.publications.values().find_map(|p| {
                p.rels.iter().find(|r| r.oid == oid).map(|r| (p, r))
            });
            let Some((p, r)) = found else {
                return missing("publication relation");
            };
            let Some((_, schema, name)) = relation(cat, r.rel) else {
                return missing("publication relation");
            };
            Ok(Identity {
                kind: "publication relation".into(),
                identity: Some(format!("{} in publication {}", qualify(&schema, &name), quote_ident(&p.name))),
                names: Some(vec![schema, name]),
                args: Some(vec![p.name.clone()]),
                ..Identity::default()
            })
        }
        super::depend::PUBLICATION_NAMESPACE => {
            let found = cat.publications.values().find_map(|p| {
                p.schemas.iter().find(|x| x.oid == oid).map(|x| (p, x))
            });
            let Some((p, x)) = found else {
                return missing("publication namespace");
            };
            let schema = cat.schema_name(x.schema).to_string();
            Ok(Identity {
                kind: "publication namespace".into(),
                identity: Some(format!("{} in publication {}", quote_ident(&schema), quote_ident(&p.name))),
                names: Some(vec![schema]),
                args: Some(vec![p.name.clone()]),
                ..Identity::default()
            })
        }
        PARAMETER_ACL => match cat.parameters.iter().find(|p| p.0 == oid) {
            Some((_, name)) => Ok(Identity {
                kind: "parameter ACL".into(),
                identity: Some(name.clone()),
                names: Some(vec![name.clone()]),
                args: Some(Vec::new()),
                ..Identity::default()
            }),
            None => missing("parameter ACL"),
        },
        _ => Err(Error::new(
            code::INTERNAL_ERROR,
            format!("unsupported object class: {class}"),
        )),
    }
}

/// The object types `pg_get_object_address` knows (Postgres's `ObjectTypeMap`), and whether
/// it supports each.
const OBJECT_TYPES: [(&str, bool); 59] = [
    ("table", true),
    ("index", true),
    ("sequence", true),
    ("toast table", false),
    ("view", true),
    ("materialized view", true),
    ("composite type", false),
    ("foreign table", true),
    ("table column", true),
    ("index column", false),
    ("sequence column", false),
    ("toast table column", false),
    ("view column", false),
    ("materialized view column", false),
    ("composite type column", false),
    ("foreign table column", true),
    ("aggregate", true),
    ("function", true),
    ("procedure", true),
    ("type", true),
    ("cast", true),
    ("collation", true),
    ("table constraint", true),
    ("domain constraint", true),
    ("conversion", true),
    ("default value", true),
    ("language", true),
    ("large object", true),
    ("operator class", true),
    ("operator family", true),
    ("access method", true),
    ("operator of access method", true),
    ("function of access method", true),
    ("rule", true),
    ("schema", true),
    ("text search parser", true),
    ("text search dictionary", true),
    ("text search template", true),
    ("text search configuration", true),
    ("role", true),
    ("database", true),
    ("tablespace", true),
    ("foreign-data wrapper", true),
    ("server", true),
    ("user mapping", true),
    ("default acl", true),
    ("extension", true),
    ("event trigger", true),
    ("policy", true),
    ("publication", true),
    ("statistics object", true),
    ("operator", true),
    ("trigger", true),
    ("routine", true),
    ("transform", true),
    ("subscription", true),
    ("publication namespace", true),
    ("publication relation", true),
    ("parameter ACL", true),
];

/// Whether `pg_get_object_address` knows the type name.
pub fn known_object_type(kind: &str) -> bool {
    OBJECT_TYPES.iter().any(|t| t.0 == kind)
}

/// A three-part name: another database's object.
fn cross_db(list: &[String], quoted: bool) -> Error {
    let n = list.join(".");
    Error::new(
        code::FEATURE_NOT_SUPPORTED,
        if quoted {
            format!("cross-database references are not implemented: \"{n}\"")
        } else {
            format!("cross-database references are not implemented: {n}")
        },
    )
}

fn invalid(msg: String) -> Error {
    Error::new(code::INVALID_PARAMETER_VALUE, msg)
}

fn exactly(list: &[String], n: usize, what: &str) -> Result<()> {
    if list.len() != n {
        return Err(invalid(format!("{what} list length must be exactly {n}")));
    }
    Ok(())
}

/// `pg_get_object_address`: the address of the object `kind` names by `names` / `args`.
pub fn object_address(
    cat: &std::sync::Arc<Catalog>,
    sp: &[String],
    kind: &str,
    names: &[String],
    args: &[String],
) -> Result<Addr> {
    match OBJECT_TYPES.iter().find(|t| t.0 == kind) {
        None => return Err(invalid(format!("unrecognized object type \"{kind}\""))),
        Some((_, false)) => return Err(invalid(format!("unsupported object type \"{kind}\""))),
        Some(_) => {}
    }
    let resolver = CatalogNames::new(cat.clone(), sp.to_vec());
    let type_of = |name: &str| resolver.type_oid(name);
    // A relation named by a qualified name list.
    let rel = |list: &[String]| -> Result<(u32, String)> {
        let (schema, name) = match list {
            [r] => (None, r.as_str()),
            [s, r] => {
                if cat.schema_by_name(s).is_none() {
                    return Err(Error::new(
                        code::UNDEFINED_SCHEMA,
                        format!("schema \"{s}\" does not exist"),
                    ));
                }
                (Some(s.as_str()), r.as_str())
            }
            [_, _, _] => return Err(cross_db(list, true)),
            _ => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "improper relation name (too many dotted names): {}",
                        list.join(".")
                    ),
                ));
            }
        };
        let oid = cat
            .find(schema, name, sp)
            .map(|r| r.oid())
            .ok_or_else(|| super::no_such_relation(schema, name))?;
        Ok((oid, name.to_string()))
    };
    let relkind = |oid: u32, name: &str, kind: &str| -> Result<()> {
        let ok = match kind {
            "table" => cat.tables.get(&oid).is_some_and(|t| !t.is_foreign()),
            "foreign table" => cat.tables.get(&oid).is_some_and(|t| t.is_foreign()),
            "index" => cat.indexes.contains_key(&oid),
            "sequence" => cat.sequences.contains_key(&oid),
            "view" => cat.views.get(&oid).is_some_and(|v| !v.materialized),
            "materialized view" => cat.views.get(&oid).is_some_and(|v| v.materialized),
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(super::wrong_kind(name, kind))
        }
    };
    match kind {
        "table" | "index" | "sequence" | "view" | "materialized view" | "foreign table" => {
            let (oid, name) = rel(names)?;
            relkind(oid, &name, kind)?;
            Ok((REL, oid, 0))
        }
        "table column" | "foreign table column" => {
            let Some((col, rest)) = names.split_last().filter(|(_, r)| !r.is_empty()) else {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "column name must be qualified",
                ));
            };
            let (oid, name) = rel(rest)?;
            let i = cat.tables.get(&oid).and_then(|t| t.column_index(col));
            let i = i.ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_COLUMN,
                    format!("column \"{col}\" of relation \"{name}\" does not exist"),
                )
            })?;
            Ok((REL, oid, i as i32 + 1))
        }
        "type" => {
            exactly(names, 1, "name")?;
            Ok((TYPE, type_of(&names[0])?, 0))
        }
        "domain constraint" => {
            exactly(names, 1, "name")?;
            exactly(args, 1, "argument")?;
            let oid = type_of(&names[0])?;
            let d = cat.domains.get(&oid).ok_or_else(|| {
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a domain", names[0]),
                )
            })?;
            if d.not_null
                && d.not_null_name
                    .clone()
                    .unwrap_or_else(|| format!("{}_not_null", d.name))
                    == args[0]
            {
                return Ok((CONSTRAINT, domain_constraint_oid(d, 63), 0));
            }
            let i = d.checks.iter().position(|c| c.0 == args[0]);
            let i = i.ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!(
                        "constraint \"{}\" for domain {} does not exist",
                        args[0], d.name
                    ),
                )
            })?;
            Ok((CONSTRAINT, domain_constraint_oid(d, i), 0))
        }
        "cast" => {
            exactly(names, 1, "name")?;
            exactly(args, 1, "argument")?;
            let (from, to) = (type_of(&names[0])?, type_of(&args[0])?);
            let builtin = super::system::builtin_casts()
                .into_iter()
                .find(|c| c.1.oid() == from && c.2.oid() == to)
                .map(|c| c.0);
            let range = || {
                cat.ranges
                    .values()
                    .find(|r| r.oid == from && r.multi_oid == to)
                    .and_then(|r| r.fn_oids.get(5).copied())
            };
            let shown = |o: u32| qualified_type(cat, o).unwrap_or_default();
            let made = || cat.user_cast(from, to).map(|c| c.oid);
            builtin.or_else(range).or_else(made).map(|o| (CAST, o, 0)).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!(
                        "cast from type {} to type {} does not exist",
                        shown(from),
                        shown(to)
                    ),
                )
            })
        }
        "function" | "procedure" | "aggregate" | "routine" => {
            let (schema, fname) = match names {
                [f] => (None, f.as_str()),
                [s, f] => (Some(s.as_str()), f.as_str()),
                [_, _, _] => return Err(cross_db(names, false)),
                _ => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "improper qualified name (too many dotted names): {}",
                            names.join(".")
                        ),
                    ));
                }
            };
            let types = args
                .iter()
                .map(|a| type_of(a))
                .collect::<Result<Vec<u32>>>()?;
            let schemas: Vec<u32> = match schema {
                Some(s) => cat
                    .schema_by_name(s)
                    .map(|s| vec![s.oid])
                    .unwrap_or_default(),
                None => cat.effective_path(sp),
            };
            let found = schemas.iter().find_map(|sc| {
                cat.functions.values().find(|f| {
                    f.schema == *sc
                        && f.name == fname
                        && f.args.len() == types.len()
                        && (0..f.args.len()).all(|i| {
                            f.arg_domain(i)
                                .unwrap_or_else(|| type_oid(cat, &f.args[i].1))
                                == types[i]
                        })
                })
            });
            let shown: Vec<String> = types.iter().map(|o| type_display(cat, *o)).collect();
            let word = if kind == "routine" { "function" } else { kind };
            let shown: Vec<String> = if kind == "aggregate" && shown.is_empty() {
                vec!["*".into()]
            } else {
                shown
            };
            // A built-in one (pg_catalog's).
            let builtin = || {
                (schemas.contains(&super::PG_CATALOG_OID))
                    .then(|| {
                        super::system::builtin_proc_sigs()
                            .iter()
                            .find(|g| g.name == fname && g.args == types)
                            .map(|g| g.oid)
                    })
                    .flatten()
            };
            let oid = match found {
                Some(f) => f.oid,
                None => builtin().ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_FUNCTION,
                        format!(
                            "{word} {}({}) does not exist",
                            names.join("."),
                            shown.join(", ")
                        ),
                    )
                })?,
            };
            Ok((PROC, oid, 0))
        }
        "operator" => {
            exactly(args, 2, "argument")?;
            let (schema, name) = match names {
                [o] => (None, o.as_str()),
                [s, o] => (Some(s.as_str()), o.as_str()),
                _ => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "improper qualified name (too many dotted names): {}",
                            names.join(".")
                        ),
                    ));
                }
            };
            let side = |a: &str| {
                if a.eq_ignore_ascii_case("none") {
                    Ok(None)
                } else {
                    type_of(a).map(Some)
                }
            };
            let (l, r) = (side(&args[0])?, side(&args[1])?);
            let schemas: Vec<u32> = match schema {
                Some(s) => cat
                    .schema_by_name(s)
                    .map(|s| vec![s.oid])
                    .unwrap_or_default(),
                None => cat.effective_path(sp),
            };
            let found = schemas.iter().find_map(|sc| {
                cat.operators.values().find(|o| {
                    o.schema == *sc
                        && o.name == name
                        && o.left.as_ref().map(|t| type_oid(cat, t)) == l
                        && Some(type_oid(cat, &o.right)) == r
                })
            });
            // A built-in one (pg_catalog's pg_operator rows).
            let builtin = || {
                use super::static_rows::{oid_at, rows, text_at};
                schemas
                    .contains(&super::PG_CATALOG_OID)
                    .then(|| {
                        rows(SysTable::PgOperator)
                            .iter()
                            .find(|x| {
                                text_at(x, 1) == name
                                    && oid_at(x, 7) == l.unwrap_or(0)
                                    && oid_at(x, 8) == r.unwrap_or(0)
                            })
                            .map(|x| oid_at(x, 0))
                    })
                    .flatten()
            };
            match found.map(|o| o.oid).or_else(builtin) {
                Some(oid) => Ok((OPERATOR, oid, 0)),
                None => Err(Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!("operator does not exist: {}", names.join(".")),
                )),
            }
        }
        "table constraint" | "trigger" | "policy" | "rule" | "default value" => {
            let Some((obj, rest)) = names.split_last().filter(|(_, r)| !r.is_empty()) else {
                if kind == "default value" {
                    return Err(Error::new(code::SYNTAX_ERROR, "column name must be qualified"));
                }
                return Err(invalid("must specify relation and object name".into()));
            };
            let (oid, relname) = rel(rest)?;
            let t = cat.tables.get(&oid);
            let found = match kind {
                "table constraint" => t
                    .and_then(|t| t.constraints.iter().find(|c| c.name == *obj))
                    .map(|c| (CONSTRAINT, c.oid, 0)),
                "trigger" => cat
                    .triggers
                    .values()
                    .find(|tg| tg.table == oid && tg.name == *obj)
                    .map(|tg| (TRIGGER, tg.oid, 0)),
                "policy" => cat
                    .policies
                    .values()
                    .find(|p| p.table == oid && p.name == *obj)
                    .map(|p| (POLICY, p.oid, 0)),
                "rule" => cat
                    .views
                    .get(&oid)
                    .filter(|_| obj == "_RETURN")
                    .map(|v| (super::depend::REWRITE, v.rule_oid(), 0))
                    .or_else(|| {
                        (cat.rules.values())
                            .find(|r| r.table == oid && r.name == *obj)
                            .map(|r| (super::depend::REWRITE, r.oid, 0))
                    }),
                _ => t.and_then(|t| {
                    t.column_index(obj)
                        .filter(|&i| {
                            t.columns[i].default.is_some() || t.columns[i].generated.is_some()
                        })
                        .map(|i| (ATTRDEF, t.columns[i].def_oid, 0))
                }),
            };
            found.ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    match kind {
                        "table constraint" => format!("constraint \"{obj}\" for table \"{relname}\" does not exist"),
                        "default value" => format!("default value for column \"{obj}\" of relation \"{relname}\" does not exist"),
                        "rule" => format!("rule \"{obj}\" for relation \"{relname}\" does not exist"),
                        _ => format!("{kind} \"{obj}\" for table \"{relname}\" does not exist"),
                    },
                )
            })
        }
        "text search parser"
        | "text search dictionary"
        | "text search template"
        | "text search configuration" => {
            use super::tsearch::TsKind;
            let k = match kind {
                "text search parser" => TsKind::Parser,
                "text search dictionary" => TsKind::Dict,
                "text search template" => TsKind::Template,
                _ => TsKind::Config,
            };
            if names.len() > 2 {
                return Err(cross_db(names, false));
            }
            let oid = cat.ts_lookup(k, names, sp, false)?.unwrap_or(0);
            Ok((ts_class(k), oid, 0))
        }
        "statistics object" => {
            let (schema, name) = match names {
                [n] => (None, n.as_str()),
                [s, n] => (Some(s.as_str()), n.as_str()),
                _ => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "improper qualified name (too many dotted names): {}",
                            names.join(".")
                        ),
                    ));
                }
            };
            let s = cat.find_statistics(schema, name, sp)?;
            s.map(|s| (STATEXT, s.oid, 0)).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("statistics object \"{}\" does not exist", names.join(".")),
                )
            })
        }
        "schema" | "language" | "extension" | "role" | "database" => {
            exactly(names, 1, "name")?;
            let n = names[0].as_str();
            let undefined = |what: &str| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("{what} \"{n}\" does not exist"),
                )
            };
            match kind {
                "schema" => cat
                    .schema_by_name(n)
                    .map(|s| (NAMESPACE, s.oid, 0))
                    .ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_SCHEMA,
                            format!("schema \"{n}\" does not exist"),
                        )
                    }),
                "language" => cat
                    .language_by_name(n)
                    .map(|l| (LANGUAGE, l.oid, 0))
                    .ok_or_else(|| undefined("language")),
                "extension" => {
                    if n == "plpgsql" {
                        return Ok((EXTENSION, PLPGSQL_EXT, 0));
                    }
                    cat.extensions
                        .iter()
                        .find(|e| e.1 == n)
                        .map(|e| (EXTENSION, e.0, 0))
                        .ok_or_else(|| undefined("extension"))
                }
                "role" => cat
                    .role_by_name(n)
                    .map(|r| (AUTHID, r.oid, 0))
                    .ok_or_else(|| undefined("role")),
                _ => Err(Error::new(
                    code::UNDEFINED_DATABASE,
                    format!("database \"{n}\" does not exist"),
                )),
            }
        }
        "foreign-data wrapper" | "server" => {
            exactly(names, 1, "name")?;
            let n = &names[0];
            if kind == "server" {
                cat.servers
                    .values()
                    .find(|x| &x.name == n)
                    .map(|x| (super::depend::SERVER, x.oid, 0))
                    .ok_or_else(|| {
                        Error::new(code::UNDEFINED_OBJECT, format!("server \"{n}\" does not exist"))
                    })
            } else {
                cat.fdws
                    .values()
                    .find(|x| &x.name == n)
                    .map(|x| (super::depend::FDW, x.oid, 0))
                    .ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("foreign-data wrapper \"{n}\" does not exist"),
                        )
                    })
            }
        }
        "user mapping" => {
            exactly(names, 1, "name")?;
            exactly(args, 1, "argument")?;
            let user = if names[0] == "public" {
                0
            } else {
                cat.role_by_name(&names[0]).map(|r| r.oid).ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "user mapping for user \"{}\" on server \"{}\" does not exist",
                            names[0], args[0]
                        ),
                    )
                })?
            };
            let srv = cat.servers.values().find(|x| x.name == args[0]).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("server \"{}\" does not exist", args[0]),
                )
            })?;
            cat.user_mappings
                .values()
                .find(|m| m.user == user && m.server == srv.oid)
                .map(|m| (super::depend::USER_MAPPING, m.oid, 0))
                .ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "user mapping for user \"{}\" on server \"{}\" does not exist",
                            names[0], args[0]
                        ),
                    )
                })
        }
        "tablespace" | "event trigger" | "publication" | "subscription" => {
            exactly(names, 1, "name")?;
            if kind == "event trigger"
                && let Some(e) = cat.event_triggers.values().find(|e| e.name == names[0])
            {
                return Ok((super::depend::EVENT_TRIGGER, e.oid, 0));
            }
            if kind == "tablespace"
                && let Some(o) = cat.tablespace_oid(&names[0])
            {
                return Ok((1213, o, 0));
            }
            if kind == "publication"
                && let Some(p) = cat.publications.values().find(|p| p.name == names[0])
            {
                return Ok((super::depend::PUBLICATION, p.oid, 0));
            }
            if kind == "subscription"
                && let Some(x) = cat.subscriptions.values().find(|x| x.name == names[0])
            {
                return Ok((super::depend::SUBSCRIPTION, x.oid, 0));
            }
            Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("{kind} \"{}\" does not exist", names[0]),
            ))
        }
        "publication namespace" | "publication relation" => {
            let target = if kind == "publication namespace" {
                exactly(names, 1, "name")?;
                exactly(args, 1, "argument")?;
                match cat.schema_by_name(&names[0]) {
                    Some(sc) => sc.oid,
                    None => {
                        return Err(Error::new(
                            code::UNDEFINED_SCHEMA,
                            format!("schema \"{}\" does not exist", names[0]),
                        ));
                    }
                }
            } else {
                exactly(args, 1, "argument")?;
                rel(names)?.0
            };
            let Some(p) = cat.publications.values().find(|p| p.name == args[0]) else {
                return Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("publication \"{}\" does not exist", args[0]),
                ));
            };
            let found = if kind == "publication namespace" {
                p.schemas
                    .iter()
                    .find(|x| x.schema == target)
                    .map(|x| (super::depend::PUBLICATION_NAMESPACE, x.oid, 0))
            } else {
                p.rels
                    .iter()
                    .find(|x| x.rel == target)
                    .map(|x| (super::depend::PUBLICATION_REL, x.oid, 0))
            };
            found.ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    if kind == "publication namespace" {
                        format!(
                            "publication schema \"{}\" in publication \"{}\" does not exist",
                            names[0], args[0]
                        )
                    } else {
                        format!(
                            "publication relation \"{}\" in publication \"{}\" does not exist",
                            names.last().cloned().unwrap_or_default(),
                            args[0]
                        )
                    },
                )
            })
        }
        "default acl" => {
            exactly(args, 1, "argument")?;
            let objtype = args[0].chars().next().unwrap_or(' ');
            let what = match objtype {
                'r' => "relations",
                'S' => "sequences",
                'f' => "functions",
                'T' => "types",
                'n' => "schemas",
                'L' => "large objects",
                c => {
                    return Err(invalid(format!(
                        "unrecognized default ACL object type \"{c}\""
                    ))
                    .hint("Valid object types are \"r\", \"S\", \"f\", \"T\", \"n\", \"L\"."));
                }
            };
            let role = cat.role_by_name(&names[0]).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("role \"{}\" does not exist", names[0]),
                )
            })?;
            let schema = match names.get(1) {
                Some(s) => Some(cat.schema_by_name(s).ok_or_else(|| {
                    Error::new(code::UNDEFINED_SCHEMA, format!("schema \"{s}\" does not exist"))
                })?),
                None => None,
            };
            let soid = schema.map_or(0, |s| s.oid);
            cat.default_acls
                .iter()
                .find(|d| d.role == role.oid && d.schema == soid && d.objtype == objtype)
                .map(|d| (super::shdepend::DEFACL, d.oid, 0))
                .ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        match names.get(1) {
                            Some(s) => format!(
                                "default ACL for user \"{}\" in schema \"{s}\" on {what} does not exist",
                                names[0]
                            ),
                            None => format!(
                                "default ACL for user \"{}\" on {what} does not exist",
                                names[0]
                            ),
                        },
                    )
                })
        }
        "access method" => {
            exactly(names, 1, "name")?;
            cat.access_method_by_name(&names[0])
                .map(|a| (super::depend::AM, a.oid, 0))
                .ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("access method \"{}\" does not exist", names[0]),
                    )
                })
        }
        "operator class" | "operator family" => {
            if names.len() < 2 {
                return Err(invalid("name list length must be at least 2".into()));
            }
            let am = cat.index_am(&names[0])?;
            let rest = &names[1..];
            let what = kind;
            let oid = if kind == "operator family" {
                cat.find_opfamily(am.oid, rest, sp)?.map(|f| (super::depend::OPFAMILY, f.oid, 0))
            } else {
                cat.find_opclass(am.oid, rest, sp)?.map(|c| (super::depend::OPCLASS, c.oid, 0))
            };
            oid.ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!(
                        "{what} \"{}\" does not exist for access method \"{}\"",
                        rest.join("."),
                        am.name
                    ),
                )
            })
        }
        "operator of access method" | "function of access method" => {
            if names.len() < 3 {
                return Err(invalid("name list length must be at least 3".into()));
            }
            exactly(args, 2, "argument")?;
            let am = cat.index_am(&names[0])?;
            let fam_parts = &names[1..names.len() - 1];
            let f = cat.find_opfamily(am.oid, fam_parts, sp)?.ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!(
                        "operator family \"{}\" does not exist for access method \"{}\"",
                        fam_parts.join("."),
                        am.name
                    ),
                )
            })?;
            let number: i16 = names[names.len() - 1].parse().unwrap_or(-1);
            let (l, r) = (type_of(&args[0])?, type_of(&args[1])?);
            let func = kind == "function of access method";
            cat.opmembers_all(func)
                .into_iter()
                .find(|m| m.family == f.oid && m.left == l && m.right == r && m.number == number)
                .map(|m| {
                    (
                        if func { super::depend::AMPROC } else { super::depend::AMOP },
                        m.oid,
                        0,
                    )
                })
                .ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "{} {} ({}, {}) of {} does not exist",
                            if func { "function" } else { "operator" },
                            names[names.len() - 1],
                            args[0],
                            args[1],
                            super::depend::opfamily_description(cat, sp, f.oid).unwrap_or_default()
                        ),
                    )
                })
        }
        "conversion" => {
            if names.len() > 2 {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cross-database references are not implemented: {}",
                        names.join(".")
                    ),
                ));
            }
            cat.find_conversion(names, sp)?
                .map(|c| (super::depend::CONVERSION, c.oid, 0))
                .ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("conversion \"{}\" does not exist", names.join(".")),
                    )
                })
        }
        "transform" => {
            exactly(names, 1, "name")?;
            exactly(args, 1, "argument")?;
            let t = type_of(&names[0])?;
            let lang = cat.language_by_name(&args[0]).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("language \"{}\" does not exist", args[0]),
                )
            })?;
            cat.transforms
                .values()
                .find(|x| x.ty == t && x.lang == lang.oid)
                .map(|x| (super::depend::TRANSFORM, x.oid, 0))
                .ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "transform for type {} language \"{}\" does not exist",
                            type_display(cat, t),
                            args[0]
                        ),
                    )
                })
        }
        "collation" => {
            let (schema, name) = match names {
                [n] => (None, n.as_str()),
                [s, n] => (Some(s.as_str()), n.as_str()),
                _ => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "improper qualified name (too many dotted names): {}",
                            names.join(".")
                        ),
                    ));
                }
            };
            let path = match schema {
                Some(s) => cat.schema_by_name(s).map(|x| vec![x.oid]).unwrap_or_default(),
                None => cat.effective_path(sp),
            };
            let defs = crate::types::collation::static_defs()
                .iter()
                .cloned()
                .chain(cat.collations.values().map(|d| (**d).clone()));
            let mut found: Vec<crate::types::collation::CollationDef> =
                defs.filter(|d| d.name == name && path.contains(&d.namespace)).collect();
            found.sort_by_key(|d| path.iter().position(|p| *p == d.namespace));
            found
                .first()
                .map(|d| (super::depend::COLLATION, d.oid, 0))
                .ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "collation \"{}\" for encoding \"UTF8\" does not exist",
                            names.join(".")
                        ),
                    )
                })
        }
        _ => Err(invalid(format!("unsupported object type \"{kind}\""))),
    }
}
