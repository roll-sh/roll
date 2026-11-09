//! Shovel and federation URIs. `amqp://` without a host means the local broker, like in
//! RabbitMQ; a URI without a path uses the vhost of the shovel or federated resource.

use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

/// Where an endpoint lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The in-process broker (no TCP).
    Local { vhost: String },
    /// A remote AMQP 0-9-1 broker; `uri` always carries an explicit vhost.
    Remote { uri: String, host: String, port: u16, tls: bool, vhost: String },
}

/// Percent-encode a vhost or credential for use in a URI.
pub fn encode(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// Parse a shovel/federation URI; `default_vhost` applies when the URI has no path.
pub fn parse(uri: &str, default_vhost: &str) -> Result<Target, String> {
    let (tls, rest) = if let Some(r) = uri.strip_prefix("amqps://") {
        (true, r)
    } else if let Some(r) = uri.strip_prefix("amqp://") {
        (false, r)
    } else {
        return Err(format!("unsupported URI scheme in '{}' (expected amqp:// or amqps://)", redact(uri)));
    };
    let (before_query, query) = match rest.find('?') {
        Some(i) => (&rest[..i], Some(&rest[i..])),
        None => (rest, None),
    };
    let (authority, path) = match before_query.find('/') {
        Some(i) => (&before_query[..i], Some(&before_query[i + 1..])),
        None => (before_query, None),
    };
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if hostport.is_empty() {
        if tls {
            return Err("amqps:// requires a host".into());
        }
        let vhost = match path {
            Some(p) if !p.is_empty() => percent_decode_str(p).decode_utf8_lossy().into_owned(),
            _ => default_vhost.to_string(),
        };
        return Ok(Target::Local { vhost });
    }
    let (host, port) = split_host_port(hostport, tls)?;
    let (uri, vhost) = match path {
        Some(p) => (uri.to_string(), percent_decode_str(p).decode_utf8_lossy().into_owned()),
        None => {
            let scheme = if tls { "amqps://" } else { "amqp://" };
            (format!("{scheme}{authority}/{}{}", encode(default_vhost), query.unwrap_or("")), default_vhost.to_string())
        }
    };
    Ok(Target::Remote { uri, host, port, tls, vhost })
}

fn split_host_port(hp: &str, tls: bool) -> Result<(String, u16), String> {
    let default = if tls { 5671 } else { 5672 };
    if let Some(rest) = hp.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or("invalid IPv6 host")?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| format!("invalid port '{p}'"))?,
            None => default,
        };
        return Ok((h.to_string(), port));
    }
    match hp.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), p.parse().map_err(|_| format!("invalid port '{p}'"))?)),
        None => Ok((hp.to_string(), default)),
    }
}

/// The URI without credentials (status output, headers).
pub fn redact(uri: &str) -> String {
    let Some(i) = uri.find("://") else { return uri.to_string() };
    let (scheme, rest) = uri.split_at(i + 3);
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) => format!("{scheme}{}", &rest[at + 1..]),
        None => uri.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_remote() {
        assert_eq!(parse("amqp://", "v").unwrap(), Target::Local { vhost: "v".into() });
        assert_eq!(parse("amqp:///%2f", "v").unwrap(), Target::Local { vhost: "/".into() });
        assert_eq!(parse("amqp://user@/other", "v").unwrap(), Target::Local { vhost: "other".into() });
        let Target::Remote { uri, host, port, tls, vhost } = parse("amqp://u:p@h:1234", "a/b").unwrap() else { panic!() };
        assert_eq!((uri.as_str(), host.as_str(), port, tls, vhost.as_str()), ("amqp://u:p@h:1234/a%2Fb", "h", 1234, false, "a/b"));
        let Target::Remote { uri, port, tls, vhost, .. } = parse("amqps://h/x?heartbeat=5", "v").unwrap() else { panic!() };
        assert_eq!((uri.as_str(), port, tls, vhost.as_str()), ("amqps://h/x?heartbeat=5", 5671, true, "x"));
        let Target::Remote { uri, .. } = parse("amqp://h?heartbeat=5", "/").unwrap() else { panic!() };
        assert_eq!(uri, "amqp://h/%2F?heartbeat=5");
        assert!(parse("http://h", "/").is_err());
    }

    #[test]
    fn redacts() {
        assert_eq!(redact("amqp://u:secret@h:5672/%2f"), "amqp://h:5672/%2f");
        assert_eq!(redact("amqp://h/v"), "amqp://h/v");
        assert_eq!(redact("amqp://"), "amqp://");
    }
}
