//! The names a preview's branches also go by (docs/previews.md, "Secrets"): the variables roll
//! secrets' providers suggest for a link to the parent (`PGPASSWORD`, `KAFKA_BROKERS`,
//! `ELASTICSEARCH_API_KEY`, …), set from the branch. A preview's secrets environment inherits
//! its base, and a link there to a parent the preview branched would still hand out the parent's
//! values under those names: set after the secrets layer, these win. A credential the branch has
//! no value for is set blank, never left the parent's.

use crate::Kind;

/// `vars` (a branch's own variables, unprefixed) plus the provider names they also stand for,
/// each name once, the branch's own first.
pub fn with_standard(kind: Kind, vars: Vec<(String, String)>) -> Vec<(String, String)> {
    let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let mut extra: Vec<(&str, String)> = Vec::new();
    match kind {
        Kind::Postgres => {
            if let Some(u) = get("DATABASE_URL").as_deref().and_then(Url::parse) {
                extra.extend([("PGHOST", u.host.clone()), ("PGPORT", u.port.clone().unwrap_or_else(|| "5432".into())), ("PGUSER", u.user.clone()), ("PGPASSWORD", u.password.clone()), ("PGDATABASE", u.path.clone())]);
            }
        }
        Kind::Redis => {
            if let Some(u) = get("REDIS_URL").as_deref().and_then(Url::parse) {
                extra.extend([("REDIS_HOST", u.host.clone()), ("REDIS_PORT", u.port.clone().unwrap_or_else(|| "6379".into())), ("REDIS_PASSWORD", u.password.clone())]);
            }
            extra.push(("UPSTASH_REDIS_REST_URL", get("REDIS_REST_URL").unwrap_or_default()));
            extra.push(("UPSTASH_REDIS_REST_TOKEN", get("REDIS_REST_TOKEN").unwrap_or_default()));
        }
        Kind::Stream => {
            extra.push(("KAFKA_BROKERS", get("KAFKA_BOOTSTRAP_SERVERS").unwrap_or_default()));
            extra.push(("KAFKA_USERNAME", get("KAFKA_SASL_USERNAME").unwrap_or_default()));
            extra.push(("KAFKA_PASSWORD", get("KAFKA_SASL_PASSWORD").unwrap_or_default()));
            // The parent's client properties carry its login.
            extra.push(("KAFKA_PROPERTIES", String::new()));
        }
        Kind::Bucket => {
            extra.push(("AWS_ENDPOINT_URL", get("AWS_ENDPOINT_URL_S3").unwrap_or_default()));
        }
        Kind::Nosql => {
            // The parent's HTTP token and DynamoDB endpoint: the branch has neither.
            extra.push(("NOSQL_TOKEN", String::new()));
            extra.push(("DYNAMODB_ENDPOINT", String::new()));
        }
        Kind::Search => {
            if let Some(raw) = get("ELASTICSEARCH_URL")
                && let Some(u) = Url::parse(&raw)
            {
                let port = u.port.clone().unwrap_or_else(|| if u.scheme == "https" { "443".into() } else { "9200".into() });
                let key = (!u.user.is_empty() && !u.user.contains('@')).then(|| {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", u.user, u.password))
                });
                extra.extend([
                    ("ELASTICSEARCH_HOST", u.host.clone()),
                    ("ELASTICSEARCH_PORT", port.clone()),
                    ("ELASTICSEARCH_USERNAME", u.user.clone()),
                    ("ELASTICSEARCH_API_KEY", key.unwrap_or_default()),
                    ("OPENSEARCH_URL", raw.clone()),
                    ("OPENSEARCH_HOST", u.host.clone()),
                    ("OPENSEARCH_PORT", port),
                    ("OPENSEARCH_CLUSTER", u.user.clone()),
                    ("OPENSEARCH_USERNAME", u.user.clone()),
                    ("OPENSEARCH_PASSWORD", u.password.clone()),
                ]);
            }
        }
        _ => {}
    }
    let mut out = vars;
    for (k, v) in extra {
        if !out.iter().any(|(n, _)| n == k) {
            out.push((k.to_string(), v));
        }
    }
    out
}

/// The branch's value for a roll secrets link's `field` to the parent the preview branched (a
/// `followed` key: roll secrets resolved nothing of the parent's), from its own variables
/// (`branch_vars`, unprefixed). Blank when the branch has nothing for that field: never the
/// parent's.
pub fn follow(kind: Kind, field: &str, branch_vars: Vec<(String, String)>) -> String {
    let name = match (kind, field) {
        (Kind::Postgres, "url" | "role_url") => "DATABASE_URL",
        (Kind::Postgres, "host") => "PGHOST",
        (Kind::Postgres, "port") => "PGPORT",
        (Kind::Postgres, "user" | "role_user") => "PGUSER",
        (Kind::Postgres, "password" | "role_password") => "PGPASSWORD",
        (Kind::Postgres, "database") => "PGDATABASE",
        (Kind::Redis, "url") => "REDIS_URL",
        (Kind::Redis, "host") => "REDIS_HOST",
        (Kind::Redis, "port") => "REDIS_PORT",
        (Kind::Redis, "password") => "REDIS_PASSWORD",
        (Kind::Redis, "rest_url") => "UPSTASH_REDIS_REST_URL",
        (Kind::Redis, "rest_token") => "UPSTASH_REDIS_REST_TOKEN",
        (Kind::Stream, "bootstrap") => "KAFKA_BROKERS",
        (Kind::Stream, "username") => "KAFKA_USERNAME",
        (Kind::Stream, "password") => "KAFKA_PASSWORD",
        (Kind::Bucket, "bucket") => "S3_BUCKET",
        (Kind::Bucket, "endpoint") => "AWS_ENDPOINT_URL",
        (Kind::Bucket, "region") => "AWS_REGION",
        (Kind::Bucket, "access_key_id") => "AWS_ACCESS_KEY_ID",
        (Kind::Bucket, "secret_access_key") => "AWS_SECRET_ACCESS_KEY",
        (Kind::Nosql, "cql") => "CASSANDRA_CONTACT_POINTS",
        (Kind::Nosql, "username") => "CASSANDRA_USERNAME",
        (Kind::Nosql, "password") => "CASSANDRA_PASSWORD",
        (Kind::Search, "url") => "ELASTICSEARCH_URL",
        (Kind::Search, "host") => "ELASTICSEARCH_HOST",
        (Kind::Search, "port") => "ELASTICSEARCH_PORT",
        (Kind::Search, "username") => "ELASTICSEARCH_USERNAME",
        (Kind::Search, "api_key") => "ELASTICSEARCH_API_KEY",
        (Kind::Search, "user") => "OPENSEARCH_USERNAME",
        (Kind::Search, "password") => "OPENSEARCH_PASSWORD",
        _ => return String::new(),
    };
    with_standard(kind, branch_vars).into_iter().find(|(n, _)| n == name).map(|(_, v)| v).unwrap_or_default()
}

/// Whether a search branch's kept link (its JSON variables) holds the branch's own login: one
/// made before a preview got only a data credential (a login in `ELASTICSEARCH_URL` beside an
/// API key, or one that isn't its data user's). The branch is made again, which changes it.
pub fn search_link_holds_branch_login(kept: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(kept) else { return false };
    let Some(u) = v["ELASTICSEARCH_URL"].as_str().and_then(Url::parse) else { return false };
    !u.user.is_empty() && (v.get("ELASTICSEARCH_API_KEY").is_some() || v["OPENSEARCH_USERNAME"].as_str() != Some(u.user.as_str()))
}

/// The parts of a `scheme://user:password@host:port/path?query` URL, percent-decoded.
struct Url {
    scheme: String,
    user: String,
    password: String,
    host: String,
    port: Option<String>,
    path: String,
}

impl Url {
    fn parse(s: &str) -> Option<Url> {
        let (scheme, rest) = s.split_once("://")?;
        let rest = rest.split(['?', '#']).next().unwrap_or("");
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (userinfo, hostport) = authority.rsplit_once('@').unwrap_or(("", authority));
        let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let (host, port) = match hostport.strip_prefix('[') {
            Some(v6) => {
                let (h, after) = v6.split_once(']')?;
                (h.to_string(), after.strip_prefix(':').map(str::to_string))
            }
            None => match hostport.rsplit_once(':') {
                Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h.to_string(), Some(p.to_string())),
                _ => (hostport.to_string(), None),
            },
        };
        Some(Url { scheme: scheme.to_ascii_lowercase(), user: decode(user), password: decode(password), host, port, path: decode(path) })
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::with_standard;
    use crate::Kind;

    fn v(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn get(vars: &[(String, String)], k: &str) -> Option<String> {
        vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    /// The names roll secrets' links to the parent would hand out are the branch's, or blank.
    #[test]
    fn a_branchs_values_go_by_the_names_a_link_to_its_parent_would_use() {
        let pg = with_standard(Kind::Postgres, v(&[("DATABASE_URL", "postgres://app_pv:s%40cret@db.test:6543/main?sslmode=require")]));
        assert_eq!(["PGHOST", "PGPORT", "PGUSER", "PGPASSWORD", "PGDATABASE"].map(|k| get(&pg, k).unwrap()), ["db.test", "6543", "app_pv", "s@cret", "main"]);
        let stream = with_standard(Kind::Stream, v(&[("KAFKA_BOOTSTRAP_SERVERS", "b:9092"), ("KAFKA_SASL_USERNAME", "pv"), ("KAFKA_SASL_PASSWORD", "p")]));
        assert_eq!(["KAFKA_BROKERS", "KAFKA_USERNAME", "KAFKA_PASSWORD", "KAFKA_PROPERTIES"].map(|k| get(&stream, k).unwrap()), ["b:9092", "pv", "p", ""], "the parent's properties never");
        let search = with_standard(Kind::Search, v(&[("ELASTICSEARCH_URL", "https://br-1:p@search.test")]));
        assert_eq!(get(&search, "ELASTICSEARCH_API_KEY").as_deref(), Some("YnItMTpw"), "base64(br-1:p)");
        assert_eq!(get(&search, "OPENSEARCH_PASSWORD").as_deref(), Some("p"));
        // The branch's own value under a name stays (a data key, say).
        let keyed = with_standard(Kind::Search, v(&[("ELASTICSEARCH_URL", "https://br-1:p@search.test"), ("ELASTICSEARCH_API_KEY", "data")]));
        assert_eq!(get(&keyed, "ELASTICSEARCH_API_KEY").as_deref(), Some("data"));
        // A followed key (roll secrets resolved nothing of the parent's) by its field, renamed or not.
        let branch = v(&[("DATABASE_URL", "postgres://app_pv:s%40cret@db.test:6543/main")]);
        assert_eq!(super::follow(Kind::Postgres, "role_password", branch.clone()), "s@cret");
        assert_eq!(super::follow(Kind::Postgres, "url", branch.clone()), "postgres://app_pv:s%40cret@db.test:6543/main");
        assert_eq!(super::follow(Kind::Nosql, "http_token", v(&[("CASSANDRA_USERNAME", "pv")])), "", "nothing for it: blank");
        let nosql = with_standard(Kind::Nosql, v(&[("CASSANDRA_USERNAME", "pv")]));
        assert_eq!(get(&nosql, "NOSQL_TOKEN").as_deref(), Some(""), "blank: never the parent's token");
    }
}
