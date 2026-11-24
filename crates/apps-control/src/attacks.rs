//! Managed attack rules for firewall rules (Vercel's OWASP core ruleset, Cloud Armor's
//! preconfigured WAF rules; docs/apps/README.md, "Firewall rules"): a rule's `attacks` names
//! kinds of attack rather than their strings, from patterns roll keeps with its releases. They
//! read the request line and a few headers (the user agent, the referer, cookies), never a body:
//! - `scanners`: probes for what a site might have left out (`/.env`, `/.git/`, `/wp-login.php`
//!   on a site that isn't WordPress) and the scanners' own user agents (sqlmap, Nikto, Nuclei…).
//! - `traversal`: `../` in any encoding, a NUL byte, well-known system files.
//! - `sqli`: SQL injection's usual shapes (`union select`, `' or 1=1`, `sleep(`…).
//! - `xss`: markup and script in the query or the path (`<script`, `onerror=`, `javascript:`).
//! - `rce`: Log4Shell's `${jndi:`, Shellshock's `() {`, shell and PHP in a parameter.
//!
//! Patterns are matched in lower case after percent-decoding (twice, as a double-encoded probe is
//! decoded twice by some stacks), so `%2e%2e%2f` and `%252e%252e%252f` are `../`. A pattern set
//! is a blunt tool: try a rule as `log` and read its count before it denies.

use crate::model::Asked;

/// A kind a firewall rule's `attacks` names.
pub const KINDS: [&str; 5] = ["scanners", "traversal", "sqli", "xss", "rce"];

/// Paths scanners probe (a prefix of the path, lower case).
const PROBES: &[&str] = &[
    "/.env", "/.git/", "/.git", "/.svn/", "/.hg/", "/.ds_store", "/.htaccess", "/.htpasswd", "/.aws/", "/.ssh/", "/.npmrc", "/.dockerenv",
    "/wp-login.php", "/wp-admin", "/xmlrpc.php", "/wp-config.php", "/wp-content/plugins/", "/phpmyadmin", "/pma/", "/myadmin", "/server-status",
    "/server-info", "/cgi-bin/", "/vendor/phpunit/", "/actuator/", "/console/", "/solr/", "/boaform/", "/hnap1", "/manager/html", "/admin.php",
    "/config.php", "/setup.php", "/install.php", "/shell.php", "/web.config", "/backup.sql", "/dump.sql", "/database.sql", "/id_rsa",
    "/autodiscover/autodiscover.xml", "/owa/", "/ecp/", "/remote/login", "/+cscoe+/", "/telescope/requests", "/_ignition/",
];

/// Scanners' user agents (lower case, contained).
const SCANNERS: &[&str] = &["sqlmap", "nikto", "nuclei", "masscan", "zgrab", "acunetix", "wpscan", "dirbuster", "gobuster", "ffuf", "feroxbuster", "netsparker", "nessus", "openvas", "w3af", "arachni", "whatweb", "jaeles", "httpx-toolkit", "fuzz faster"];

const TRAVERSAL: &[&str] = &["../", "..\\", "/etc/passwd", "/etc/shadow", "/proc/self/", "win.ini", "boot.ini", "\0", "/windows/system32"];

const SQLI: &[&str] = &[
    "union select", "union all select", "' or '1'='1", "\" or \"1\"=\"1", "' or 1=1", " or 1=1--", "or 1=1#", "sleep(", "benchmark(", "pg_sleep(",
    "waitfor delay", "information_schema", "xp_cmdshell", "; drop table", "';--", "' --", "extractvalue(", "updatexml(", "load_file(", "into outfile",
];

const XSS: &[&str] = &["<script", "</script", "javascript:", "onerror=", "onload=", "onmouseover=", "onfocus=", "<svg", "<iframe", "<img src", "document.cookie", "alert(", "prompt(", "<body onload", "vbscript:", "srcdoc="];

const RCE: &[&str] = &["${jndi:", "${${", "() {", "<?php", "system(", "passthru(", "shell_exec(", "base64_decode(", ";wget ", "|wget ", ";curl ", "|curl ", "$(curl", "`curl", "/bin/sh", "/bin/bash", "cmd.exe", "powershell -"];

/// Percent-decoded once, `+` a space (a query's), lower case.
fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().to_ascii_lowercase()
}

/// What the patterns read of a request: its path and query as sent and decoded twice, and the
/// headers attacks ride in.
struct Seen {
    texts: Vec<String>,
}

fn seen(a: &Asked<'_>) -> Seen {
    let raw = format!("{}?{}", a.raw_path, a.query.unwrap_or(""));
    let once = decode(&raw);
    let twice = decode(&once);
    let mut texts = vec![raw.to_ascii_lowercase(), once, twice];
    for h in [axum::http::header::USER_AGENT, axum::http::header::REFERER, axum::http::header::COOKIE] {
        for v in a.headers.get_all(&h).iter().filter_map(|v| v.to_str().ok()) {
            texts.push(decode(v));
        }
    }
    Seen { texts }
}

fn any(texts: &[String], patterns: &[&str]) -> bool {
    texts.iter().any(|t| patterns.iter().any(|p| t.contains(p)))
}

/// Whether any of `kinds` takes the request.
pub fn matches(kinds: &[String], a: &Asked<'_>) -> bool {
    // Bounded work: a request line past 8 KiB is read as far as that.
    let s = seen(a);
    let texts: Vec<String> = s.texts.into_iter().map(|t| t.chars().take(8192).collect()).collect();
    kinds.iter().any(|k| match k.as_str() {
        "scanners" => {
            // As the app would read it (`//wp-admin`, `/./cgi-bin`, `/wp-admin;x` are the probes too).
            let normal = crate::model::normal_path(a.raw_path).to_ascii_lowercase();
            let p = normal.trim_end_matches('/');
            PROBES.iter().any(|probe| {
                let probe = probe.trim_end_matches('/');
                p == probe || p.starts_with(&format!("{probe}/")) || (probe.contains('.') && p.ends_with(probe))
            }) || SCANNERS.iter().any(|ua| a.user_agent.to_ascii_lowercase().contains(ua))
        }
        "traversal" => any(&texts, TRAVERSAL),
        "sqli" => any(&texts, SQLI),
        "xss" => any(&texts, XSS),
        "rce" => any(&texts, RCE),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(raw_path: &str, query: Option<&str>, ua: &str, headers: &axum::http::HeaderMap) -> bool {
        let a = Asked { method: "GET", path: raw_path, raw_path, user_agent: ua, ip: None, headers, query, bot: None };
        matches(&KINDS.iter().map(|k| k.to_string()).collect::<Vec<_>>(), &a)
    }

    #[test]
    fn attacks_are_caught_in_any_encoding_and_ordinary_requests_are_not() {
        let h = axum::http::HeaderMap::new();
        for (path, q) in [
            ("/.env", None),
            ("/.git/config", None),
            ("/blog/wp-login.php", None),
            ("/download", Some("file=..%2f..%2fetc%2fpasswd")),
            ("/download", Some("file=%252e%252e%252fetc%252fpasswd")),
            ("/items", Some("id=1%27%20UNION%20SELECT%20password%20FROM%20users")),
            ("/search", Some("q=%3Cscript%3Ealert(1)%3C/script%3E")),
            ("/x", Some("v=${jndi:ldap://evil/a}")),
        ] {
            assert!(ask(path, q, "Mozilla/5.0", &h), "{path}?{q:?}");
        }
        for path in ["//wp-admin", "/./cgi-bin/x", "/wp-admin;x=1", "/WP-LOGIN.PHP"] {
            assert!(ask(path, None, "Mozilla/5.0", &h), "{path} as the app reads it");
        }
        assert!(ask("/", None, "sqlmap/1.8", &h), "a scanner's user agent");
        let mut shellshock = axum::http::HeaderMap::new();
        shellshock.insert(axum::http::header::REFERER, "() { :; }; /bin/sh -c id".parse().unwrap());
        assert!(ask("/cgi", None, "Mozilla/5.0", &shellshock));
        for (path, q) in [("/", None), ("/blog/2024/union-station", Some("q=select+a+plan&page=2")), ("/env", None), ("/docs/getting-started", Some("from=..."))] {
            assert!(!ask(path, q, "Mozilla/5.0 (Macintosh) Safari/605.1.15", &h), "{path}?{q:?}");
        }
    }
}
