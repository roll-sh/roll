//! libpq's connection options (fe-connect.c `PQconninfoOptions`) and connection strings
//! (`PQconninfoParse`: `key=value` lists and `postgresql://` URIs), with libpq's messages.

/// A libpq connection option: keyword, environment variable, compiled-in default and
/// dispchar ("" plain, "*" secret, "D" debug).
pub struct OptDef {
    pub keyword: &'static str,
    pub env: Option<&'static str>,
    pub default: Option<&'static str>,
    pub dispchar: &'static str,
}

const fn o(keyword: &'static str, env: Option<&'static str>, default: Option<&'static str>, dispchar: &'static str) -> OptDef {
    OptDef { keyword, env, default, dispchar }
}

/// PQconninfoOptions of PostgreSQL 18.6's libpq, in its order (PQconndefaults).
pub const OPTIONS: &[OptDef] = &[
    o("service", Some("PGSERVICE"), None, ""),
    o("user", Some("PGUSER"), None, ""),
    o("password", Some("PGPASSWORD"), None, "*"),
    o("passfile", Some("PGPASSFILE"), None, ""),
    o("channel_binding", Some("PGCHANNELBINDING"), Some("prefer"), ""),
    o("connect_timeout", Some("PGCONNECT_TIMEOUT"), None, ""),
    o("dbname", Some("PGDATABASE"), None, ""),
    o("host", Some("PGHOST"), None, ""),
    o("hostaddr", Some("PGHOSTADDR"), None, ""),
    o("port", Some("PGPORT"), Some("5432"), ""),
    o("client_encoding", Some("PGCLIENTENCODING"), None, ""),
    o("options", Some("PGOPTIONS"), Some(""), ""),
    o("application_name", Some("PGAPPNAME"), None, ""),
    o("fallback_application_name", None, None, ""),
    o("keepalives", None, None, ""),
    o("keepalives_idle", None, None, ""),
    o("keepalives_interval", None, None, ""),
    o("keepalives_count", None, None, ""),
    o("tcp_user_timeout", None, None, ""),
    o("sslmode", Some("PGSSLMODE"), Some("prefer"), ""),
    o("sslnegotiation", Some("PGSSLNEGOTIATION"), Some("postgres"), ""),
    o("sslcompression", Some("PGSSLCOMPRESSION"), Some("0"), ""),
    o("sslcert", Some("PGSSLCERT"), None, ""),
    o("sslkey", Some("PGSSLKEY"), None, ""),
    o("sslcertmode", Some("PGSSLCERTMODE"), None, ""),
    o("sslpassword", None, None, "*"),
    o("sslrootcert", Some("PGSSLROOTCERT"), None, ""),
    o("sslcrl", Some("PGSSLCRL"), None, ""),
    o("sslcrldir", Some("PGSSLCRLDIR"), None, ""),
    o("sslsni", Some("PGSSLSNI"), Some("1"), ""),
    o("requirepeer", Some("PGREQUIREPEER"), None, ""),
    o("require_auth", Some("PGREQUIREAUTH"), None, ""),
    o("min_protocol_version", Some("PGMINPROTOCOLVERSION"), None, ""),
    o("max_protocol_version", Some("PGMAXPROTOCOLVERSION"), None, ""),
    o("ssl_min_protocol_version", Some("PGSSLMINPROTOCOLVERSION"), Some("TLSv1.2"), ""),
    o("ssl_max_protocol_version", Some("PGSSLMAXPROTOCOLVERSION"), None, ""),
    o("gssencmode", Some("PGGSSENCMODE"), Some("prefer"), ""),
    o("krbsrvname", Some("PGKRBSRVNAME"), Some("postgres"), ""),
    o("gsslib", Some("PGGSSLIB"), None, ""),
    o("gssdelegation", Some("PGGSSDELEGATION"), Some("0"), ""),
    o("replication", None, None, "D"),
    o("target_session_attrs", Some("PGTARGETSESSIONATTRS"), Some("any"), ""),
    o("load_balance_hosts", Some("PGLOADBALANCEHOSTS"), Some("disable"), ""),
    o("scram_client_key", None, None, "D"),
    o("scram_server_key", None, None, "D"),
    o("oauth_issuer", None, None, ""),
    o("oauth_client_id", None, None, ""),
    o("oauth_client_secret", None, None, "*"),
    o("oauth_scope", None, None, ""),
    o("sslkeylogfile", None, None, "D"),
];

pub fn option(keyword: &str) -> Option<&'static OptDef> {
    OPTIONS.iter().find(|o| o.keyword == keyword)
}

/// A parsed connection string: the options given, in order (a later value of a keyword
/// replaces an earlier one, as conninfo_storeval does).
#[derive(Clone, Debug, Default)]
pub struct ConnInfo {
    pub values: Vec<(String, String)>,
}

impl ConnInfo {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.values.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    /// The value of `k`, unless empty (libpq treats an empty option as unset).
    pub fn nonempty(&self, k: &str) -> Option<&str> {
        self.get(k).filter(|v| !v.is_empty())
    }

    pub fn set(&mut self, k: &str, v: &str) {
        match self.values.iter_mut().find(|(n, _)| n == k) {
            Some(e) => e.1 = v.to_string(),
            None => self.values.push((k.to_string(), v.to_string())),
        }
    }

    /// conninfo_storeval: `requiressl=1` is `sslmode=require`; unknown keywords fail.
    fn store(&mut self, k: &str, v: &str) -> Result<(), String> {
        if k == "requiressl" {
            if v.starts_with('1') {
                self.set("sslmode", "require");
            }
            return Ok(());
        }
        if option(k).is_none() {
            return Err(format!("invalid connection option \"{k}\""));
        }
        self.set(k, v);
        Ok(())
    }
}

/// PQconninfoParse: a URI (`postgresql://` / `postgres://`) or `keyword=value` pairs.
/// The error is libpq's message (without its trailing newline).
pub fn parse(s: &str) -> Result<ConnInfo, String> {
    if let Some(rest) = s.strip_prefix("postgresql://").or_else(|| s.strip_prefix("postgres://")) {
        return parse_uri(s, rest);
    }
    parse_pairs(s)
}

/// Whether `s` is a URI rather than `keyword=value` pairs (uri_prefix_length).
pub fn is_uri(s: &str) -> bool {
    s.starts_with("postgresql://") || s.starts_with("postgres://")
}

/// Whether `s` looks like a connection string (recognized_connection_string): a URI or
/// something with an `=`.
pub fn recognized(s: &str) -> bool {
    is_uri(s) || s.contains('=')
}

fn parse_pairs(s: &str) -> Result<ConnInfo, String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = ConnInfo::default();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let mut name = String::new();
        while i < b.len() && b[i] != '=' {
            if b[i].is_ascii_whitespace() {
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                break;
            }
            name.push(b[i]);
            i += 1;
        }
        if i >= b.len() || b[i] != '=' {
            return Err(format!("missing \"=\" after \"{name}\" in connection info string"));
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut val = String::new();
        if i < b.len() && b[i] == '\'' {
            i += 1;
            loop {
                if i >= b.len() {
                    return Err("unterminated quoted string in connection info string".into());
                }
                if b[i] == '\\' {
                    i += 1;
                    if i < b.len() {
                        val.push(b[i]);
                        i += 1;
                    }
                    continue;
                }
                if b[i] == '\'' {
                    i += 1;
                    break;
                }
                val.push(b[i]);
                i += 1;
            }
        } else {
            while i < b.len() {
                if b[i].is_ascii_whitespace() {
                    i += 1;
                    break;
                }
                if b[i] == '\\' {
                    i += 1;
                    if i < b.len() {
                        val.push(b[i]);
                        i += 1;
                    }
                } else {
                    val.push(b[i]);
                    i += 1;
                }
            }
        }
        out.store(&name, &val)?;
    }
    Ok(out)
}

/// conninfo_uri_decode: %XX escapes.
fn uri_decode(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = |c: u8| (c as char).to_digit(16);
            match (b.get(i + 1).and_then(|c| hex(*c)), b.get(i + 2).and_then(|c| hex(*c))) {
                (Some(h), Some(l)) => {
                    let v = (h * 16 + l) as u8;
                    if v == 0 {
                        return Err(format!("forbidden value %00 in percent-encoded value: \"{s}\""));
                    }
                    out.push(v);
                    i += 3;
                }
                _ => return Err(format!("invalid percent-encoded token: \"{s}\"")),
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// conninfo_uri_parse_options: `[user[:password]@][host][:port][,...][/dbname][?params]`.
fn parse_uri(uri: &str, rest: &str) -> Result<ConnInfo, String> {
    let mut out = ConnInfo::default();
    let (main, query) = match rest.split_once('?') {
        Some((m, q)) => (m, Some(q)),
        None => (rest, None),
    };
    let (authority, path) = match main.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (main, None),
    };
    let hostpart = match authority.rsplit_once('@') {
        Some((cred, h)) => {
            let (user, pw) = match cred.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (cred, None),
            };
            if !user.is_empty() {
                out.store("user", &uri_decode(user)?)?;
            }
            if let Some(p) = pw.filter(|p| !p.is_empty()) {
                out.store("password", &uri_decode(p)?)?;
            }
            h
        }
        None => authority,
    };
    let (mut hosts, mut ports) = (Vec::new(), Vec::new());
    for spec in hostpart.split(',') {
        let (h, p) = if let Some(r) = spec.strip_prefix('[') {
            let Some(end) = r.find(']') else {
                return Err(format!(
                    "end of string reached when looking for matching \"]\" in IPv6 host address in URI: \"{uri}\""
                ));
            };
            if end == 0 {
                return Err(format!("IPv6 host address may not be empty in URI: \"{uri}\""));
            }
            let after = &r[end + 1..];
            let port = match after.strip_prefix(':') {
                Some(p) => p,
                None if after.is_empty() => "",
                None => {
                    return Err(format!(
                        "unexpected character \"{}\" at position {} in URI (expected \":\" or \"/\"): \"{uri}\"",
                        after.chars().next().unwrap_or(' '),
                        uri.len() - rest.len() + spec.len() - after.len() + 1
                    ));
                }
            };
            (r[..end].to_string(), port.to_string())
        } else {
            match spec.split_once(':') {
                Some((h, p)) => (h.to_string(), p.to_string()),
                None => (spec.to_string(), String::new()),
            }
        };
        hosts.push(uri_decode(&h)?);
        ports.push(uri_decode(&p)?);
    }
    if hosts.iter().any(|h| !h.is_empty()) {
        out.store("host", &hosts.join(","))?;
    }
    if ports.iter().any(|p| !p.is_empty()) {
        out.store("port", &ports.join(","))?;
    }
    if let Some(db) = path.filter(|p| !p.is_empty()) {
        out.store("dbname", &uri_decode(db)?)?;
    }
    if let Some(q) = query {
        for kv in q.split('&').filter(|s| !s.is_empty()) {
            let Some((k, v)) = kv.split_once('=') else {
                return Err(format!("missing key/value separator \"=\" in URI query parameter: \"{kv}\""));
            };
            if v.contains('=') {
                return Err(format!("extra key/value separator \"=\" in URI query parameter: \"{kv}\""));
            }
            let (k, v) = (uri_decode(k)?, uri_decode(v)?);
            let k = match k.as_str() {
                "ssl" if v == "true" => {
                    out.set("sslmode", "require");
                    continue;
                }
                _ => k,
            };
            if option(&k).is_none() && k != "requiressl" {
                return Err(format!("invalid URI query parameter: \"{k}\""));
            }
            out.store(&k, &v)?;
        }
    }
    Ok(out)
}

/// The value of option `k` a connection uses: given, else libpq's compiled-in default. (The
/// engine's environment is never consulted: `PGPASSWORD`, `PGSERVICEFILE` and the like of
/// the server process would hand its credentials to every role.)
pub fn effective(info: &ConnInfo, k: &str) -> Option<String> {
    if let Some(v) = info.nonempty(k) {
        return Some(v.to_string());
    }
    option(k)?.default.map(str::to_string)
}

/// PQconnectionNeedsPassword-style check of a parsed string: whether it gives a password.
pub fn has_password(info: &ConnInfo) -> bool {
    info.nonempty("password").is_some()
}
