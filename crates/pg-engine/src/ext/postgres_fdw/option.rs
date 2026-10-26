//! postgres_fdw's options (option.c): which objects take which options, the validator, and
//! the connection options of a server and user mapping.

use crate::catalog::Catalog;
use crate::error::{Error, Result, code};
use crate::ext::native::FdwValidate;
use crate::remote::conninfo;

pub const FOREIGN_DATA_WRAPPER: u32 = 2328;
pub const FOREIGN_SERVER: u32 = 1417;
pub const USER_MAPPING: u32 = 1418;
pub const FOREIGN_TABLE: u32 = 3118;
pub const ATTRIBUTE: u32 = 1249;

/// A postgres_fdw option: keyword, the catalog of the objects taking it, whether libpq's.
struct Opt {
    keyword: &'static str,
    context: u32,
    libpq: bool,
}

/// postgres_fdw_options (InitPgFdwOptions): libpq's options (no debug ones,
/// fallback_application_name, client_encoding or OAuth ones; `user` and secrets on user
/// mappings, the rest on servers), then postgres_fdw's own.
fn options() -> &'static [Opt] {
    static O: std::sync::OnceLock<Vec<Opt>> = std::sync::OnceLock::new();
    O.get_or_init(|| {
        let mut out = Vec::new();
        for o in conninfo::OPTIONS {
            if o.dispchar.contains('D')
                || o.keyword == "fallback_application_name"
                || o.keyword == "client_encoding"
                || o.keyword.starts_with("oauth_")
            {
                continue;
            }
            let context = if o.keyword == "user" || o.dispchar.contains('*') { USER_MAPPING } else { FOREIGN_SERVER };
            out.push(Opt { keyword: o.keyword, context, libpq: true });
        }
        for (keyword, context, libpq) in [
            ("schema_name", FOREIGN_TABLE, false),
            ("table_name", FOREIGN_TABLE, false),
            ("column_name", ATTRIBUTE, false),
            ("use_remote_estimate", FOREIGN_SERVER, false),
            ("use_remote_estimate", FOREIGN_TABLE, false),
            ("fdw_startup_cost", FOREIGN_SERVER, false),
            ("fdw_tuple_cost", FOREIGN_SERVER, false),
            ("extensions", FOREIGN_SERVER, false),
            ("updatable", FOREIGN_SERVER, false),
            ("updatable", FOREIGN_TABLE, false),
            ("truncatable", FOREIGN_SERVER, false),
            ("truncatable", FOREIGN_TABLE, false),
            ("fetch_size", FOREIGN_SERVER, false),
            ("fetch_size", FOREIGN_TABLE, false),
            ("batch_size", FOREIGN_SERVER, false),
            ("batch_size", FOREIGN_TABLE, false),
            ("async_capable", FOREIGN_SERVER, false),
            ("async_capable", FOREIGN_TABLE, false),
            ("parallel_commit", FOREIGN_SERVER, false),
            ("parallel_abort", FOREIGN_SERVER, false),
            ("keep_connections", FOREIGN_SERVER, false),
            ("password_required", USER_MAPPING, false),
            ("analyze_sampling", FOREIGN_SERVER, false),
            ("analyze_sampling", FOREIGN_TABLE, false),
            ("use_scram_passthrough", FOREIGN_SERVER, false),
            ("use_scram_passthrough", USER_MAPPING, false),
            ("sslcert", USER_MAPPING, true),
            ("sslkey", USER_MAPPING, true),
            ("gssdelegation", USER_MAPPING, true),
        ] {
            out.push(Opt { keyword, context, libpq });
        }
        out
    })
}

fn is_valid(keyword: &str, context: u32) -> bool {
    options().iter().any(|o| o.context == context && o.keyword == keyword)
}

/// Whether `keyword` is a libpq option (one a connection string takes).
pub fn is_libpq(keyword: &str) -> bool {
    options().iter().any(|o| o.libpq && o.keyword == keyword)
}

pub fn split(o: &str) -> (&str, &str) {
    o.split_once('=').unwrap_or((o, ""))
}

/// The value of option `name` in a list of `name=value` options.
pub fn get<'a>(opts: Option<&'a Vec<String>>, name: &str) -> Option<&'a str> {
    opts?.iter().map(|o| split(o)).find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// parse_real (no unit).
fn parse_real(v: &str) -> Option<f64> {
    v.trim().parse::<f64>().ok().filter(|f| !f.is_nan())
}

/// postgres_fdw_validator.
pub fn validate(v: &FdwValidate) -> Result<()> {
    for o in v.options {
        let (k, val) = split(o);
        if !is_valid(k, v.catalog) {
            let valid: Vec<&str> = options().iter().filter(|o| o.context == v.catalog).map(|o| o.keyword).collect();
            let e = Error::new(code::FDW_INVALID_OPTION_NAME, format!("invalid option \"{k}\""));
            return Err(if valid.is_empty() {
                e.hint("There are no valid options in this context.")
            } else {
                match crate::session::ddl::closest_match(k, valid.iter().copied()) {
                    Some(m) => e.hint(format!("Perhaps you meant the option \"{m}\".")),
                    None => e,
                }
            });
        }
        match k {
            "use_remote_estimate" | "updatable" | "truncatable" | "async_capable" | "parallel_commit"
            | "parallel_abort" | "keep_connections" | "use_scram_passthrough" => {
                crate::ext::dblink::def_boolean(k, val)?;
            }
            "fdw_startup_cost" | "fdw_tuple_cost" => {
                let Some(f) = parse_real(val) else {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid value for floating point option \"{k}\": {val}"),
                    ));
                };
                if f < 0.0 {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("\"{k}\" must be a floating point value greater than or equal to zero"),
                    ));
                }
            }
            "extensions" => {
                extension_list(v.cat, val, Some(v.notice))?;
            }
            "fetch_size" | "batch_size" => {
                let Some(n) = parse_int(val) else {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid value for integer option \"{k}\": {val}"),
                    ));
                };
                if n <= 0 {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("\"{k}\" must be an integer value greater than zero"),
                    ));
                }
            }
            "password_required" => {
                if !crate::ext::dblink::def_boolean(k, val)? && !v.superuser {
                    return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "password_required=false is superuser-only")
                        .hint("User mappings with the password_required option set to false may only be created or modified by the superuser."));
                }
            }
            "sslcert" | "sslkey" if v.catalog == USER_MAPPING && !v.superuser => {
                return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "sslcert and sslkey are superuser-only")
                    .hint("User mappings with the sslcert or sslkey options set may only be created or modified by the superuser."));
            }
            "analyze_sampling" if !matches!(val, "off" | "auto" | "random" | "system" | "bernoulli") => {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("invalid value for string option \"{k}\": {val}"),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// parse_int (no unit): decimal, hex or octal, or a number with a fraction rounded.
fn parse_int(v: &str) -> Option<i64> {
    let t = v.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let n = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()
    } else if body.len() > 1 && body.starts_with('0') && body.bytes().all(|b| b.is_ascii_digit()) {
        i64::from_str_radix(&body[1..], 8).ok()
    } else if let Ok(n) = body.parse::<i64>() {
        Some(n)
    } else {
        body.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| f.round() as i64)
    }?;
    let n = if neg { -n } else { n };
    i32::try_from(n).ok().map(i64::from)
}

/// ExtractExtensionList: the oids of the extensions a comma-separated list names (missing
/// ones warned about when `warn` is given).
pub fn extension_list(cat: &Catalog, list: &str, warn: Option<&dyn Fn(Error)>) -> Result<Vec<u32>> {
    let Some(names) = crate::session::settings::split_identifiers(list) else {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "parameter \"extensions\" must be a list of extension names"));
    };
    let mut out = Vec::new();
    for n in names {
        match cat.extensions.iter().find(|e| e.1 == n) {
            Some(e) => out.push(e.0),
            None => {
                if let Some(w) = warn {
                    let mut e = Error::new(code::UNDEFINED_OBJECT, format!("extension \"{n}\" is not installed"));
                    e.severity = crate::error::Severity::Warning;
                    w(e);
                }
            }
        }
    }
    Ok(out)
}
