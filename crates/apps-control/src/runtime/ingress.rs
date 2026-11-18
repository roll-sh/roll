//! Reaching a closed instance from rolld (docs/apps/README.md, "Egress"). A closed environment's
//! network has no address of the host's (Docker 28's isolated gateway), so its code reaches
//! none of the host's services, and rolld can't reach its instances at their addresses there
//! either. Instead, a closed web instance's network sandbox runs `socat` (the links' relay
//! image): it listens on a unix socket in a directory bind-mounted from
//! `<home>/apps/ingress/<digest>/` (the sandbox's name hashed: a unix socket's path is at most
//! 107 bytes, and a preview environment's sandbox name alone is near 60) and joins each caller to the app's port inside the
//! instance's own network namespace (its waker's while it sleeps). rolld answers on a loopback
//! port of its own for each, joining callers to that socket, so the proxy reaches a closed
//! instance at `127.0.0.1` as it does any other.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// The socket's name in its directory (`/in/s.sock` in the sandbox).
pub const SOCKET: &str = "s.sock";
/// Where the directory is mounted in the sandbox.
pub const MOUNT: &str = "/in";

pub struct Ingress {
    dir: PathBuf,
    /// Each sandbox's loopback listener, and the task that serves it.
    serving: Mutex<HashMap<String, (SocketAddr, tokio::task::AbortHandle)>>,
}

impl Ingress {
    pub fn new(dir: PathBuf) -> Ingress {
        Ingress { dir, serving: Mutex::new(HashMap::new()) }
    }

    /// The sandbox's socket directory: named by a digest of its name, so the socket's path stays
    /// short whatever the sandbox is called.
    fn socket_dir(&self, sandbox: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(sandbox.as_bytes());
        self.dir.join(h.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>())
    }

    /// The directory a new sandbox's socket goes in (made `0700`, emptied of an earlier one's),
    /// and the owner the sandbox runs as so its `socat` can make the socket there.
    pub fn prepare(&self, sandbox: &str) -> std::io::Result<(PathBuf, u32, u32)> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let d = self.socket_dir(sandbox);
        // What rolld connects to (`sockaddr_un` holds 108 bytes with its NUL).
        let path = d.join(SOCKET);
        if path.as_os_str().len() > 107 {
            return Err(std::io::Error::other(format!("{} is too long for a unix socket (107 bytes at most): a shorter ROLL_HOME", path.display())));
        }
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&d)?;
        let _ = std::fs::remove_file(d.join(SOCKET));
        let m = std::fs::metadata(&d)?;
        Ok((d, m.uid(), m.gid()))
    }

    /// Whether `sandbox` is reached through a socket (made by [`Ingress::prepare`]).
    pub fn has(&self, sandbox: &str) -> bool {
        self.socket_dir(sandbox).is_dir()
    }

    /// The loopback address that reaches `sandbox`'s app: listening already, or from now on.
    pub async fn serve(&self, sandbox: &str) -> anyhow::Result<SocketAddr> {
        if let Some((a, _)) = self.serving.lock().get(sandbox) {
            return Ok(*a);
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let socket = self.socket_dir(sandbox).join(SOCKET);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut caller, _)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                };
                let socket = socket.clone();
                tokio::spawn(async move {
                    // The sandbox's socat makes its socket as it starts: a caller right after a
                    // start waits for it a little.
                    for attempt in 0..100u32 {
                        match tokio::net::UnixStream::connect(&socket).await {
                            Ok(mut up) => {
                                let _ = tokio::io::copy_bidirectional(&mut caller, &mut up).await;
                                return;
                            }
                            Err(_) if attempt < 99 => tokio::time::sleep(Duration::from_millis(20)).await,
                            Err(_) => return,
                        }
                    }
                });
            }
        });
        let mut serving = self.serving.lock();
        // Another call started one meanwhile: that one is kept.
        if let Some((a, _)) = serving.get(sandbox) {
            task.abort();
            return Ok(*a);
        }
        serving.insert(sandbox.to_string(), (addr, task.abort_handle()));
        Ok(addr)
    }

    /// The sandbox went: its listener stops and its directory goes.
    pub fn forget(&self, sandbox: &str) {
        if let Some((_, task)) = self.serving.lock().remove(sandbox) {
            task.abort();
        }
        let _ = std::fs::remove_dir_all(self.socket_dir(sandbox));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn a_loopback_caller_reaches_the_sandboxs_socket_even_before_it_is_made() {
        let home = tempfile::tempdir().unwrap();
        let ing = Ingress::new(home.path().join("ingress"));
        // A preview environment's sandbox: its name doesn't make the socket's path too long.
        let sb = "roll-apps-2jzm7a5uaaec-pv-zfq567oulris-dcskls-1-net";
        let (dir, _, _) = ing.prepare(sb).unwrap();
        assert!(dir.join(SOCKET).as_os_str().len() < home.path().as_os_str().len() + 40, "{}", dir.display());
        assert!(ing.has(sb) && !ing.has("other"));
        let addr = ing.serve(sb).await.unwrap();
        assert_eq!(ing.serve(sb).await.unwrap(), addr, "one listener a sandbox");
        let mut caller = tokio::net::TcpStream::connect(addr).await.unwrap();
        caller.write_all(b"ping").await.unwrap();
        // The socket comes after the caller (a sandbox still starting).
        tokio::time::sleep(Duration::from_millis(60)).await;
        let server = tokio::net::UnixListener::bind(dir.join(SOCKET)).unwrap();
        let (mut s, _) = server.accept().await.unwrap();
        let mut got = [0u8; 4];
        s.read_exact(&mut got).await.unwrap();
        s.write_all(b"pong").await.unwrap();
        caller.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"pong");
        ing.forget(sb);
        assert!(!ing.has(sb));
        // A home too long for any socket is said, not a health check that never passes.
        let long = Ingress::new(home.path().join("x".repeat(120)));
        assert!(long.prepare("sb").unwrap_err().to_string().contains("too long"));
    }
}
