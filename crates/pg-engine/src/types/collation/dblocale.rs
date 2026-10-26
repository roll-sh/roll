//! A database's locale: pg_database's encoding, datlocprovider, datcollate, datctype,
//! datlocale, daticurules and datcollversion, settled as initdb and CREATE DATABASE
//! (dbcommands.c `createdb`) settle them on the reference server, whose C library is musl
//! and whose ICU is 78.1.
//!
//! musl accepts every locale name (`setlocale` makes a locale of any name it has no data
//! for, behaving as C.UTF-8 for LC_CTYPE), compares strings by bytes whatever the locale
//! (`strcoll` is `strcmp`), and reports no collation version; see libc.rs for its ctype.

use super::{Collation, CollationDef, DEFAULT_OID, builtin_canonical, icu};
use crate::error::{Error, Result, code};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const SQL_ASCII: i32 = 0;
pub const UTF8: i32 = 6;

/// A database's locale settings (its pg_database row's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbLocale {
    pub encoding: i32,
    /// 'c' libc, 'i' ICU, 'b' builtin.
    pub provider: char,
    pub collate: String,
    pub ctype: String,
    /// datlocale: the ICU or builtin locale (None for libc).
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub icu_rules: Option<String>,
    #[serde(default)]
    pub collversion: Option<String>,
}

/// What initdb makes with `--locale=C --encoding=UTF8` (the reference's settings, and a
/// database's from before its locale was kept).
impl Default for DbLocale {
    fn default() -> Self {
        DbLocale {
            encoding: UTF8,
            provider: 'c',
            collate: "C".into(),
            ctype: "C".into(),
            locale: None,
            icu_rules: None,
            collversion: None,
        }
    }
}

impl DbLocale {
    /// The locale string the provider's version comes from (libc: datcollate).
    pub fn version_locale(&self) -> &str {
        match self.provider {
            'c' => &self.collate,
            _ => self.locale.as_deref().unwrap_or(""),
        }
    }

    /// `get_collation_actual_version` of the database's locale.
    pub fn actual_version(&self) -> Result<Option<String>> {
        super::actual_version(self.provider, self.version_locale())
    }

    /// The database's default collation as a definition (`init_database_collation`): what
    /// "default" (oid 100) behaves as. pg_collation's row stays the 'd' one.
    pub fn def(&self) -> CollationDef {
        CollationDef {
            oid: DEFAULT_OID,
            name: "default".into(),
            namespace: crate::catalog::PG_CATALOG_OID,
            owner: crate::catalog::BOOTSTRAP_SUPERUSER_OID,
            provider: self.provider,
            deterministic: true,
            encoding: -1,
            collate: Some(self.collate.clone()),
            ctype: Some(self.ctype.clone()),
            locale: self.locale.clone(),
            icu_rules: self.icu_rules.clone(),
            version: self.collversion.clone(),
        }
    }

    /// Whether the database's ctype is C (`database_ctype_is_c`: text search's character
    /// classes go by it, whatever the provider).
    pub fn ctype_is_c(&self) -> bool {
        matches!(self.ctype.as_str(), "C" | "POSIX")
    }

    /// The locale provider's name (`collprovider_name`).
    pub fn provider_name(&self) -> &'static str {
        provider_name(self.provider)
    }

    /// CheckMyDatabase's WARNING when the recorded collation version isn't the provider's
    /// (database `db`).
    pub fn version_warning(&self, db: &str) -> Option<Error> {
        let have = self.collversion.as_ref()?;
        let mut w = match self.actual_version().ok()? {
            None => Error::new(
                code::WARNING,
                format!("database \"{db}\" has no actual collation version, but a version was recorded"),
            ),
            Some(act) if &act != have => Error::new(
                code::WARNING,
                format!("database \"{db}\" has a collation version mismatch"),
            )
            .detail(format!(
                "The database was created using collation version {have}, but the operating system provides version {act}."
            ))
            .hint(format!(
                "Rebuild all objects in this database that use the default collation and run ALTER DATABASE {} REFRESH COLLATION VERSION, or build PostgreSQL with the right library version.",
                crate::catalog::quote_ident(db)
            )),
            Some(_) => return None,
        };
        w.severity = crate::error::Severity::Warning;
        Some(w)
    }
}

/// A database's locale with its default collation opened: what the database's catalogs
/// carry (`Catalog::db_locale`).
#[derive(Debug)]
pub struct DbDefault {
    pub locale: DbLocale,
    pub collation: Arc<Collation>,
}

impl DbDefault {
    /// `init_database_collation`.
    pub fn open(locale: DbLocale) -> Result<DbDefault> {
        let collation = Arc::new(Collation::open(&locale.def())?);
        Ok(DbDefault { locale, collation })
    }
}

pub fn provider_name(p: char) -> &'static str {
    match p {
        'b' => "builtin",
        'i' => "icu",
        _ => "libc",
    }
}

/// A locale category, for `setlocale`'s name of a locale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Collate,
    Ctype,
    Messages,
    Monetary,
    Numeric,
    Time,
}

impl Category {
    fn env(self) -> &'static str {
        match self {
            Category::Collate => "LC_COLLATE",
            Category::Ctype => "LC_CTYPE",
            Category::Messages => "LC_MESSAGES",
            Category::Monetary => "LC_MONETARY",
            Category::Numeric => "LC_NUMERIC",
            Category::Time => "LC_TIME",
        }
    }
}

/// musl's `LOCALE_NAME_MAX`.
const LOCALE_NAME_MAX: usize = 23;

/// The name musl's `setlocale(category, name)` gives a locale (locale_map.c `__get_locale`;
/// it accepts every name): a name with a `/`, starting with `.` or longer than
/// LOCALE_NAME_MAX is C.UTF-8; C, POSIX and C.UTF-8 are built in, and only LC_CTYPE has a
/// C.UTF-8 apart from C; any other name is a locale of that name. An empty name is the
/// environment's (`env`: LC_ALL, then the category's variable, then LANG).
pub fn musl_name(cat: Category, name: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    let from_env;
    let mut val = name;
    if val.is_empty() {
        from_env = ["LC_ALL", cat.env(), "LANG"]
            .iter()
            .find_map(|v| env(v).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "C".into());
        val = &from_env;
    }
    let n = val
        .bytes()
        .take(LOCALE_NAME_MAX)
        .take_while(|&b| b != b'/')
        .count();
    if val.starts_with('.') || n < val.len() {
        val = "C.UTF-8";
    }
    match val {
        "C" | "POSIX" => "C".into(),
        "C.UTF-8" if cat == Category::Ctype => "C.UTF-8".into(),
        "C.UTF-8" => "C".into(),
        v => v.to_string(),
    }
}

/// `pg_get_encoding_from_locale` under musl: C and POSIX work with any encoding (SQL_ASCII),
/// every other locale is UTF-8 (its `nl_langinfo(CODESET)`).
pub fn locale_encoding(name: &str) -> i32 {
    if matches!(name, "C" | "POSIX") {
        SQL_ASCII
    } else {
        UTF8
    }
}

/// `pg_valid_server_encoding_id`.
pub fn valid_server_encoding(id: i32) -> bool {
    (0..=34).contains(&id)
}

/// `is_encoding_supported_by_icu` (`pg_enc2icu_tbl`'s entries).
pub fn icu_supports_encoding(id: i32) -> bool {
    valid_server_encoding(id) && !matches!(id, 0 | 5 | 7 | 17 | 21)
}

pub fn encoding_name(id: i32) -> &'static str {
    crate::types::encnames::encoding_name(id as i64).unwrap_or("")
}

/// `builtin_validate_locale`: the canonical name of a builtin locale for `encoding`.
pub fn builtin_validate(encoding: i32, locale: &str) -> Result<String> {
    let Some(name) = builtin_canonical(locale) else {
        return Err(Error::new(
            code::WRONG_OBJECT_TYPE,
            format!("invalid locale name \"{locale}\" for builtin provider"),
        ));
    };
    if name != "C" && encoding != UTF8 {
        return Err(Error::new(
            code::WRONG_OBJECT_TYPE,
            format!(
                "encoding \"{}\" does not match locale \"{locale}\"",
                encoding_name(encoding)
            ),
        ));
    }
    Ok(name.to_string())
}

/// `check_encoding_locale_matches` (a superuser may pick SQL_ASCII for any locale).
pub fn check_encoding_matches(encoding: i32, collate: &str, ctype: &str, superuser: bool) -> Result<()> {
    for (name, what) in [(ctype, "LC_CTYPE"), (collate, "LC_COLLATE")] {
        let enc = locale_encoding(name);
        if !(enc == encoding || enc == SQL_ASCII || encoding == SQL_ASCII && superuser) {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "encoding \"{}\" does not match locale \"{name}\"",
                    encoding_name(encoding)
                ),
            )
            .detail(format!(
                "The chosen {what} setting requires encoding \"{}\".",
                encoding_name(enc)
            )));
        }
    }
    Ok(())
}

/// `icu_validation_level` as a message level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IcuLevel {
    /// disabled, or a level nobody sees.
    Quiet,
    Notice,
    Warning,
    Error,
}

impl IcuLevel {
    pub fn from_setting(v: Option<&str>) -> IcuLevel {
        match v {
            Some("error") => IcuLevel::Error,
            Some("warning") => IcuLevel::Warning,
            Some("notice") => IcuLevel::Notice,
            _ => IcuLevel::Quiet,
        }
    }
}

/// Where locale checks send their messages: ICU validation ones at `level` (an error at
/// ERROR), and NOTICEs and WARNINGs, through `report`.
pub struct Reporter<'a> {
    pub level: IcuLevel,
    pub report: &'a mut dyn FnMut(Error, IcuLevel),
}

impl Reporter<'_> {
    /// Report `e` at the validation level.
    pub fn validation(&mut self, e: Error) -> Result<()> {
        match self.level {
            IcuLevel::Error => Err(e),
            IcuLevel::Quiet => Ok(()),
            l => {
                (self.report)(e, l);
                Ok(())
            }
        }
    }

    pub fn notice(&mut self, m: String) {
        (self.report)(Error::new(code::SUCCESSFUL_COMPLETION, m), IcuLevel::Notice);
    }

    pub fn warning(&mut self, e: Error) {
        (self.report)(e, IcuLevel::Warning);
    }
}

/// `icu_language_tag(loc, icu_validation_level)`: the locale's BCP 47 tag, or None (and a
/// message at the validation level) when ICU can't make one.
pub fn icu_language_tag(loc: &str, r: &mut Reporter) -> Result<Option<String>> {
    match icu::language_tag(loc) {
        Ok(tag) => Ok(Some(tag)),
        Err(e) => {
            r.validation(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("could not convert locale name \"{loc}\" to language tag: {e}"),
            ))?;
            Ok(None)
        }
    }
}

const ICU_HINT: &str =
    "To disable ICU locale validation, set the parameter \"icu_validation_level\" to \"disabled\".";

/// `icu_validate_locale`.
pub fn icu_validate_locale(loc: &str, r: &mut Reporter) -> Result<()> {
    if r.level == IcuLevel::Quiet {
        return Ok(());
    }
    let lang = match icu::language(loc) {
        Ok(l) => l,
        Err(e) => {
            return r.validation(
                Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("could not get language from ICU locale \"{loc}\": {e}"),
                )
                .hint(ICU_HINT),
            );
        }
    };
    if !icu::known_language(&lang) {
        r.validation(
            Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("ICU locale \"{loc}\" has unknown language \"{lang}\""),
            )
            .hint(ICU_HINT),
        )?;
    }
    icu::open(loc, None)?;
    Ok(())
}

/// `check_locale`: the name musl gives the locale; PostgreSQL refuses non-ASCII names
/// (with a WARNING) before asking.
fn check_locale(
    cat: Category,
    name: &str,
    env: &dyn Fn(&str) -> Option<String>,
    r: &mut Reporter,
) -> Option<String> {
    if !name.is_ascii() {
        r.warning(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("locale name \"{name}\" contains non-ASCII characters"),
        ));
        return None;
    }
    Some(musl_name(cat, name, env))
}

/// CREATE DATABASE's (or initdb's) locale options, as given.
#[derive(Clone, Debug, Default)]
pub struct LocaleOptions {
    pub encoding: Option<i32>,
    pub locale: Option<String>,
    pub lc_collate: Option<String>,
    pub lc_ctype: Option<String>,
    pub builtin_locale: Option<String>,
    pub icu_locale: Option<String>,
    pub icu_rules: Option<String>,
    pub provider: Option<char>,
    pub collversion: Option<String>,
    /// BUILTIN_LOCALE / ICU_LOCALE / COLLATION_VERSION were written (even without a value).
    pub builtin_given: bool,
    pub icu_given: bool,
    pub collversion_given: bool,
}

/// `createdb`'s locale part: the new database's locale from the options and the template's
/// (`src`, named `template`). `env` is the session's locale environment (what an empty
/// locale name means: the current database's settings).
pub fn createdb(
    o: &LocaleOptions,
    template: &str,
    src: &DbLocale,
    env: &dyn Fn(&str) -> Option<String>,
    superuser: bool,
    r: &mut Reporter,
) -> Result<DbLocale> {
    let mut dblocale = o.icu_locale.clone().or_else(|| o.builtin_locale.clone()).or_else(|| o.locale.clone());
    let encoding = o.encoding.unwrap_or(src.encoding);
    let collate = o.lc_collate.clone().or_else(|| o.locale.clone()).unwrap_or_else(|| src.collate.clone());
    let ctype = o.lc_ctype.clone().or_else(|| o.locale.clone()).unwrap_or_else(|| src.ctype.clone());
    let provider = o.provider.unwrap_or(src.provider);
    // (The template's locale rather than one from the options: kept as it is.)
    let inherited = dblocale.is_none() && provider == src.provider;
    if inherited {
        dblocale = src.locale.clone();
    }
    let icu_rules = o.icu_rules.clone().or_else(|| src.icu_rules.clone());
    let invalid = |what: &str, name: &str| {
        let e = Error::new(code::WRONG_OBJECT_TYPE, format!("invalid {what} locale name: \"{name}\""));
        match provider {
            'b' => e.hint("If the locale name is specific to the builtin provider, use BUILTIN_LOCALE."),
            'i' => e.hint("If the locale name is specific to the ICU provider, use ICU_LOCALE."),
            _ => e,
        }
    };
    let collate = check_locale(Category::Collate, &collate, env, r)
        .ok_or_else(|| invalid("LC_COLLATE", &collate))?;
    let ctype =
        check_locale(Category::Ctype, &ctype, env, r).ok_or_else(|| invalid("LC_CTYPE", &ctype))?;
    check_encoding_matches(encoding, &collate, &ctype, superuser)?;
    let not_unless = |m: &str| Error::new(code::INVALID_OBJECT_DEFINITION, m.to_string());
    if provider != 'b' && o.builtin_given {
        return Err(not_unless("BUILTIN_LOCALE cannot be specified unless locale provider is builtin"));
    }
    if provider != 'i' {
        if o.icu_given {
            return Err(not_unless("ICU locale cannot be specified unless locale provider is ICU"));
        }
        if icu_rules.is_some() {
            return Err(not_unless("ICU rules cannot be specified unless locale provider is ICU"));
        }
    }
    let must =
        |p: &str| Error::new(code::INVALID_PARAMETER_VALUE, format!("LOCALE or {p} must be specified"));
    match provider {
        'b' => {
            let l = dblocale.ok_or_else(|| must("BUILTIN_LOCALE"))?;
            dblocale = Some(builtin_validate(encoding, &l)?);
        }
        'i' => {
            if !icu_supports_encoding(encoding) {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("encoding \"{}\" is not supported with ICU provider", encoding_name(encoding)),
                ));
            }
            let mut l = dblocale.ok_or_else(|| must("ICU_LOCALE"))?;
            if !inherited
                && let Some(tag) = icu_language_tag(&l, r)?
                && tag != l
            {
                r.notice(format!("using standard form \"{tag}\" for ICU locale \"{l}\""));
                l = tag;
            }
            icu_validate_locale(&l, r)?;
            dblocale = Some(l);
        }
        _ => dblocale = None,
    }
    let mut new = DbLocale { encoding, provider, collate, ctype, locale: dblocale, icu_rules, collversion: None };
    if template != "template0" {
        check_template(&new, src)?;
    }
    if let Some(have) = &src.collversion
        && !o.collversion_given
    {
        let act = new.actual_version()?.ok_or_else(|| {
            Error::new(
                code::INTERNAL_ERROR,
                format!(
                    "template database \"{template}\" has a collation version, but no actual collation version could be determined"
                ),
            )
        })?;
        if &act != have {
            return Err(Error::new(
                code::INTERNAL_ERROR,
                format!("template database \"{template}\" has a collation version mismatch"),
            )
            .detail(format!(
                "The template database was created using collation version {have}, but the operating system provides version {act}."
            ))
            .hint(format!(
                "Rebuild all objects in the template database that use the default collation and run ALTER DATABASE {} REFRESH COLLATION VERSION, or build PostgreSQL with the right library version.",
                crate::catalog::quote_ident(template)
            )));
        }
    }
    new.collversion = match o.collversion.clone().or_else(|| src.collversion.clone()) {
        Some(v) => Some(v),
        None => new.actual_version()?,
    };
    Ok(new)
}

/// A template other than template0 must have the new database's encoding and locale.
fn check_template(new: &DbLocale, src: &DbLocale) -> Result<()> {
    let bad = |m: String, hint: &str| {
        Err(Error::new(code::INVALID_PARAMETER_VALUE, m).hint(format!(
            "Use the same {hint} as in the template database, or use template0 as template."
        )))
    };
    if new.encoding != src.encoding {
        return bad(
            format!(
                "new encoding ({}) is incompatible with the encoding of the template database ({})",
                encoding_name(new.encoding),
                encoding_name(src.encoding)
            ),
            "encoding",
        );
    }
    if new.collate != src.collate {
        return bad(
            format!(
                "new collation ({}) is incompatible with the collation of the template database ({})",
                new.collate, src.collate
            ),
            "collation",
        );
    }
    if new.ctype != src.ctype {
        return bad(
            format!(
                "new LC_CTYPE ({}) is incompatible with the LC_CTYPE of the template database ({})",
                new.ctype, src.ctype
            ),
            "LC_CTYPE",
        );
    }
    if new.provider != src.provider {
        return bad(
            format!(
                "new locale provider ({}) does not match locale provider of the template database ({})",
                new.provider_name(),
                src.provider_name()
            ),
            "locale provider",
        );
    }
    if new.provider == 'i' {
        let (a, b) = (new.locale.as_deref().unwrap_or(""), src.locale.as_deref().unwrap_or(""));
        if a != b {
            return bad(
                format!("new ICU locale ({a}) is incompatible with the ICU locale of the template database ({b})"),
                "ICU locale",
            );
        }
        let (a, b) = (new.icu_rules.as_deref().unwrap_or(""), src.icu_rules.as_deref().unwrap_or(""));
        if a != b {
            return bad(
                format!(
                    "new ICU collation rules ({a}) are incompatible with the ICU collation rules of the template database ({b})"
                ),
                "ICU collation rules",
            );
        }
    }
    Ok(())
}

/// initdb's locale options (`--locale`, `--lc-*`, `--locale-provider`, `--icu-locale`,
/// `--builtin-locale`, `--icu-rules`, `--encoding`). The default is the reference's
/// `--locale=C --encoding=UTF8`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitdbLocale {
    pub locale: Option<String>,
    pub lc_collate: Option<String>,
    pub lc_ctype: Option<String>,
    pub lc_messages: Option<String>,
    pub lc_monetary: Option<String>,
    pub lc_numeric: Option<String>,
    pub lc_time: Option<String>,
    /// "libc" (the default), "icu" or "builtin".
    pub provider: Option<String>,
    pub icu_locale: Option<String>,
    pub builtin_locale: Option<String>,
    pub icu_rules: Option<String>,
    pub encoding: Option<String>,
}

impl Default for InitdbLocale {
    fn default() -> Self {
        InitdbLocale {
            locale: Some("C".into()),
            lc_collate: None,
            lc_ctype: None,
            lc_messages: None,
            lc_monetary: None,
            lc_numeric: None,
            lc_time: None,
            provider: None,
            icu_locale: None,
            builtin_locale: None,
            icu_rules: None,
            encoding: Some("UTF8".into()),
        }
    }
}

/// What initdb settles: the template databases' locale (template1's, with its collation
/// version) and postgresql.conf's lc_messages, lc_monetary, lc_numeric and lc_time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitdbResult {
    pub db: DbLocale,
    pub config: Vec<(String, String)>,
}

/// initdb's `setlocales` and `setup_locale_encoding`, with musl's locale names. `env` is the
/// process environment (what an unset or empty locale means).
pub fn initdb(o: &InitdbLocale, env: &dyn Fn(&str) -> Option<String>) -> Result<InitdbResult> {
    let fatal = |m: String| Error::new(code::INVALID_PARAMETER_VALUE, m);
    let provider = match o.provider.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("libc") => 'c',
        Some("icu") => 'i',
        Some("builtin") => 'b',
        Some(p) => return Err(fatal(format!("unrecognized locale provider: {p}"))),
    };
    for (given, opt, need) in [
        (o.builtin_locale.is_some(), "--builtin-locale", 'b'),
        (o.icu_locale.is_some(), "--icu-locale", 'i'),
        (o.icu_rules.is_some(), "--icu-rules", 'i'),
    ] {
        if given && provider != need {
            return Err(fatal(format!(
                "{opt} cannot be specified unless locale provider \"{}\" is chosen",
                provider_name(need)
            )));
        }
    }
    let pick = |v: &Option<String>| v.clone().or_else(|| o.locale.clone()).unwrap_or_default();
    let name = |cat, v: &Option<String>| musl_name(cat, &pick(v), env);
    let ctype = name(Category::Ctype, &o.lc_ctype);
    let collate = name(Category::Collate, &o.lc_collate);
    let config = vec![
        ("lc_messages".to_string(), name(Category::Messages, &o.lc_messages)),
        ("lc_monetary".to_string(), name(Category::Monetary, &o.lc_monetary)),
        ("lc_numeric".to_string(), name(Category::Numeric, &o.lc_numeric)),
        ("lc_time".to_string(), name(Category::Time, &o.lc_time)),
    ];
    let mut datlocale = o.icu_locale.clone().or_else(|| o.builtin_locale.clone());
    if datlocale.is_none() && provider != 'c' {
        datlocale = o.locale.clone();
    }
    let datlocale = match provider {
        'c' => None,
        _ => {
            let l = datlocale.ok_or_else(|| {
                fatal(format!("locale must be specified if provider is {}", provider_name(provider)))
            })?;
            Some(match provider {
                'b' => builtin_canonical(&l)
                    .ok_or_else(|| fatal(format!("invalid locale name \"{l}\" for builtin provider")))?
                    .to_string(),
                _ => icu::language_tag(&l).map_err(|e| {
                    fatal(format!("could not convert locale name \"{l}\" to language tag: {e}"))
                })?,
            })
        }
    };
    let encoding = match &o.encoding {
        Some(e) => crate::types::encnames::encoding_id(e)
            .filter(|&id| valid_server_encoding(id))
            .ok_or_else(|| fatal(format!("\"{e}\" is not a valid server encoding name")))?,
        None if locale_encoding(&ctype) == SQL_ASCII => SQL_ASCII,
        None => UTF8,
    };
    for l in [&ctype, &collate] {
        let enc = locale_encoding(l);
        if enc != SQL_ASCII && enc != encoding {
            return Err(fatal(format!(
                "encoding mismatch: the encoding you selected ({}) and the encoding that the selected locale uses ({}) do not match",
                encoding_name(encoding),
                encoding_name(enc)
            )));
        }
    }
    match (provider, datlocale.as_deref()) {
        ('b', Some(l)) => {
            builtin_validate(encoding, l)?;
        }
        ('i', Some(l)) => {
            if !icu_supports_encoding(encoding) {
                return Err(fatal(format!(
                    "encoding \"{}\" is not supported with ICU provider",
                    encoding_name(encoding)
                )));
            }
            icu::open(l, o.icu_rules.as_deref())?;
        }
        _ => {}
    }
    let mut db = DbLocale {
        encoding,
        provider,
        collate,
        ctype,
        locale: datlocale,
        icu_rules: o.icu_rules.clone(),
        collversion: None,
    };
    db.collversion = db.actual_version()?;
    Ok(InitdbResult { db, config })
}
