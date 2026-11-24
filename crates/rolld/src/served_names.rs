//! The names the fleet answers on, from every product's arguments: the edge's domains (the
//! dashboard and API on the product domain, user content on the site domain), each product's
//! public and advertised names, and the internal ones. The web crawler never fetches them
//! (`roll.crawler.fleet_names`, docs/search/crawler.md "Blocked hosts"), by name as well as by
//! the address check every outbound call makes; they're the crawler's alone, so other egress
//! callers' deny lists don't grow. Loopback (a development daemon's defaults) isn't listed: the
//! egress check refuses it unless private hosts are allowed for a local test site.

use crate::Args;

/// The host of each argument that names one roll serves, by its flag; `None` when unset.
fn served(args: &Args) -> Vec<(&'static str, Option<String>)> {
    let edge = args.edge.enabled;
    vec![
        ("edge-domain", edge.then(|| args.edge.domain.clone())),
        ("edge-site-domain", edge.then(|| args.edge.site_domain.clone())),
        ("dashboard-upstream", args.edge.dashboard_upstream.clone()),
        ("public-host", Some(args.public_host.clone())),
        ("internal-url", args.internal_url.clone()),
        ("kafka-advertised-host", args.kafka_advertised_host.clone()),
        ("code-public-url", args.code_public_url.clone()),
        ("code-git-url", args.code_git_url.clone()),
        ("web-url", Some(args.web_url.clone())),
        ("code-preview-domain", Some(args.code_preview_domain.clone())),
        ("code-advertise-url", args.code.code_advertise_url.clone()),
        ("advertise-url", args.code.cluster.advertise_url.clone()),
        ("s3-domain", args.s3_domain.clone()),
        ("s3-website-domain", args.s3_website_domain.clone()),
        ("s3-public-endpoint", args.s3_public_endpoint.clone()),
        ("storage-api-public-url", args.storage_api_public_url.clone()),
        ("apps-domain", args.apps.domain.clone()),
        ("apps-ports-host", args.apps.ports_host.map(|ip| ip.to_string())),
        ("nosql-domain", args.nosql.nosql_domain.clone()),
        ("nosql-alternator-domain", args.nosql.nosql_alternator_domain.clone()),
        ("queues-advertise-host", args.queues.advertise_host.clone()),
        ("search-domain", args.search.search_domain.clone()),
        ("search-public-url", args.search.search_public_url.clone()),
    ]
}

/// A URL's or `host[:port]`'s host, lowercase, without a trailing dot or brackets.
fn host_of(v: &str) -> Option<String> {
    let v = v.trim();
    let host = match v.contains("://") {
        true => reqwest::Url::parse(v).ok()?.host_str()?.to_string(),
        false => match v.parse::<std::net::IpAddr>() {
            Ok(ip) => ip.to_string(),
            Err(_) => v.rsplit_once(':').filter(|(_, p)| p.parse::<u16>().is_ok()).map_or(v, |(h, _)| h).to_string(),
        },
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').trim_start_matches('.').trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn loopback(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// Every name of [`served`] that is set, once each.
pub(crate) fn fleet_names(args: &Args) -> Vec<String> {
    let mut names: Vec<String> = served(args).into_iter().filter_map(|(_, v)| v.as_deref().and_then(host_of)).filter(|h| !loopback(h)).collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    /// Arguments whose names look like a host's but aren't one roll answers on.
    const NOT_SERVED: &[&str] = &[
        "s3-endpoint", // the backing object store, called out to
        "s3-notify-aws-endpoint", // AWS, for bucket notifications
        "s3-notify-aws-endpoints",
        "cluster-dynamodb-endpoint", // DynamoDB, for leases
        "search-embed-url", // an embedding model, called out to
        "default-endpoint", // a Postgres endpoint id
        "kafka-advertised-port",
        "edge-origin-pull", // a switch
        "edge-origin-pull-ca", // a file
        "cdn-origin-secret",
        "queues-vhost-publish-rate", // "vhost", a rate
        "queues-vhost-publish-bytes-rate",
    ];

    /// Every argument that names a host is either one roll serves (and its value reaches the
    /// crawler's list) or one roll calls out to.
    #[test]
    fn every_served_name_argument_is_a_fleet_name() {
        let cmd = Args::command();
        let named = |l: &str| ["domain", "url", "host", "advertise", "endpoint", "origin"].iter().any(|w| l.contains(w));
        let longs: Vec<String> = cmd.get_arguments().filter_map(|a| a.get_long()).filter(|l| named(l)).map(str::to_string).collect();
        let args = Args::parse_from(["rolld"]);
        let known: Vec<&str> = served(&args).iter().map(|(f, _)| *f).collect();
        let unsorted: Vec<&String> = longs.iter().filter(|l| !known.contains(&l.as_str()) && !NOT_SERVED.contains(&l.as_str())).collect();
        assert!(unsorted.is_empty(), "add these to served_names::served (a name roll answers on) or NOT_SERVED: {unsorted:?}");

        // Each served argument, set to a name of its own, reaches the list.
        let mut argv = vec!["rolld".to_string(), "--edge".into()];
        let mut want = Vec::new();
        for (i, flag) in known.iter().enumerate() {
            let value = match *flag {
                "apps-ports-host" => format!("203.0.113.{}", i + 1),
                f if f.ends_with("url") || f.ends_with("upstream") || f.ends_with("endpoint") => format!("https://n{i}.served.example:8443/x"),
                _ => format!("n{i}.served.example"),
            };
            want.push(host_of(&value).unwrap());
            argv.push(format!("--{flag}={value}"));
        }
        let names = fleet_names(&Args::parse_from(argv));
        let missing: Vec<&String> = want.iter().filter(|w| !names.contains(w)).collect();
        assert!(missing.is_empty(), "{missing:?} not in {names:?}");
        assert!(fleet_names(&Args::parse_from(["rolld"])).is_empty(), "a development daemon's loopback defaults aren't listed");
    }
}
