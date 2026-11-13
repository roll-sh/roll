//! The Hostnames table (docs/cloud/README.md): what a TLS server name or `Host` header names.
//!
//! Names customers connect to directly carry the region (`<id>.pg.us-east-1.roll.sh`,
//! `s3.us-east-1.roll.site`), and a node routes only its own region's. Global and
//! Cloudflare-proxied names don't (`roll.sh`, `api.roll.sh`, `git.roll.sh`, `chg-<id>.roll.site`).

/// The zones and region every hostname is built from (`ROLL_EDGE_DOMAIN`,
/// `ROLL_EDGE_SITE_DOMAIN`, `ROLL_EDGE_REGION`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hostnames {
    /// `roll.sh`: the product.
    pub domain: String,
    /// `roll.site`: user content only (S3, public buckets, previews).
    pub site_domain: String,
    /// `us-east-1`
    pub region: String,
}

/// What a hostname names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    /// `roll.sh`, `www.roll.sh`: the site and dashboard (and git under `.git` paths).
    Dashboard,
    /// `api.roll.sh`
    Api,
    /// `git.roll.sh`
    Git,
    /// `<id>.pg.<region>.roll.sh`
    Pg(String),
    /// `<id>.redis.<region>.roll.sh`
    Redis(String),
    /// `<id>.kafka.<region>.roll.sh`
    Kafka(String),
    /// `<id>.nosql.<region>.roll.sh`
    Nosql(String),
    /// `<id>.mq.<region>.roll.sh`
    Mq(String),
    /// `<cluster>.search.<region>.roll.sh`: a search cluster's Elasticsearch REST API.
    Search(String),
    /// `tasks.<region>.roll.sh`
    Tasks,
    /// `origin.<region>.roll.sh`: the region's NLB itself.
    Origin,
    /// `s3.<region>.roll.site` (path style), `<bucket>.s3.<region>.roll.site` (virtual host).
    S3 { bucket: Option<String> },
    /// `chg-<id>.roll.site`: a code preview (the whole label).
    Preview(String),
    /// `<bucket>.roll.site`: a public bucket or site (served by the CDN Worker).
    Site(String),
    Unknown,
}

/// The tenant products, by the label in their hostnames.
pub const TENANT_PRODUCTS: &[&str] = &["pg", "redis", "kafka", "nosql", "mq", "search"];

impl Host {
    /// Whether Cloudflare proxies this hostname (so `CF-Connecting-IP` from a Cloudflare address
    /// is the client).
    pub fn proxied(&self) -> bool {
        matches!(self, Host::Dashboard | Host::Api | Host::Preview(_) | Host::Site(_))
    }

    /// The tenant a tenant hostname names.
    pub fn tenant(&self) -> Option<&str> {
        match self {
            Host::Pg(t) | Host::Redis(t) | Host::Kafka(t) | Host::Nosql(t) | Host::Mq(t) | Host::Search(t) => Some(t),
            _ => None,
        }
    }
}

/// A certificate's worth of names, issued and stored together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertSet {
    /// The stored object's name (`system/certs/<name>.sealed`).
    pub name: String,
    pub names: Vec<String>,
}

fn label(s: &str) -> Option<String> {
    let ok = !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then(|| s.to_string())
}

/// `host` without a port or trailing dot, lowercase.
pub fn normalize(host: &str) -> String {
    let host = host.rsplit_once(':').filter(|(h, p)| (h.ends_with(']') || !h.contains(':')) && p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
    host.trim_end_matches('.').to_ascii_lowercase()
}

impl Hostnames {
    pub fn new(domain: &str, site_domain: &str, region: &str) -> Self {
        let clean = |s: &str| s.trim().trim_matches('.').to_ascii_lowercase();
        Self { domain: clean(domain), site_domain: clean(site_domain), region: clean(region) }
    }

    /// `<sub>.<region>.<domain>`: `product_domain("pg")` is `pg.us-east-1.roll.sh`.
    pub fn product_domain(&self, sub: &str) -> String {
        format!("{sub}.{}.{}", self.region, self.domain)
    }

    /// The S3 API's domain: `s3.us-east-1.roll.site`.
    pub fn s3_domain(&self) -> String {
        format!("s3.{}.{}", self.region, self.site_domain)
    }

    /// A tenant's hostname: `tenant("pg", "ep1")` is `ep1.pg.us-east-1.roll.sh`.
    pub fn tenant(&self, product: &str, id: &str) -> String {
        format!("{id}.{}", self.product_domain(product))
    }

    /// What `host` names. Accepts a `Host` header's form: any case, a port, a trailing dot.
    pub fn classify(&self, host: &str) -> Host {
        let host = normalize(host);
        if host == self.domain {
            return Host::Dashboard;
        }
        let region = |sub: &str| sub.strip_suffix(self.region.as_str()).and_then(|s| s.strip_suffix('.')).map(str::to_string);
        if let Some(sub) = host.strip_suffix(&self.domain).and_then(|s| s.strip_suffix('.')) {
            match sub {
                "www" => return Host::Dashboard,
                "api" => return Host::Api,
                "git" => return Host::Git,
                _ => {}
            }
            let Some(sub) = region(sub) else { return Host::Unknown };
            return match sub.as_str() {
                "tasks" => Host::Tasks,
                "origin" => Host::Origin,
                sub => match sub.split_once('.') {
                    Some((id, product)) => match (label(id), product) {
                        (Some(id), "pg") => Host::Pg(id),
                        (Some(id), "redis") => Host::Redis(id),
                        (Some(id), "kafka") => Host::Kafka(id),
                        (Some(id), "nosql") => Host::Nosql(id),
                        (Some(id), "mq") => Host::Mq(id),
                        (Some(id), "search") => Host::Search(id),
                        _ => Host::Unknown,
                    },
                    None => Host::Unknown,
                },
            };
        }
        if let Some(sub) = host.strip_suffix(&self.site_domain).and_then(|s| s.strip_suffix('.')) {
            if let Some(sub) = region(sub) {
                if sub == "s3" {
                    return Host::S3 { bucket: None };
                }
                // Bucket names may hold dots: everything before `.s3`.
                return match sub.strip_suffix(".s3").filter(|b| !b.is_empty()) {
                    Some(bucket) => Host::S3 { bucket: Some(bucket.to_string()) },
                    None => Host::Unknown,
                };
            }
            return match label(sub) {
                Some(l) if l.starts_with("chg-") => Host::Preview(l),
                Some(l) => Host::Site(l),
                None => Host::Unknown,
            };
        }
        Host::Unknown
    }

    /// The certificates the edge serves: one for the product's zone, one for the user-content
    /// zone (so the two never share a certificate). They cover every name in the Hostnames table
    /// except bucket names with dots in them (as on S3, those use path style over HTTPS).
    pub fn cert_sets(&self) -> Vec<CertSet> {
        let d = &self.domain;
        let mut product = vec![d.clone(), format!("api.{d}"), format!("git.{d}"), self.product_domain("tasks")];
        product.extend(TENANT_PRODUCTS.iter().map(|p| format!("*.{}", self.product_domain(p))));
        let s3 = self.s3_domain();
        let site = vec![format!("*.{}", self.site_domain), s3.clone(), format!("*.{s3}")];
        vec![CertSet { name: "product".into(), names: product }, CertSet { name: "site".into(), names: site }]
    }

    /// Every name in [`Hostnames::cert_sets`].
    pub fn all_cert_names(&self) -> Vec<String> {
        self.cert_sets().into_iter().flat_map(|s| s.names).collect()
    }
}

/// Whether a path on the dashboard's host is git smart HTTP (`/<owner>/<repo>.git[/…]`).
pub fn is_git_path(path: &str) -> bool {
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else { return false };
    !owner.is_empty() && repo.len() > 4 && repo.to_ascii_lowercase().ends_with(".git")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_the_table() {
        let h = Hostnames::new("roll.sh", "roll.site", "us-east-1");
        let cases = [
            ("roll.sh", Host::Dashboard),
            ("ROLL.sh.:443", Host::Dashboard),
            ("www.roll.sh", Host::Dashboard),
            ("api.roll.sh", Host::Api),
            ("git.roll.sh", Host::Git),
            ("ep_1.pg.us-east-1.roll.sh", Host::Pg("ep_1".into())),
            ("db1.redis.us-east-1.roll.sh:6379", Host::Redis("db1".into())),
            ("k1.kafka.us-east-1.roll.sh", Host::Kafka("k1".into())),
            ("n1.nosql.us-east-1.roll.sh", Host::Nosql("n1".into())),
            ("q1.mq.us-east-1.roll.sh", Host::Mq("q1".into())),
            ("shop.search.us-east-1.roll.sh", Host::Search("shop".into())),
            ("tasks.us-east-1.roll.sh", Host::Tasks),
            ("origin.us-east-1.roll.sh", Host::Origin),
            ("s3.us-east-1.roll.site", Host::S3 { bucket: None }),
            ("my.bucket.s3.us-east-1.roll.site", Host::S3 { bucket: Some("my.bucket".into()) }),
            ("chg-abc.roll.site", Host::Preview("chg-abc".into())),
            ("photos.roll.site", Host::Site("photos".into())),
            // Region-less endpoints, other regions, and names that aren't in the table.
            ("ep1.pg.roll.sh", Host::Unknown),
            ("tasks.roll.sh", Host::Unknown),
            ("x.s3.roll.site", Host::Unknown),
            ("ep1.pg.eu-west-1.roll.sh", Host::Unknown),
            ("a.b.pg.us-east-1.roll.sh", Host::Unknown),
            ("ep1.nope.us-east-1.roll.sh", Host::Unknown),
            ("us-east-1.roll.sh", Host::Unknown),
            ("evilroll.sh", Host::Unknown),
            ("example.com", Host::Unknown),
        ];
        for (host, want) in cases {
            assert_eq!(h.classify(host), want, "{host}");
        }
        let names = h.all_cert_names();
        for n in ["roll.sh", "api.roll.sh", "git.roll.sh", "tasks.us-east-1.roll.sh", "*.pg.us-east-1.roll.sh", "*.nosql.us-east-1.roll.sh", "*.mq.us-east-1.roll.sh", "*.search.us-east-1.roll.sh", "*.roll.site", "s3.us-east-1.roll.site", "*.s3.us-east-1.roll.site"] {
            assert!(names.contains(&n.to_string()), "{n}");
        }
        assert_eq!(h.tenant("kafka", "k1"), "k1.kafka.us-east-1.roll.sh");

        assert!(is_git_path("/o/r.git/info/refs") && is_git_path("/o/r.git") && is_git_path("/o/R.GIT/git-upload-pack"));
        assert!(!is_git_path("/o/r") && !is_git_path("/dashboard") && !is_git_path("/o/.git") && !is_git_path("//r.git"));
    }
}
