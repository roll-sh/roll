//! A connection's first words: which database it is for, read before anything serves it, so a
//! node can hand the connection to the database's owner (roll cloud's edge). Without a TLS server
//! name, the database is named by the connection's first AUTH (or HELLO … AUTH); commands before
//! it are answered as this server answers a connection that hasn't authenticated (nothing a client
//! can do there changes the connection's state), so the owner gets the stream from the AUTH on.

use crate::directory::AuthRequest;
use bytes::BytesMut;
use redis_engine::commands::eq;
use redis_proto::Limits;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// What [`read_auth`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum FirstAuth {
    /// The database the connection's AUTH names, and the bytes read from that command on (they
    /// go first to whoever serves it).
    Names { db: String, read: BytesMut },
    /// Nothing named a database (bad input, a stream that went quiet): serve it here with what
    /// was read, and let the server answer it.
    Unknown { read: BytesMut },
    /// The client left (or said QUIT, answered).
    Closed,
}

const NOAUTH: &[u8] = b"-NOAUTH Authentication required.\r\n";
const NOAUTH_HELLO: &[u8] = b"-NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time\r\n";

/// The credentials an AUTH or HELLO … AUTH command carries.
fn credentials(args: &[bytes::Bytes]) -> Option<(Option<&[u8]>, &[u8])> {
    if eq(&args[0], "auth") {
        return match args.len() {
            2 => Some((None, &args[1])),
            3 => Some((Some(&args[1]), &args[2])),
            _ => None,
        };
    }
    if eq(&args[0], "hello") {
        let i = args.iter().skip(2).position(|a| eq(a, "auth"))? + 2;
        if args.len() > i + 2 {
            return Some((Some(&args[i + 1]), &args[i + 2]));
        }
    }
    None
}

/// Read commands until the first AUTH (or HELLO … AUTH) and tell which database it names (as
/// [`crate::node::route`] reads it, with `domain` the hostnames' suffix). Gives up after
/// `timeout` without one.
pub async fn read_auth<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, domain: Option<&str>, timeout: Duration) -> FirstAuth {
    let mut buf = BytesMut::with_capacity(4096);
    let limits = Limits { authenticated: false, ..Limits::default() };
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let before = buf.clone();
        match redis_proto::parse_request(&mut buf, limits) {
            Ok(Some(args)) if args.is_empty() => continue,
            Ok(Some(args)) => {
                if let Some((user, password)) = credentials(&args) {
                    let req = AuthRequest { user, password, sni: None, client: None, command: &args[0] };
                    return match crate::node::route(&req, domain) {
                        Some((db, _, _)) => FirstAuth::Names { db, read: before },
                        None => FirstAuth::Unknown { read: before },
                    };
                }
                let reply: &[u8] = if eq(&args[0], "quit") {
                    let _ = stream.write_all(b"+OK\r\n").await;
                    return FirstAuth::Closed;
                } else if eq(&args[0], "hello") {
                    NOAUTH_HELLO
                } else {
                    NOAUTH
                };
                if stream.write_all(reply).await.is_err() {
                    return FirstAuth::Closed;
                }
                continue;
            }
            Ok(None) => {}
            Err(_) => return FirstAuth::Unknown { read: before },
        }
        match tokio::time::timeout_at(deadline, stream.read_buf(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return FirstAuth::Closed,
            Ok(Ok(_)) => {}
            Err(_) => return FirstAuth::Unknown { read: buf },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn first(input: &[u8]) -> (FirstAuth, Vec<u8>) {
        let (mut client, mut server) = tokio::io::duplex(1 << 16);
        client.write_all(input).await.unwrap();
        let got = read_auth(&mut server, Some("redis.example.com"), Duration::from_millis(200)).await;
        drop(server);
        let mut answered = Vec::new();
        client.read_to_end(&mut answered).await.unwrap();
        (got, answered)
    }

    #[tokio::test]
    async fn the_first_auth_names_the_database_and_what_came_before_is_answered() {
        let (got, answered) = first(b"*1\r\n$4\r\nPING\r\n*2\r\n$4\r\nAUTH\r\n$9\r\ndb1_s3cr3\r\n*1\r\n$4\r\nPING\r\n").await;
        assert_eq!(got, FirstAuth::Names { db: "db1".into(), read: BytesMut::from(&b"*2\r\n$4\r\nAUTH\r\n$9\r\ndb1_s3cr3\r\n*1\r\n$4\r\nPING\r\n"[..]) });
        assert_eq!(answered, NOAUTH);
        let (got, _) = first(b"HELLO 3 AUTH app@db2 pw\r\n").await;
        assert!(matches!(got, FirstAuth::Names { db, .. } if db == "db2"));
        let (got, answered) = first(b"HELLO 3\r\nQUIT\r\n").await;
        assert_eq!((got, answered), (FirstAuth::Closed, [NOAUTH_HELLO, b"+OK\r\n"].concat()));
        let (got, _) = first(b"PING\r\n").await;
        assert!(matches!(got, FirstAuth::Unknown { .. }));
    }
}
