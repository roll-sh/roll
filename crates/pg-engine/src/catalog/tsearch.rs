//! Text search objects: the built-in configurations, dictionaries, parsers and templates in
//! pg_catalog and the ones CREATE TEXT SEARCH made, and their name lookups (namespace.c's
//! get_ts_*_oid and TS*IsVisible).

use super::names::parse_qualified;
use super::{Catalog, quote_ident};
use crate::error::Result;
use crate::types::tsearch::config::{
    TsConfigDef, TsDictDef, TsLookup, TsParserDef, TsTemplateDef, builtin_configs, builtin_dicts,
    builtin_parsers, builtin_templates,
};
use crate::{bail, err};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TsKind {
    Config,
    Dict,
    Parser,
    Template,
}

impl TsKind {
    pub fn noun(self) -> &'static str {
        match self {
            TsKind::Config => "text search configuration",
            TsKind::Dict => "text search dictionary",
            TsKind::Parser => "text search parser",
            TsKind::Template => "text search template",
        }
    }
}

impl Catalog {
    pub fn ts_config(&self, oid: u32) -> Option<TsConfigDef> {
        match self.ts_configs.get(&oid) {
            Some(c) => Some((**c).clone()),
            None => builtin_configs()
                .iter()
                .find(|c| c.oid == oid && !self.ts_removed.contains(&oid))
                .cloned(),
        }
    }

    pub fn ts_dict(&self, oid: u32) -> Option<TsDictDef> {
        match self.ts_dicts.get(&oid) {
            Some(d) => Some((**d).clone()),
            None => builtin_dicts()
                .iter()
                .find(|d| d.oid == oid && !self.ts_removed.contains(&oid))
                .cloned(),
        }
    }

    pub fn ts_parser(&self, oid: u32) -> Option<TsParserDef> {
        match self.ts_parsers.get(&oid) {
            Some(p) => Some((**p).clone()),
            None => builtin_parsers()
                .iter()
                .find(|p| p.oid == oid && !self.ts_removed.contains(&oid))
                .cloned(),
        }
    }

    pub fn ts_template(&self, oid: u32) -> Option<TsTemplateDef> {
        match self.ts_templates.get(&oid) {
            Some(t) => Some((**t).clone()),
            None => builtin_templates()
                .iter()
                .find(|t| t.oid == oid && !self.ts_removed.contains(&oid))
                .cloned(),
        }
    }

    /// Every object of a kind: (oid, namespace, name), in oid order.
    pub fn ts_objects(&self, kind: TsKind) -> Vec<(u32, u32, String)> {
        fn merge<T>(
            removed: &std::collections::BTreeSet<u32>,
            builtin: &[T],
            stored: &std::collections::BTreeMap<u32, std::sync::Arc<T>>,
            key: impl Fn(&T) -> (u32, u32, String),
        ) -> Vec<(u32, u32, String)> {
            builtin
                .iter()
                .map(&key)
                .filter(|o| !removed.contains(&o.0) && !stored.contains_key(&o.0))
                .chain(stored.values().map(|x| key(x)))
                .collect()
        }
        let r = &self.ts_removed;
        let mut v = match kind {
            TsKind::Config => merge(r, builtin_configs(), &self.ts_configs, |c| {
                (c.oid, c.namespace, c.name.clone())
            }),
            TsKind::Dict => merge(r, builtin_dicts(), &self.ts_dicts, |d| {
                (d.oid, d.namespace, d.name.clone())
            }),
            TsKind::Parser => merge(r, builtin_parsers(), &self.ts_parsers, |p| {
                (p.oid, p.namespace, p.name.clone())
            }),
            TsKind::Template => merge(r, builtin_templates(), &self.ts_templates, |t| {
                (t.oid, t.namespace, t.name.clone())
            }),
        };
        v.sort_by_key(|o| o.0);
        v
    }

    /// The kind of the text search object with an oid.
    pub fn ts_kind_of(&self, oid: u32) -> Option<TsKind> {
        [
            TsKind::Config,
            TsKind::Dict,
            TsKind::Parser,
            TsKind::Template,
        ]
        .into_iter()
        .find(|k| self.ts_ident(*k, oid).is_some())
    }

    /// Store a configuration (new or changed).
    pub fn ts_put_config(&mut self, c: TsConfigDef) {
        self.ts_configs.insert(c.oid, std::sync::Arc::new(c));
    }

    pub fn ts_put_dict(&mut self, d: TsDictDef) {
        self.ts_dicts.insert(d.oid, std::sync::Arc::new(d));
    }

    pub fn ts_put_parser(&mut self, p: TsParserDef) {
        self.ts_parsers.insert(p.oid, std::sync::Arc::new(p));
    }

    pub fn ts_put_template(&mut self, t: TsTemplateDef) {
        self.ts_templates.insert(t.oid, std::sync::Arc::new(t));
    }

    /// Rename or move an object.
    pub fn ts_set_name(&mut self, kind: TsKind, oid: u32, namespace: u32, name: String) {
        match kind {
            TsKind::Config => {
                if let Some(c) = self.ts_config(oid) {
                    self.ts_put_config(TsConfigDef {
                        namespace,
                        name,
                        ..c
                    });
                }
            }
            TsKind::Dict => {
                if let Some(d) = self.ts_dict(oid) {
                    self.ts_put_dict(TsDictDef {
                        namespace,
                        name,
                        ..d
                    });
                }
            }
            TsKind::Parser => {
                if let Some(p) = self.ts_parser(oid) {
                    self.ts_put_parser(TsParserDef {
                        namespace,
                        name,
                        ..p
                    });
                }
            }
            TsKind::Template => {
                if let Some(t) = self.ts_template(oid) {
                    self.ts_put_template(TsTemplateDef {
                        namespace,
                        name,
                        ..t
                    });
                }
            }
        }
    }

    /// Remove an object (a built-in one is remembered as dropped).
    pub fn ts_remove(&mut self, kind: TsKind, oid: u32) {
        let builtin = match kind {
            TsKind::Config => {
                self.ts_configs.remove(&oid);
                builtin_configs().iter().any(|c| c.oid == oid)
            }
            TsKind::Dict => {
                self.ts_dicts.remove(&oid);
                builtin_dicts().iter().any(|d| d.oid == oid)
            }
            TsKind::Parser => {
                self.ts_parsers.remove(&oid);
                builtin_parsers().iter().any(|p| p.oid == oid)
            }
            TsKind::Template => {
                self.ts_templates.remove(&oid);
                builtin_templates().iter().any(|t| t.oid == oid)
            }
        };
        if builtin {
            self.ts_removed.insert(oid);
        }
    }

    /// The owner of a configuration or dictionary (parsers and templates have none: the
    /// bootstrap superuser).
    pub fn ts_owner(&self, kind: TsKind, oid: u32) -> u32 {
        match kind {
            TsKind::Config => self
                .ts_config(oid)
                .map_or(super::BOOTSTRAP_SUPERUSER_OID, |c| c.owner),
            TsKind::Dict => self
                .ts_dict(oid)
                .map_or(super::BOOTSTRAP_SUPERUSER_OID, |d| d.owner),
            _ => super::BOOTSTRAP_SUPERUSER_OID,
        }
    }

    /// Whether the name is the one the search path finds (`TS*IsVisible`).
    pub fn ts_visible(&self, kind: TsKind, oid: u32, search_path: &[String]) -> bool {
        let Some((_, name)) = self.ts_ident(kind, oid) else {
            return false;
        };
        self.ts_path(search_path)
            .into_iter()
            .find_map(|s| self.ts_oid_in(kind, s, &name))
            == Some(oid)
    }

    /// The name as messages show it: qualified unless visible.
    pub fn ts_qualified(&self, kind: TsKind, oid: u32, search_path: &[String]) -> Option<String> {
        let (ns, name) = self.ts_ident(kind, oid)?;
        Some(if self.ts_visible(kind, oid, search_path) {
            quote_ident(&name)
        } else {
            format!(
                "{}.{}",
                quote_ident(self.schema_name(ns)),
                quote_ident(&name)
            )
        })
    }

    /// The object of a kind named `name` in a namespace.
    pub fn ts_oid_in(&self, kind: TsKind, namespace: u32, name: &str) -> Option<u32> {
        self.ts_objects(kind)
            .into_iter()
            .find(|o| o.1 == namespace && o.2 == name)
            .map(|o| o.0)
    }

    /// (namespace, name) of an object.
    pub fn ts_ident(&self, kind: TsKind, oid: u32) -> Option<(u32, String)> {
        self.ts_objects(kind)
            .into_iter()
            .find(|o| o.0 == oid)
            .map(|o| (o.1, o.2))
    }

    /// `get_ts_*_oid` of an already split name: None when missing and `missing_ok`.
    pub fn ts_lookup(
        &self,
        kind: TsKind,
        parts: &[String],
        search_path: &[String],
        missing_ok: bool,
    ) -> Result<Option<u32>> {
        let (schema, name) = match parts {
            [n] => (None, n),
            [s, n] => (Some(s), n),
            [_, s, n] => (Some(s), n),
            _ => bail!(
                SYNTAX_ERROR,
                "improper qualified name (too many dotted names): {}",
                parts.join(".")
            ),
        };
        let found = match schema {
            Some(s) => {
                let Some(sc) = self.schema_by_name(s) else {
                    if missing_ok {
                        return Ok(None);
                    }
                    bail!(UNDEFINED_SCHEMA, "schema \"{s}\" does not exist");
                };
                self.ts_oid_in(kind, sc.oid, name)
            }
            None => self
                .ts_path(search_path)
                .into_iter()
                .find_map(|ns| self.ts_oid_in(kind, ns, name)),
        };
        match found {
            Some(o) => Ok(Some(o)),
            None if missing_ok => Ok(None),
            None => Err(err!(
                UNDEFINED_OBJECT,
                "{} \"{}\" does not exist",
                kind.noun(),
                parts.join(".")
            )),
        }
    }

    /// The schemas an unqualified name is looked up in: the path without the temporary schema.
    fn ts_path(&self, search_path: &[String]) -> Vec<u32> {
        self.effective_path(search_path)
            .into_iter()
            .filter(|s| !self.schema_name(*s).starts_with("pg_temp"))
            .collect()
    }

    /// The oid of a name as text (`'english'::regconfig`): a missing schema is a missing
    /// object.
    pub fn ts_lookup_text(&self, kind: TsKind, name: &str, search_path: &[String]) -> Result<u32> {
        let parts = parse_qualified(name)?;
        self.ts_lookup(kind, &parts, search_path, true)?
            .ok_or_else(|| {
                err!(
                    UNDEFINED_OBJECT,
                    "{} \"{}\" does not exist",
                    kind.noun(),
                    parts.join(".")
                )
            })
    }

    /// The name as regconfigout / regdictionaryout show it: qualified unless it is the one
    /// the path finds.
    pub fn ts_display(&self, kind: TsKind, oid: u32, search_path: &[String]) -> Option<String> {
        self.ts_qualified(kind, oid, search_path)
    }
}

impl Catalog {
    /// `check_default_text_search_config`: the configuration's qualified name, or None when
    /// there is no such configuration.
    pub fn ts_config_setting(&self, value: &str, search_path: &[String]) -> Option<String> {
        let parts = parse_qualified(value).ok()?;
        let oid = self
            .ts_lookup(TsKind::Config, &parts, search_path, true)
            .ok()??;
        let (ns, name) = self.ts_ident(TsKind::Config, oid)?;
        Some(format!(
            "{}.{}",
            quote_ident(self.schema_name(ns)),
            quote_ident(&name)
        ))
    }

    /// `getTSCurrentConfig`: default_text_search_config's configuration.
    pub fn ts_current_config(&self, setting: &str, search_path: &[String]) -> Result<u32> {
        if setting.is_empty() {
            return Err(crate::error::internal(
                "text search configuration isn't set",
            ));
        }
        let parts = parse_qualified(setting)?;
        Ok(self
            .ts_lookup(TsKind::Config, &parts, search_path, false)?
            .unwrap_or(0))
    }
}

/// A `default_text_search_config` value as SET stores it: qualified, after checking the
/// configuration exists.
pub fn check_ts_config_setting(
    cat: &Catalog,
    value: &str,
    search_path: &[String],
) -> Result<String> {
    cat.ts_config_setting(value, search_path).ok_or_else(|| {
        err!(
            INVALID_PARAMETER_VALUE,
            "invalid value for parameter \"default_text_search_config\": \"{value}\""
        )
    })
}

/// The catalog as the text search code sees it.
pub struct TsCatalog<'a> {
    pub catalog: &'a Catalog,
    pub search_path: &'a [String],
}

impl TsLookup for TsCatalog<'_> {
    fn config(&self, oid: u32) -> Option<TsConfigDef> {
        self.catalog.ts_config(oid)
    }

    fn dict(&self, oid: u32) -> Option<TsDictDef> {
        self.catalog.ts_dict(oid)
    }

    fn dict_oid(&self, name: &str) -> Result<u32> {
        let parts = parse_qualified(name)?;
        Ok(self
            .catalog
            .ts_lookup(TsKind::Dict, &parts, self.search_path, false)?
            .unwrap_or(0))
    }

    fn parser(&self, oid: u32) -> Option<TsParserDef> {
        self.catalog.ts_parser(oid)
    }

    fn template(&self, oid: u32) -> Option<TsTemplateDef> {
        self.catalog.ts_template(oid)
    }
}
