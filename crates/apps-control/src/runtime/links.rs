//! Linked roll databases from containers on one node (docs/apps/README.md, "Links on one
//! node"). A link's URL names this host's loopback (`127.0.0.1:55432`, rolld's Postgres proxy),
//! which a container can't reach: its `host.docker.internal` is the bridge's gateway, a
//! loopback listener doesn't answer there, and a host firewall may drop every packet from a
//! container to the host anyway. So the port is served on a unix socket instead
//! (`<home>/apps/links/<port>/s.sock`, forwarded to the loopback port it stands for), and a
//! relay on the project's network (`socat`, pinned by digest) answers TCP on that port under the
//! alias `roll-link-<port>`, connecting each caller to the socket through a bind mount, which no
//! firewall sees. A container reaches only the ports its project's links name, never the rest of
//! the host.

use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// The relay: `socat` on Alpine, pinned (it runs on every node that hosts apps).
pub const IMAGE: &str = "alpine/socat:1.8.0.3@sha256:beb4a68d9e4fe6b0f21ea774a0fde6c31f580dde6368939ed70100c5385b015e";
/// A relay's alias on its network: `roll-link-55432`.
const ALIAS: &str = "roll-link-";
/// A relay container's name: the network's with `link` for `net`, and the port.
pub const NAME_PREFIX: &str = "roll-apps-link-";

pub struct Links {
    dir: PathBuf,
    /// Ports served on a socket, and the loopback host each stands for.
    serving: Mutex<HashMap<u16, String>>,
}

impl Links {
    /// Sockets under `dir`; a previous process's are gone (its listeners are).
    pub fn new(dir: PathBuf) -> Links {
        let _ = std::fs::remove_dir_all(&dir);
        Links { dir, serving: Mutex::new(HashMap::new()) }
    }

    /// The port's socket directory (a relay mounts it).
    pub fn socket_dir(&self, port: u16) -> PathBuf {
        self.dir.join(port.to_string())
    }

    /// `url` with each loopback `host:port` as its relay's (`roll-link-<port>:<port>`), each such
    /// port served from now on. Anything else stays as it is.
    pub fn reach(&self, url: &str) -> String {
        let (out, found) = rewrite(url);
        for (host, port) in found {
            self.serve(&host, port);
        }
        out
    }

    fn serve(&self, host: &str, port: u16) {
        if self.serving.lock().contains_key(&port) {
            return;
        }
        let dir = self.socket_dir(port);
        let path = dir.join("s.sock");
        let made = (|| -> std::io::Result<std::os::unix::net::UnixListener> {
            std::fs::create_dir_all(&dir)?;
            let _ = std::fs::remove_file(&path);
            let l = std::os::unix::net::UnixListener::bind(&path)?;
            // The relay's process may not be this user (rootless Docker, user namespaces); the
            // socket only reaches a port anyone on the host reaches already.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
            l.set_nonblocking(true)?;
            Ok(l)
        })();
        let listener = match made.and_then(tokio::net::UnixListener::from_std) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(port, error = %e, "apps: a link's socket couldn't be made; containers won't reach it");
                return;
            }
        };
        self.serving.lock().insert(port, host.to_string());
        let target = format!("{host}:{port}");
        tokio::spawn(async move {
            loop {
                let Ok((mut caller, _)) = listener.accept().await else { continue };
                let target = target.clone();
                tokio::spawn(async move {
                    if let Ok(mut up) = tokio::net::TcpStream::connect(&target).await {
                        let _ = tokio::io::copy_bidirectional(&mut caller, &mut up).await;
                    }
                });
            }
        });
    }
}

/// The relay ports `vars`' values name (`roll-link-<port>:`).
pub fn ports(vars: &[(String, String)]) -> BTreeSet<u16> {
    let mut out = BTreeSet::new();
    for (_, v) in vars {
        let mut rest = v.as_str();
        while let Some(i) = rest.find(ALIAS) {
            rest = &rest[i + ALIAS.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(p) = digits.parse::<u16>()
                && rest[digits.len()..].starts_with(':')
            {
                out.insert(p);
            }
        }
    }
    out
}

/// A relay's container name for `port` on `network` (`roll-apps-net-…`).
pub fn relay_name(network: &str, port: u16) -> String {
    format!("{NAME_PREFIX}{}-{port}", network.strip_prefix(super::PREFIX).unwrap_or(network).trim_start_matches("net-"))
}

/// `docker run` arguments for a relay: on `network` as `roll-link-<port>`, the socket's
/// directory mounted, nothing else (no capabilities but binding a low port, a read-only root,
/// 32 MiB, 64 processes), kept running across a daemon's restart.
pub fn relay_args(network: &str, port: u16, socket_dir: &Path) -> Vec<String> {
    let name = relay_name(network, port);
    [
        "run", "-d", "--name", &name, "--network", network, "--network-alias", &format!("{ALIAS}{port}"),
        "--label", &format!("{}=links", super::LABEL), "--label", &format!("{}.role=link", super::LABEL),
        "--read-only", "--cap-drop", "ALL", "--cap-add", "NET_BIND_SERVICE", "--security-opt", "no-new-privileges",
        "--memory", "32m", "--pids-limit", "64", "--restart", "unless-stopped",
        "-v", &format!("{}:/link", socket_dir.display()),
        IMAGE, &format!("TCP-LISTEN:{port},fork,reuseaddr"), "UNIX-CONNECT:/link/s.sock",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// `url` with loopback `host:port`s as relays', and what it found.
fn rewrite(url: &str) -> (String, Vec<(String, u16)>) {
    let mut out = url.to_string();
    let mut found = Vec::new();
    for host in ["127.0.0.1", "localhost", "[::1]"] {
        for sep in ["@", "//"] {
            let needle = format!("{sep}{host}:");
            let mut from = 0;
            while let Some(i) = out[from..].find(&needle).map(|i| i + from) {
                let after = i + needle.len();
                let digits: String = out[after..].chars().take_while(char::is_ascii_digit).collect();
                match digits.parse::<u16>() {
                    Ok(port) => {
                        let with = format!("{sep}{ALIAS}{port}:");
                        out.replace_range(i..after, &with);
                        found.push((host.to_string(), port));
                        from = i + with.len();
                    }
                    Err(_) => from = after,
                }
            }
        }
    }
    (out, found)
}

#[cfg(test)]
mod tests {
    #[test]
    fn loopback_ports_become_relays_and_nothing_else_moves() {
        let (out, found) = super::rewrite("postgres://roll:pw@127.0.0.1:55432/main?options=endpoint%3Dep_1&sslmode=disable");
        assert_eq!(out, "postgres://roll:pw@roll-link-55432:55432/main?options=endpoint%3Dep_1&sslmode=disable");
        assert_eq!(found, [("127.0.0.1".to_string(), 55432)]);
        assert_eq!(super::rewrite("redis://localhost:6379").0, "redis://roll-link-6379:6379");
        assert_eq!(super::rewrite("https://db.roll.sh:5432/x").0, "https://db.roll.sh:5432/x");
        assert_eq!(super::ports(&[("A".into(), out)]).into_iter().collect::<Vec<_>>(), [55432]);
        assert_eq!(super::relay_name("roll-apps-net-cespwc-production", 55432), "roll-apps-link-cespwc-production-55432");
    }
}
