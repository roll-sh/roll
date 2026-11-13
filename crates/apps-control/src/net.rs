//! Fetching a URL a tenant chose (a registry, a notification target), as AGENTS.md asks: a
//! resolver that refuses private addresses, a check for addresses written out (IP literals never
//! reach the resolver), no redirects, timeouts, and bounded reads.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Resolves names, refusing private addresses.
pub struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| !crate::model::private_ip(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{host} resolves to a private address").into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Whether `host` (a URL's host, `[…]` for IPv6) is a private address written out or `localhost`.
pub fn literal_private(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.to_ascii_lowercase().ends_with(".localhost") || h.parse().is_ok_and(crate::model::private_ip)
}

/// A client for tenant-chosen URLs: public addresses only (unless `allow_private`, local
/// development and tests), no redirects, short timeouts.
pub fn client(allow_private: bool, timeout: Duration) -> reqwest::Client {
    let mut b = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).timeout(timeout).redirect(reqwest::redirect::Policy::none()).no_proxy().user_agent("roll-apps");
    if !allow_private {
        b = b.dns_resolver(Arc::new(PublicOnly));
    }
    b.build().expect("a reqwest client builds")
}

/// At most `max` bytes of a response's body (the rest isn't read).
pub async fn read_bounded(mut res: reqwest::Response, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = res.chunk().await {
        let room = max.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= max {
            break;
        }
    }
    out
}
