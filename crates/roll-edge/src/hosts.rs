//! The Hostnames table (docs/cloud/README.md): what a TLS server name or `Host` header names.

/// The zones and region every hostname is built from (`ROLL_EDGE_DOMAIN`,
/// `ROLL_EDGE_SITE_DOMAIN`, `ROLL_EDGE_REGION`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hostnames {
    /// `roll.cloud`
    pub domain: String,
    /// `roll.site`
    pub site_domain: String,
    /// `us-east-1`
    pub region: String,
}

/// What a hostname names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    /// `roll.cloud`
    Dashboard,
    /// `api.roll.cloud`
    Api,
    /// `git.roll.cloud`
    Git,
    /// `<id>.pg.<region>.<domain>`
    Pg(String),
    /// `<id>.redis.<region>.<domain>`
    Redis(String),
    /// `<id>.kafka.<region>.<domain>`
    Kafka(String),
    /// `<id>.cql.<region>.<domain>`
    Cql(String),
    /// `<id>.mq.<region>.<domain>`
    Mq(String),
    /// `s3.<region>.<domain>` (path style), `<bucket>.s3.<region>.<domain>` (virtual host).
    S3 { bucket: Option<String> },
    /// `tasks.<region>.<domain>`
    Tasks,
    /// `origin.<region>.<domain>`: the NLB itself.
    Origin,
    /// `chg-<id>.<site domain>`: a code preview (the whole label).
    Preview(String),
    /// `<bucket>.<site domain>`: a public bucket or site.
    Site(String),
    Unknown,
}

impl Host {
    /// Whether Cloudflare proxies this hostname (so `CF-Connecting-IP` from a Cloudflare address
    /// is the client).
    pub fn proxied(&self) -> bool {
        matches!(self, Host::Dashboard | Host::Api | Host::Preview(_) | Host::Site(_))
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

impl Hostnames {
    pub fn new(domain: &str, site_domain: &str, region: &str) -> Self {
        let clean = |s: &str| s.trim().trim_matches('.').to_ascii_lowercase();
        Self { domain: clean(domain), site_domain: clean(site_domain), region: clean(region) }
    }

    /// `<sub>.<region>.<domain>`
    pub fn regional(&self, sub: &str) -> String {
        format!("{sub}.{}.{}", self.region, self.domain)
    }

    /// A tenant's hostname: `tenant("pg", "ep1")` is `ep1.pg.us-east-1.roll.cloud`.
    pub fn tenant(&self, product: &str, id: &str) -> String {
        format!("{id}.{}", self.regional(product))
    }

    /// What `host` names. Accepts a `Host` header's form: any case, a port, a trailing dot.
    pub fn classify(&self, host: &str) -> Host {
        let host = host.rsplit_once(':').filter(|(h, p)| !h.ends_with(']') && p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host == self.domain {
            return Host::Dashboard;
        }
        if let Some(sub) = host.strip_suffix(&self.domain).and_then(|s| s.strip_suffix('.')) {
            match sub {
                "api" => return Host::Api,
                "git" => return Host::Git,
                "www" => return Host::Dashboard,
                _ => {}
            }
            let region = format!(".{}", self.region);
            if let Some(sub) = sub.strip_suffix(&region) {
                return match sub {
                    "s3" => Host::S3 { bucket: None },
                    "tasks" => Host::Tasks,
                    "origin" => Host::Origin,
                    _ => match sub.split_once('.') {
                        // Bucket names may hold dots: everything before `.s3`.
                        Some(_) if sub.ends_with(".s3") => Host::S3 { bucket: Some(sub.trim_end_matches(".s3").to_string()) },
                        Some((id, product)) => match (label(id), product) {
                            (Some(id), "pg") => Host::Pg(id),
                            (Some(id), "redis") => Host::Redis(id),
                            (Some(id), "kafka") => Host::Kafka(id),
                            (Some(id), "cql") => Host::Cql(id),
                            (Some(id), "mq") => Host::Mq(id),
                            _ => Host::Unknown,
                        },
                        None => Host::Unknown,
                    },
                };
            }
            return Host::Unknown;
        }
        if let Some(sub) = host.strip_suffix(&self.site_domain).and_then(|s| s.strip_suffix('.')) {
            return match label(sub) {
                Some(l) if l.starts_with("chg-") => Host::Preview(l),
                Some(l) => Host::Site(l),
                None => Host::Unknown,
            };
        }
        Host::Unknown
    }

    /// The certificates the edge serves: one for the product's zone, one for the user-content
    /// zone (so the two never share a certificate). Every name in the Hostnames table is covered
    /// by one of these, except bucket names with dots in them (as on S3, they use path style).
    pub fn cert_sets(&self) -> Vec<CertSet> {
        let d = &self.domain;
        let mut cloud = vec![d.clone(), format!("*.{d}"), format!("*.{}.{d}", self.region)];
        for p in ["pg", "redis", "kafka", "cql", "mq", "s3"] {
            cloud.push(format!("*.{}", self.regional(p)));
        }
        let s = &self.site_domain;
        vec![CertSet { name: "cloud".into(), names: cloud }, CertSet { name: "site".into(), names: vec![s.clone(), format!("*.{s}")] }]
    }

    /// Every name in [`Hostnames::cert_sets`].
    pub fn all_cert_names(&self) -> Vec<String> {
        self.cert_sets().into_iter().flat_map(|s| s.names).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_the_table() {
        let h = Hostnames::new("roll.cloud", "roll.site", "us-east-1");
        let cases = [
            ("roll.cloud", Host::Dashboard),
            ("ROLL.cloud.:443", Host::Dashboard),
            ("api.roll.cloud", Host::Api),
            ("git.roll.cloud", Host::Git),
            ("ep_1.pg.us-east-1.roll.cloud", Host::Pg("ep_1".into())),
            ("db1.redis.us-east-1.roll.cloud:6379", Host::Redis("db1".into())),
            ("k1.kafka.us-east-1.roll.cloud", Host::Kafka("k1".into())),
            ("n1.cql.us-east-1.roll.cloud", Host::Cql("n1".into())),
            ("q1.mq.us-east-1.roll.cloud", Host::Mq("q1".into())),
            ("s3.us-east-1.roll.cloud", Host::S3 { bucket: None }),
            ("my.bucket.s3.us-east-1.roll.cloud", Host::S3 { bucket: Some("my.bucket".into()) }),
            ("tasks.us-east-1.roll.cloud", Host::Tasks),
            ("origin.us-east-1.roll.cloud", Host::Origin),
            ("chg-abc.roll.site", Host::Preview("chg-abc".into())),
            ("photos.roll.site", Host::Site("photos".into())),
            ("a.b.pg.us-east-1.roll.cloud", Host::Unknown),
            ("ep1.pg.eu-west-1.roll.cloud", Host::Unknown),
            ("ep1.nope.us-east-1.roll.cloud", Host::Unknown),
            ("evilroll.cloud", Host::Unknown),
            ("example.com", Host::Unknown),
        ];
        for (host, want) in cases {
            assert_eq!(h.classify(host), want, "{host}");
        }
        let names = h.all_cert_names();
        assert!(names.contains(&"*.pg.us-east-1.roll.cloud".to_string()) && names.contains(&"*.roll.site".to_string()));
        assert_eq!(h.tenant("kafka", "k1"), "k1.kafka.us-east-1.roll.cloud");
    }
}
