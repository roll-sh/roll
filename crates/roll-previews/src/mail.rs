//! A mail catcher for previews (review apps' MailHog): a preview runs on a copy of production
//! data, so its services send mail here, never to the people in that data. Each preview signs in
//! with credentials of its own (`SMTP_USER` is its id, the password a keyed hash only roll makes),
//! so a mail is kept with the preview that sent it, and nothing is relayed anywhere.
//!
//! Only what's needed to accept a message: `EHLO`/`HELO`, `AUTH PLAIN` and `AUTH LOGIN`, `MAIL`,
//! `RCPT`, `DATA` (dot-unstuffed), `RSET`, `NOOP` and `QUIT`, every line and message bounded. No
//! TLS: the listener is the node's own, reached from the preview's containers.

use base64::Engine as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// The longest command line (RFC 5321's 512, with room).
pub const MAX_LINE: usize = 1000;
/// The largest message kept.
pub const MAX_MESSAGE: usize = 1 << 20;
pub const MAX_RECIPIENTS: usize = 50;
const IDLE: Duration = Duration::from_secs(60);

/// A caught message.
#[derive(Clone, Debug, PartialEq)]
pub struct Mail {
    pub from: String,
    pub to: Vec<String>,
    /// The message as sent (headers and body), at most [`MAX_MESSAGE`].
    pub raw: Vec<u8>,
}

impl Mail {
    /// Its `Subject:` header, unfolded, one printable line.
    pub fn subject(&self) -> Option<String> {
        let head = self.raw.split(|b| *b == b'\n').take_while(|l| !l.iter().all(|b| *b == b'\r'));
        let mut out: Option<String> = None;
        for line in head {
            let text = String::from_utf8_lossy(line).trim_end_matches('\r').to_string();
            match (&mut out, text.chars().next()) {
                (Some(s), Some(' ' | '\t')) => s.push_str(&format!(" {}", text.trim())),
                (Some(_), _) => break,
                (None, _) => {
                    if text.len() > 8 && text[..8].eq_ignore_ascii_case("subject:") {
                        out = Some(text[8..].trim().to_string());
                    }
                }
            }
        }
        out.map(|s| s.chars().map(|c| if c.is_control() { ' ' } else { c }).take(200).collect())
    }
}

/// Who may send, and where a message goes.
#[async_trait::async_trait]
pub trait Mailbox: Send + Sync {
    /// The preview these credentials are, if they're one's.
    async fn authorize(&self, user: &str, password: &str) -> Option<String>;
    async fn keep(&self, preview: &str, mail: Mail);
}

/// The store as the catcher's mailbox (its reads and writes off the async threads).
pub struct Store(pub Arc<crate::store::Store>);

#[async_trait::async_trait]
impl Mailbox for Store {
    async fn authorize(&self, user: &str, password: &str) -> Option<String> {
        let (s, u, p) = (self.0.clone(), user.to_string(), password.to_string());
        let ok = tokio::task::spawn_blocking(move || s.mail_signs_in(&u, &p).unwrap_or(false)).await.unwrap_or(false);
        ok.then(|| user.to_string())
    }
    async fn keep(&self, preview: &str, mail: Mail) {
        let (s, p) = (self.0.clone(), preview.to_string());
        match tokio::task::spawn_blocking(move || s.keep_mail(&p, &mail)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(preview, error = %e, "previews: keeping a caught mail"),
            Err(e) => tracing::warn!(preview, error = %e, "previews: keeping a caught mail"),
        }
    }
}

/// Serve SMTP on `listener` until it fails (each connection a task of its own).
pub async fn serve(listener: tokio::net::TcpListener, mailbox: Arc<dyn Mailbox>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(64));
    loop {
        let Ok((sock, _)) = listener.accept().await else { continue };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            drop(sock);
            continue;
        };
        let mailbox = mailbox.clone();
        tokio::spawn(async move {
            let (r, w) = sock.into_split();
            let _ = session(r, w, &*mailbox).await;
            drop(permit);
        });
    }
}

enum Line {
    Text(String),
    TooLong,
    Closed,
}

async fn read_line<R: AsyncRead + Unpin>(r: &mut BufReader<R>, max: usize) -> std::io::Result<Line> {
    let mut buf = Vec::new();
    let mut over = false;
    loop {
        let chunk = tokio::time::timeout(IDLE, r.fill_buf()).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
        if chunk.is_empty() {
            return Ok(match (over, buf.is_empty()) {
                (true, _) => Line::TooLong,
                (false, true) => Line::Closed,
                (false, false) => Line::Text(String::from_utf8_lossy(&buf).into_owned()),
            });
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        // Past the limit, the rest of the line is read and dropped.
        if !over {
            if buf.len() + take <= max {
                buf.extend_from_slice(&chunk[..take]);
            } else {
                over = true;
                buf.clear();
            }
        }
        r.consume(if done { take + 1 } else { take });
        if done {
            if over {
                return Ok(Line::TooLong);
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Line::Text(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
}

/// An address out of `FROM:<a@b>` / `TO:<a@b>` (bounded, no spaces or brackets inside).
fn address(arg: &str, prefix: &str) -> Option<String> {
    let rest = arg.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| arg[prefix.len()..].trim_start())?;
    let inner = rest.strip_prefix('<')?.split('>').next()?;
    (inner.len() <= 254 && !inner.contains([' ', '<', '\r', '\n'])).then(|| inner.to_string())
}

fn decode(b64: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    String::from_utf8(bytes).ok()
}

/// One SMTP conversation.
pub async fn session<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(r: R, mut w: W, mailbox: &dyn Mailbox) -> std::io::Result<()> {
    let mut r = BufReader::new(r);
    let say = async |w: &mut W, s: &str| -> std::io::Result<()> {
        w.write_all(s.as_bytes()).await?;
        w.write_all(b"\r\n").await?;
        w.flush().await
    };
    say(&mut w, "220 roll previews mail catcher (nothing is relayed)").await?;
    let mut preview: Option<String> = None;
    let mut from: Option<String> = None;
    let mut to: Vec<String> = Vec::new();
    loop {
        let line = match read_line(&mut r, MAX_LINE).await? {
            Line::Closed => return Ok(()),
            Line::TooLong => {
                say(&mut w, "500 line too long").await?;
                continue;
            }
            Line::Text(t) => t,
        };
        let (verb, arg) = line.split_once(' ').map_or((line.as_str(), ""), |(v, a)| (v, a.trim()));
        match verb.to_ascii_uppercase().as_str() {
            "EHLO" => say(&mut w, &format!("250-roll previews\r\n250-AUTH PLAIN LOGIN\r\n250-SIZE {MAX_MESSAGE}\r\n250 8BITMIME")).await?,
            "HELO" => say(&mut w, "250 roll previews").await?,
            "AUTH" => {
                let (mech, initial) = arg.split_once(' ').map_or((arg, None), |(m, i)| (m, Some(i)));
                let creds = match mech.to_ascii_uppercase().as_str() {
                    "PLAIN" => {
                        let b64 = match initial {
                            Some(i) => i.to_string(),
                            None => {
                                say(&mut w, "334 ").await?;
                                match read_line(&mut r, MAX_LINE).await? {
                                    Line::Text(t) => t,
                                    _ => return Ok(()),
                                }
                            }
                        };
                        // `authzid \0 user \0 password`
                        decode(&b64).and_then(|s| {
                            let mut parts = s.splitn(3, '\0');
                            let (_, user, pass) = (parts.next()?, parts.next()?, parts.next()?);
                            Some((user.to_string(), pass.to_string()))
                        })
                    }
                    "LOGIN" => {
                        let user = match initial {
                            Some(i) => decode(i),
                            None => {
                                say(&mut w, "334 VXNlcm5hbWU6").await?;
                                match read_line(&mut r, MAX_LINE).await? {
                                    Line::Text(t) => decode(&t),
                                    _ => return Ok(()),
                                }
                            }
                        };
                        say(&mut w, "334 UGFzc3dvcmQ6").await?;
                        let pass = match read_line(&mut r, MAX_LINE).await? {
                            Line::Text(t) => decode(&t),
                            _ => return Ok(()),
                        };
                        user.zip(pass)
                    }
                    _ => {
                        say(&mut w, "504 PLAIN or LOGIN").await?;
                        continue;
                    }
                };
                let who = match creds {
                    Some((u, p)) => mailbox.authorize(&u, &p).await,
                    None => None,
                };
                match who {
                    Some(p) => {
                        preview = Some(p);
                        say(&mut w, "235 signed in").await?;
                    }
                    None => say(&mut w, "535 those aren't a preview's SMTP credentials").await?,
                }
            }
            "MAIL" => match (&preview, address(arg, "FROM:")) {
                (None, _) => say(&mut w, "530 sign in first (SMTP_USER and SMTP_PASSWORD)").await?,
                (_, None) => say(&mut w, "501 MAIL FROM:<address>").await?,
                (Some(_), Some(a)) => {
                    from = Some(a);
                    to.clear();
                    say(&mut w, "250 ok").await?;
                }
            },
            "RCPT" => match (&from, address(arg, "TO:")) {
                (None, _) => say(&mut w, "503 MAIL first").await?,
                (_, None) => say(&mut w, "501 RCPT TO:<address>").await?,
                (Some(_), Some(_)) if to.len() >= MAX_RECIPIENTS => say(&mut w, "452 too many recipients").await?,
                (Some(_), Some(a)) => {
                    to.push(a);
                    say(&mut w, "250 ok").await?;
                }
            },
            "DATA" => {
                if from.is_none() || to.is_empty() {
                    say(&mut w, "503 MAIL and RCPT first").await?;
                    continue;
                }
                say(&mut w, "354 end with <CRLF>.<CRLF>").await?;
                let mut raw: Vec<u8> = Vec::new();
                let mut over = false;
                loop {
                    let line = match read_line(&mut r, MAX_LINE).await? {
                        Line::Closed => return Ok(()),
                        Line::TooLong => {
                            over = true;
                            continue;
                        }
                        Line::Text(t) => t,
                    };
                    if line == "." {
                        break;
                    }
                    let line = line.strip_prefix('.').unwrap_or(&line);
                    if raw.len() + line.len() + 2 > MAX_MESSAGE {
                        over = true;
                    }
                    if !over {
                        raw.extend_from_slice(line.as_bytes());
                        raw.extend_from_slice(b"\r\n");
                    }
                }
                if over {
                    say(&mut w, "552 message too big (1 MiB) or a line too long").await?;
                } else if let (Some(p), Some(f)) = (&preview, from.take()) {
                    mailbox.keep(p, Mail { from: f, to: std::mem::take(&mut to), raw }).await;
                    say(&mut w, "250 caught (not delivered)").await?;
                }
                from = None;
                to.clear();
            }
            "RSET" => {
                from = None;
                to.clear();
                say(&mut w, "250 ok").await?;
            }
            "NOOP" => say(&mut w, "250 ok").await?,
            "QUIT" => {
                say(&mut w, "221 bye").await?;
                return Ok(());
            }
            _ => say(&mut w, "502 not here").await?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct Box_(Mutex<Vec<(String, Mail)>>);
    #[async_trait::async_trait]
    impl Mailbox for Box_ {
        async fn authorize(&self, user: &str, password: &str) -> Option<String> {
            (user == "pv_1" && password == "secret").then(|| user.to_string())
        }
        async fn keep(&self, preview: &str, mail: Mail) {
            self.0.lock().push((preview.to_string(), mail));
        }
    }

    async fn talk(script: &str) -> (String, Vec<(String, Mail)>) {
        let b = Box_::default();
        let mut out = Vec::new();
        session(script.as_bytes(), &mut out, &b).await.unwrap();
        (String::from_utf8(out).unwrap(), b.0.into_inner())
    }

    /// A preview's service signs in with its own credentials and its mail is kept with it, sent
    /// nowhere; without them nothing is accepted.
    #[tokio::test]
    async fn a_preview_s_mail_is_caught_and_kept_with_it() {
        let plain = base64::engine::general_purpose::STANDARD.encode("\0pv_1\0secret");
        let (said, kept) = talk(&format!(
            "EHLO app\r\nAUTH PLAIN {plain}\r\nMAIL FROM:<shop@example.com>\r\nRCPT TO:<ada@corp.example>\r\nDATA\r\nSubject: Your\r\n order\r\n\r\n..dotted\r\nbody\r\n.\r\nQUIT\r\n"
        ))
        .await;
        assert!(said.contains("235 ") && said.contains("250 caught"), "{said}");
        let (preview, mail) = &kept[0];
        assert_eq!((preview.as_str(), mail.from.as_str(), mail.to.as_slice()), ("pv_1", "shop@example.com", &["ada@corp.example".to_string()][..]));
        assert_eq!(mail.subject().as_deref(), Some("Your order"));
        assert!(String::from_utf8_lossy(&mail.raw).contains("\r\n.dotted\r\n"), "dot-unstuffed");
        // AUTH LOGIN works too; a wrong password, or none, can't send.
        let b = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let (said, kept) = talk(&format!("EHLO app\r\nAUTH LOGIN\r\n{}\r\n{}\r\nMAIL FROM:<a@b>\r\n", b("pv_1"), b("secret"))).await;
        assert!(said.contains("235 ") && said.ends_with("250 ok\r\n") && kept.is_empty(), "{said}");
        let (said, kept) = talk(&format!("AUTH PLAIN {}\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\nx\r\n.\r\n", b("\0pv_1\0guess"))).await;
        assert!(said.contains("535 ") && said.contains("530 ") && kept.is_empty(), "{said}");
        // Bounded: a line past the limit, a message past 1 MiB.
        let long = "x".repeat(MAX_LINE + 10);
        let (said, _) = talk(&format!("{long}\r\nNOOP\r\n")).await;
        assert!(said.contains("500 line too long") && said.ends_with("250 ok\r\n"), "{said}");
        let big: String = std::iter::repeat_n("y".repeat(900) + "\r\n", MAX_MESSAGE / 900 + 2).collect();
        let (said, kept) = talk(&format!("AUTH PLAIN {plain}\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\n{big}.\r\n")).await;
        assert!(said.contains("552 ") && kept.is_empty(), "{}", &said[said.len().saturating_sub(200)..]);
    }
}
