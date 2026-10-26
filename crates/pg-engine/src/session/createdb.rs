//! CREATE DATABASE's encoding and locale options (dbcommands.c `createdb`): read here,
//! settled against the template's by `collation::dblocale::createdb`.

use super::{Session, notice, warning};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{def_arg_str, inner, with_pos};
use crate::types::collation::dblocale::{self, DbLocale, IcuLevel, LocaleOptions, Reporter};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// createdb's options.
const OPTIONS: &[&str] = &[
    "tablespace",
    "owner",
    "template",
    "encoding",
    "locale",
    "builtin_locale",
    "lc_collate",
    "lc_ctype",
    "icu_locale",
    "icu_rules",
    "locale_provider",
    "is_template",
    "allow_connections",
    "connection_limit",
    "collation_version",
    "location",
    "oid",
    "strategy",
];

/// CREATE DATABASE's options as far as encoding, locale and strategy go.
#[derive(Default)]
pub(super) struct Options {
    pub locale: LocaleOptions,
    strategy: Option<String>,
    seen: Vec<String>,
}

impl Options {
    /// Read option `d`; a repeated or unknown one fails, as in createdb.
    pub(super) fn read(&mut self, d: &pb::DefElem) -> Result<()> {
        let name = d.defname.as_str();
        if !OPTIONS.contains(&name) {
            return Err(with_pos(
                Error::new(code::SYNTAX_ERROR, format!("option \"{name}\" not recognized")),
                d.location,
            ));
        }
        if name != "location" && self.seen.iter().any(|s| s == name) {
            return Err(with_pos(
                Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                d.location,
            ));
        }
        self.seen.push(name.to_string());
        let o = &mut self.locale;
        let val = def_arg_str(d);
        match name {
            "encoding" if d.arg.is_some() => o.encoding = Some(encoding(d)?),
            "locale" => o.locale = val,
            "builtin_locale" => {
                o.builtin_given = true;
                o.builtin_locale = val;
            }
            "lc_collate" => o.lc_collate = val,
            "lc_ctype" => o.lc_ctype = val,
            "icu_locale" => {
                o.icu_given = true;
                o.icu_locale = val;
            }
            "icu_rules" => o.icu_rules = val,
            "locale_provider" => {
                if let Some(p) = val {
                    o.provider = Some(match p.to_ascii_lowercase().as_str() {
                        "builtin" => 'b',
                        "icu" => 'i',
                        "libc" => 'c',
                        _ => {
                            return Err(Error::new(
                                code::INVALID_OBJECT_DEFINITION,
                                format!("unrecognized locale provider: {p}"),
                            ));
                        }
                    });
                }
            }
            "collation_version" => {
                o.collversion_given = true;
                o.collversion = val;
            }
            "strategy" => self.strategy = val,
            _ => {}
        }
        Ok(())
    }

    /// The creation strategy must be one createdb knows.
    pub(super) fn check_strategy(&self) -> Result<()> {
        match self.strategy.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("wal_log" | "file_copy") => Ok(()),
            Some(_) => Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "invalid create database strategy \"{}\"",
                    self.strategy.as_deref().unwrap_or_default()
                ),
            )
            .hint("Valid strategies are \"wal_log\" and \"file_copy\".")),
        }
    }
}

/// ENCODING's value: an encoding's code or name, which must be a server encoding.
fn encoding(d: &pb::DefElem) -> Result<i32> {
    let bad = |m: String| with_pos(Error::new(code::UNDEFINED_OBJECT, m), d.location);
    if let Some(NodeEnum::Integer(i)) = d.arg.as_deref().and_then(inner) {
        return match dblocale::valid_server_encoding(i.ival) {
            true => Ok(i.ival),
            false => Err(bad(format!("{} is not a valid encoding code", i.ival))),
        };
    }
    let name = def_arg_str(d).unwrap_or_default();
    crate::types::encnames::encoding_id(&name)
        .filter(|&id| dblocale::valid_server_encoding(id))
        .ok_or_else(|| bad(format!("{name} is not a valid encoding name")))
}

impl Session {
    /// The new database's encoding and locale: `o` settled against the template's
    /// (`template`, its locale `src`), with createdb's NOTICEs and WARNINGs.
    pub(super) fn createdb_locale(
        &mut self,
        o: &Options,
        template: &str,
        src: &DbLocale,
        superuser: bool,
    ) -> Result<DbLocale> {
        // (An empty locale name is the environment's: what this database set.)
        let cur = self.state.cluster.database_locale(&self.state.database).unwrap_or_default();
        let env = |v: &str| match v {
            "LC_COLLATE" => Some(cur.collate.clone()),
            "LC_CTYPE" => Some(cur.ctype.clone()),
            _ => None,
        };
        let level = IcuLevel::from_setting(
            self.state.settings.borrow().get_raw("icu_validation_level").as_deref(),
        );
        let mut msgs = Vec::new();
        let new = {
            let mut report = |e: Error, l: IcuLevel| msgs.push((e, l));
            let mut r = Reporter { level, report: &mut report };
            dblocale::createdb(&o.locale, template, src, &env, superuser, &mut r)
        };
        for (e, l) in msgs {
            let mut n = match l {
                IcuLevel::Warning => warning(e.code, e.message),
                _ => {
                    let mut n = notice(e.message);
                    n.code = e.code;
                    n
                }
            };
            n.detail = e.detail;
            n.hint = e.hint;
            self.raise_notice(n);
        }
        let new = new?;
        if new.encoding != dblocale::UTF8 {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("encoding \"{}\" is not supported", dblocale::encoding_name(new.encoding)),
            )
            .detail("Databases here are UTF8."));
        }
        Ok(new)
    }
}
