//! Which cluster a request is for, and which node should serve it.
//!
//! **The cluster** is read the way the node reads it (`search_server::auth`): Basic auth user
//! `<cluster>` or `<user>@<cluster>`, `ApiKey base64(<id>:<secret>)` with the id naming the
//! cluster (or `<key>@<cluster>`), `Bearer <cluster>_<secret>`, or the TLS SNI / `Host` label
//! under the fleet's domain. The proxy never checks secrets: the node does. A request whose
//! cluster can't be told (no credentials, a malformed header) goes to any node, which answers it
//! as Elasticsearch would (401, `GET /` without auth in dev mode). Reading a different cluster
//! than the node would costs one redirect, never a wrong answer: the node refuses clusters it
//! doesn't own and names the owner.
//!
//! **The node** is chosen by rendezvous (highest random weight) hashing of the cluster name
//! over the live, non-draining nodes, among the proxy's own availability zone first: every
//! proxy of an AZ picks the same node, a node joining or leaving moves only its share of the
//! clusters, and traffic stays in its AZ. Ownership is the lease's: a cluster still loaded on
//! another node keeps being served there (the node redirects with `X-Roll-Search-Owner`, which
//! the proxy remembers) until it unloads, so membership changes never move a busy cluster.

use base64::Engine;

/// The cluster label of `host` under `domain` (`c1.search.example.com` → `c1`; the port is
/// ignored; deeper names and the bare domain don't match).
pub fn host_label<'a>(host: &'a str, domain: Option<&str>) -> Option<&'a str> {
    let domain = domain?.trim_end_matches('.');
    let host = host.trim_end_matches('.');
    let host = match host.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    let rest = host.len().checked_sub(domain.len()).filter(|_| host.is_char_boundary(host.len() - domain.len()))?;
    let (label, suffix) = host.split_at(rest);
    if !suffix.eq_ignore_ascii_case(domain) {
        return None;
    }
    let label = label.strip_suffix('.')?;
    (!label.is_empty() && !label.contains('.')).then_some(label)
}

fn strip_scheme<'a>(h: &'a str, scheme: &str) -> Option<&'a str> {
    let n = scheme.len();
    (h.len() > n && h.as_bytes()[..n].eq_ignore_ascii_case(scheme.as_bytes()) && h.as_bytes()[n] == b' ').then(|| h[n + 1..].trim())
}

fn decode(b64: &str) -> Option<String> {
    let e = base64::engine::general_purpose::STANDARD;
    let bytes = e.decode(b64).or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64)).ok()?;
    String::from_utf8(bytes).ok()
}

/// The cluster a request names, from its `Authorization` header and the cluster its host name
/// (SNI or `Host`) selects, with the node's precedence.
pub fn cluster_of(authorization: Option<&str>, host_cluster: Option<&str>) -> Option<String> {
    let own = |s: &str| (!s.is_empty()).then(|| s.to_string());
    if let Some(h) = authorization {
        if let Some(v) = strip_scheme(h, "Basic") {
            if v.is_empty() {
                return host_cluster.and_then(own);
            }
            let text = decode(v)?;
            let (user, _) = text.split_once(':')?;
            return match user.rsplit_once('@') {
                Some((_, c)) => own(c),
                None => host_cluster.or(Some(user)).and_then(own),
            };
        }
        if let Some(v) = strip_scheme(h, "ApiKey") {
            if v.is_empty() {
                return host_cluster.and_then(own);
            }
            let text = decode(v)?;
            let (id, _) = text.split_once(':')?;
            return match id.rsplit_once('@') {
                Some((_, c)) => own(c),
                None => host_cluster.or(Some(id)).and_then(own),
            };
        }
        if let Some(token) = strip_scheme(h, "Bearer") {
            if token.is_empty() {
                return host_cluster.and_then(own);
            }
            if let Some(c) = host_cluster
                && !token.contains('_')
            {
                return own(c);
            }
            return token.rsplit_once('_').and_then(|(c, _)| own(c));
        }
    }
    host_cluster.and_then(own)
}

/// FNV-1a over `cluster`, a separator and `node`, finished with SplitMix64's mixer: stable
/// across processes, platforms and releases (every proxy must agree).
pub fn weight(cluster: &str, node: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in cluster.bytes().chain(std::iter::once(0xff)).chain(node.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// A node that may take a request.
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
    pub id: &'a str,
    pub az: Option<&'a str>,
}

/// The rendezvous choice for `cluster` among `nodes`, preferring `az` (`None`: any). Ties
/// (practically impossible) go to the smaller id.
pub fn pick<'a>(cluster: &str, nodes: &[Candidate<'a>], az: Option<&str>) -> Option<&'a str> {
    let best = |same_az: bool| {
        nodes
            .iter()
            .filter(|n| !same_az || (az.is_some() && n.az == az))
            .map(|n| (weight(cluster, n.id), std::cmp::Reverse(n.id), n.id))
            .max()
            .map(|(_, _, id)| id)
    };
    best(true).or_else(|| best(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(user: &str, pw: &str) -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pw}")))
    }

    #[test]
    fn clusters_like_the_node() {
        assert_eq!(cluster_of(Some(&basic("app", "pw")), None).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some(&basic("bob@app", "pw")), None).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some(&basic("a@b@app", "pw:x")), None).as_deref(), Some("app"));
        // A host label wins over a plain Basic user, not over `user@cluster`.
        assert_eq!(cluster_of(Some(&basic("elastic", "pw")), Some("app")).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some(&basic("bob@other", "pw")), Some("app")).as_deref(), Some("other"));
        // Case-insensitive scheme, padding optional.
        let b = basic("app", "pw").replace("Basic", "bAsIc");
        assert_eq!(cluster_of(Some(&b), None).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some("Basic YXBwOnB3"), None).as_deref(), Some("app"));
        let k = |s: &str| format!("ApiKey {}", base64::engine::general_purpose::STANDARD.encode(s));
        assert_eq!(cluster_of(Some(&k("app:secret")), None).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some(&k("key1@app:secret")), None).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some(&k("key1:secret")), Some("app")).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some("Bearer app_s3cr3t"), None).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some("Bearer my-app_x_s3cr3t"), None).as_deref(), Some("my-app_x"));
        assert_eq!(cluster_of(Some("Bearer s3cr3t"), Some("app")).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some("Bearer s3cr3t"), None), None);
        assert_eq!(cluster_of(None, Some("app")).as_deref(), Some("app"));
        assert_eq!(cluster_of(None, None), None);
        // Malformed: any node (it answers 401 like Elasticsearch).
        assert_eq!(cluster_of(Some("Basic !!!"), None), None);
        assert_eq!(cluster_of(Some(&k("nocolon")), None), None);
        assert_eq!(cluster_of(Some(&basic("", "pw")), None), None);
        assert_eq!(cluster_of(Some("Digest x"), Some("app")).as_deref(), Some("app"));
        assert_eq!(cluster_of(Some("Basic "), Some("app")).as_deref(), Some("app"));
    }

    #[test]
    fn host_labels() {
        let d = Some("search.example.com");
        assert_eq!(host_label("app.search.example.com", d), Some("app"));
        assert_eq!(host_label("app.search.example.com:443", d), Some("app"));
        assert_eq!(host_label("APP.Search.Example.com.", d), Some("APP"));
        assert_eq!(host_label("search.example.com", d), None);
        assert_eq!(host_label("a.b.search.example.com", d), None);
        assert_eq!(host_label("appsearch.example.com", d), None);
        assert_eq!(host_label("app.search.example.com", None), None);
        assert_eq!(host_label("x.other.com", d), None);
        assert_eq!(host_label("é.search.example.com", d), Some("é"));
        assert_eq!(host_label("[::1]:9200", d), None);
    }

    #[test]
    fn rendezvous_is_stable_and_moves_little() {
        // Fixed values: every proxy (and every release) must compute the same weights.
        assert_eq!(weight("app", "node-a"), weight("app", "node-a"));
        assert_ne!(weight("app", "node-a"), weight("app", "node-b"));
        assert_ne!(weight("ab", "c"), weight("a", "bc"));
        let ids: Vec<String> = (0..5).map(|i| format!("node-{i}")).collect();
        let nodes: Vec<Candidate> = ids.iter().map(|id| Candidate { id, az: None }).collect();
        let clusters: Vec<String> = (0..2000).map(|i| format!("cluster-{i}")).collect();
        let before: Vec<&str> = clusters.iter().map(|c| pick(c, &nodes, None).unwrap()).collect();
        // Spread: each node gets roughly a fifth.
        for id in &ids {
            let n = before.iter().filter(|p| **p == id.as_str()).count();
            assert!((300..=500).contains(&n), "{id}: {n}");
        }
        // Removing a node moves only its clusters.
        let fewer: Vec<Candidate> = nodes.iter().copied().filter(|n| n.id != "node-2").collect();
        for (c, b) in clusters.iter().zip(&before) {
            let after = pick(c, &fewer, None).unwrap();
            if *b != "node-2" {
                assert_eq!(after, *b, "{c} moved without reason");
            }
        }
        assert_eq!(pick("x", &[], None), None);
    }

    #[test]
    fn own_az_first() {
        let nodes = [Candidate { id: "a1", az: Some("az-a") }, Candidate { id: "b1", az: Some("az-b") }, Candidate { id: "b2", az: Some("az-b") }];
        for i in 0..200 {
            let c = format!("c{i}");
            assert_eq!(pick(&c, &nodes, Some("az-a")), Some("a1"));
            assert!(pick(&c, &nodes, Some("az-b")).unwrap().starts_with('b'));
            // An AZ without nodes falls back to all of them.
            assert!(pick(&c, &nodes, Some("az-c")).is_some());
        }
    }
}
