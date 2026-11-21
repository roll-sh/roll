//! A compute image's [`Answers`], taken from its own Postgres: every way a prober can poke a
//! login before the password is checked ([`probes`]) is sent to a running compute as a role
//! that doesn't exist, and what comes back is kept only if the decoy, answering from it, says
//! exactly the same to every probe. The same probes and comparison are what
//! `pg-control/tests/e2e.rs` runs through the proxy.

use super::{Answers, Decoy, Refusal, Site, refuse};
use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// What a prober might send after the server asks for SCRAM.
#[derive(Clone, Copy, Debug)]
pub enum Probe {
    /// A whole exchange with a wrong password (flag `n` or `y`).
    WrongPassword(u8),
    /// A client-first-message Postgres refuses.
    First(&'static [u8]),
    /// A client-final-message (from the server's nonce `r`) Postgres refuses.
    Final(fn(&str) -> String),
    /// A first SASL message with this body, which Postgres can't read.
    Initial(&'static [u8]),
    Mechanism,
    NotSasl,
    Oversized,
    /// No initial response: the server asks again with an empty challenge.
    NoInitial,
}

/// A probe: the protocol version, extra startup parameters, and what's sent after.
pub type Poke = (i32, Vec<(&'static str, &'static str)>, Probe);

/// A proof of the right length that's wrong: base64 of 32 zero bytes.
const WRONG_PROOF: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const CLIENT_NONCE: &str = "fakeclientnonce0000000000";
/// The decoy's salt while it's compared with a compute (only its shape is).
const PROBE_SALT: &str = "AAAAAAAAAAAAAAAAAAAAAA==";
/// The longest message a probe expects back (the longest Postgres sends here is ~250 bytes).
const MAX_MESSAGE: usize = 8192;
/// How long one probe may take against a compute.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Every way a prober can poke a login, besides the password itself: each error the decoy can
/// raise is reached by at least one.
pub fn probes() -> Vec<Poke> {
    let v3 = 3 << 16;
    let plain = |p| (v3, vec![], p);
    vec![
        plain(Probe::WrongPassword(b'n')),
        plain(Probe::WrongPassword(b'y')),
        plain(Probe::NoInitial),
        plain(Probe::Mechanism),
        plain(Probe::NotSasl),
        plain(Probe::Oversized),
        plain(Probe::Initial(b"SCRAM-SHA-256")),
        plain(Probe::Initial(b"SCRAM-SHA-256\0\0\0")),
        plain(Probe::Initial(b"SCRAM-SHA-256\0\0\0\0\x64abc")),
        plain(Probe::Initial(b"SCRAM-SHA-256\0\0\0\0\x03abcde")),
        plain(Probe::First(b"")),
        plain(Probe::First(b"n,,n=,r=a\0b")),
        plain(Probe::First(b"garbage")),
        plain(Probe::First(b"nx")),
        plain(Probe::First(b"y")),
        plain(Probe::First(b"p=tls-server-end-point,,n=,r=abc")),
        plain(Probe::First(b"n,a=someone,n=,r=abc")),
        plain(Probe::First(b"n,x")),
        plain(Probe::First(b"n,,m=ext,n=,r=abc")),
        plain(Probe::First(b"n,,r=abc")),
        plain(Probe::First(b"n,,n")),
        plain(Probe::First(b"n,,n=,r=ab\x7fc")),
        plain(Probe::First(b"n,,n=,r=abc,1=x")),
        plain(Probe::First(b"n,,n=,r=abc,x")),
        plain(Probe::Final(|r| format!("c=eSws,r={r},p={WRONG_PROOF}"))),
        plain(Probe::Final(|r| format!("c=biws,r={r}x,p={WRONG_PROOF}"))),
        plain(Probe::Final(|r| format!("c=biws,r={r},p=AAAA"))),
        plain(Probe::Final(|r| format!("c=biws,r={r},p={WRONG_PROOF},x=1"))),
        plain(Probe::Final(|r| format!("c=biws,r={r}"))),
        plain(Probe::Final(|r| format!("d=biws,r={r}"))),
        (v3 | 2, vec![("_pq_.fake_option", "1")], Probe::WrongPassword(b'n')),
        (v3 | 9, vec![], Probe::WrongPassword(b'n')),
        (v3, vec![("replication", "maybe")], Probe::WrongPassword(b'n')),
        (v3, vec![("replication", "database")], Probe::WrongPassword(b'n')),
    ]
}

/// A startup packet for `user` with the probe's parameters (what the proxy passes a compute, so
/// without `options`).
pub fn startup(version: i32, user: &str, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut body = version.to_be_bytes().to_vec();
    for (k, v) in params(user, extra) {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut packet = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    packet.extend_from_slice(&body);
    packet
}

fn params(user: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut p = vec![("user".to_string(), user.to_string()), ("database".to_string(), "main".to_string())];
    p.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    p
}

/// Everything the server says on `s` for `probe` (its startup packet sent already), until it
/// hangs up.
pub async fn transcript<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, probe: Probe) -> Vec<Vec<u8>> {
    let mut got = vec![];
    loop {
        let Some(m) = read(s).await else { return got };
        let tag = m[0];
        got.push(m);
        if tag == b'R' {
            break;
        }
        if tag == b'E' {
            return got;
        }
    }
    let first = |flag: u8| format!("{},,n=,r={CLIENT_NONCE}", flag as char).into_bytes();
    match probe {
        // Only what Postgres reads before it gives up: bytes it never reads would reset the
        // connection under its answer.
        Probe::NotSasl => send(s, b"X").await,
        Probe::Mechanism => send(s, &sasl_initial("PLAIN", Some(&first(b'n')))).await,
        Probe::First(f) => send(s, &sasl_initial("SCRAM-SHA-256", Some(f))).await,
        Probe::Initial(body) => send(s, &sasl(body)).await,
        Probe::Oversized => send(s, &[&b"p"[..], &2000i32.to_be_bytes()].concat()).await,
        Probe::NoInitial | Probe::WrongPassword(_) | Probe::Final(_) => {
            let flag = if let Probe::WrongPassword(f) = probe { f } else { b'n' };
            if let Probe::NoInitial = probe {
                send(s, &sasl_initial("SCRAM-SHA-256", None)).await;
                let Some(m) = read(s).await else { return got };
                got.push(m);
                send(s, &sasl(&first(flag))).await;
            } else {
                send(s, &sasl_initial("SCRAM-SHA-256", Some(&first(flag)))).await;
            }
            let Some(m) = read(s).await else { return got };
            let text = String::from_utf8_lossy(m.get(9..).unwrap_or_default()).to_string();
            got.push(m);
            if let Some(r) = text.strip_prefix("r=").and_then(|t| t.split(',').next()) {
                let binding = if flag == b'y' { "eSws" } else { "biws" };
                let last = match probe {
                    Probe::Final(f) => f(r),
                    _ => format!("c={binding},r={r},p={WRONG_PROOF}"),
                };
                send(s, &sasl(last.as_bytes())).await;
            }
        }
    }
    while let Some(m) = read(s).await {
        got.push(m);
    }
    got
}

/// One whole message, or `None` once the server hangs up (or sends what no login answer is).
async fn read<S: AsyncRead + Unpin>(s: &mut S) -> Option<Vec<u8>> {
    let mut head = [0u8; 5];
    s.read_exact(&mut head).await.ok()?;
    let len = usize::try_from(i32::from_be_bytes(head[1..].try_into().unwrap())).ok()?;
    if !(4..=MAX_MESSAGE).contains(&len) {
        return None;
    }
    let mut m = head.to_vec();
    m.resize(len + 1, 0);
    s.read_exact(&mut m[5..]).await.ok()?;
    Some(m)
}

async fn send<S: AsyncWrite + Unpin>(s: &mut S, bytes: &[u8]) {
    let _ = s.write_all(bytes).await;
}

/// A `p` message: the SASL mechanism and initial response (`None`: no response, length -1).
fn sasl_initial(mech: &str, data: Option<&[u8]>) -> Vec<u8> {
    let mut body = mech.as_bytes().to_vec();
    body.push(0);
    match data {
        Some(d) => {
            body.extend_from_slice(&(d.len() as i32).to_be_bytes());
            body.extend_from_slice(d);
        }
        None => body.extend_from_slice(&(-1i32).to_be_bytes()),
    }
    sasl(&body)
}

fn sasl(body: &[u8]) -> Vec<u8> {
    let mut m = vec![b'p'];
    m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    m.extend_from_slice(body);
    m
}

/// A transcript as comparable text: the server's random nonce and the salt (each role has its
/// own) taken out when they have Postgres' shape, and the role's name.
pub fn shape(t: &[Vec<u8>], user: &str) -> Vec<String> {
    t.iter()
        .map(|m| {
            if m.len() > 9 && m[0] == b'R' && m[5..9] == 11i32.to_be_bytes() {
                let text = String::from_utf8_lossy(&m[9..]).to_string();
                let parts: Vec<&str> = text.split(',').collect();
                if let [r, s, "i=4096"] = parts[..]
                    && r.strip_prefix("r=").and_then(|r| r.strip_prefix(CLIENT_NONCE)).is_some_and(|n| n.len() == 24)
                    && s.strip_prefix("s=").is_some_and(|s| s.len() == 24)
                {
                    return "R11 r=<client><24> s=<16 bytes> i=4096".to_string();
                }
            }
            String::from_utf8_lossy(m).replace(user, "<role>")
        })
        .collect()
}

/// The compute's answers at `addr` (a running Postgres), or `None` when this decoy can't say
/// what it says to every probe. An error: the compute couldn't be asked.
pub async fn learn(addr: SocketAddr) -> io::Result<Option<Answers>> {
    let mut raw = [0u8; 8];
    getrandom::fill(&mut raw).expect("os randomness");
    let user = format!("roll_probe_{:016x}", u64::from_be_bytes(raw));
    let mut theirs = vec![];
    for (version, extra, probe) in probes() {
        let ask = async {
            let mut s = tokio::net::TcpStream::connect(addr).await?;
            s.write_all(&startup(version, &user, &extra)).await?;
            Ok::<_, io::Error>(transcript(&mut s, probe).await)
        };
        let t = tokio::time::timeout(PROBE_TIMEOUT, ask).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "a probe timed out"))??;
        theirs.push(t);
    }
    Ok(from_transcripts(&user, &theirs).await)
}

/// The answers that make this decoy say what `theirs` says (a compute's transcripts of
/// [`probes`], in order, for `user`), if any do.
pub async fn from_transcripts(user: &str, theirs: &[Vec<Vec<u8>>]) -> Option<Answers> {
    let pokes = probes();
    if theirs.len() != pokes.len() {
        return None;
    }
    let at = |want: &dyn Fn(&Poke) -> bool| pokes.iter().position(want).map(|i| &theirs[i]);
    // Asked for 3.9 with nothing else, Postgres offers its newest.
    let newest = at(&|(v, extra, _)| v & 0xffff == 9 && extra.is_empty())?.first()?;
    if newest[0] != b'v' || newest.len() < 9 {
        return None;
    }
    let latest_minor = i32::from_be_bytes(newest[5..9].try_into().ok()?) & 0xffff;
    let wrong = at(&|(v, extra, p)| *v == 3 << 16 && extra.is_empty() && matches!(p, Probe::WrongPassword(b'n')))?.last()?;
    let refusal = Refusal::learn(wrong.get(5..)?, user)?;

    // Where each error is raised: the decoy answers with a marker after its own fields, and the
    // compute's fields must start with the decoy's; what follows is that error's tail.
    let markers = Answers {
        latest_minor,
        tails: Site::ALL.iter().enumerate().map(|(i, s)| (*s, format!("\x01{i}\0\0").into_bytes())).collect(),
        refusal: refusal.clone(),
    };
    let mut tails = BTreeMap::new();
    for (poke, t) in pokes.iter().zip(theirs) {
        let ours = answer(&markers, user, poke).await;
        let (Some(o), Some(c)) = (ours.last(), t.last()) else { continue };
        let Some((site, head)) = marked(o) else { continue };
        let tail = c.get(5..).filter(|_| c[0] == b'E')?.strip_prefix(head)?;
        if tails.insert(site, tail.to_vec()).is_some_and(|before| before != tail) {
            return None;
        }
    }
    let answers = Answers { latest_minor, tails, refusal };
    if !answers.complete() {
        return None;
    }
    for (poke, t) in pokes.iter().zip(theirs) {
        if shape(&answer(&answers, user, poke).await, user) != shape(t, user) {
            return None;
        }
    }
    Some(answers)
}

/// The decoy's error in `m` with a marker tail: which error, and the fields before the marker.
fn marked(m: &[u8]) -> Option<(Site, &[u8])> {
    if m.first() != Some(&b'E') {
        return None;
    }
    let fields = m.get(5..)?;
    let mut at = 0;
    while let Some(&tag) = fields.get(at).filter(|&&t| t != 0) {
        let end = at + 1 + fields[at + 1..].iter().position(|&b| b == 0)?;
        if tag == 1 {
            let i: usize = std::str::from_utf8(&fields[at + 1..end]).ok()?.parse().ok()?;
            return Some((*Site::ALL.get(i)?, &fields[..at]));
        }
        at = end + 1;
    }
    None
}

/// What the decoy says to `poke` from `answers`.
pub async fn answer(answers: &Answers, user: &str, (version, extra, probe): &Poke) -> Vec<Vec<u8>> {
    let (mut client, server) = tokio::io::duplex(MAX_MESSAGE);
    let params = params(user, extra);
    let decoy = Decoy { salt: PROBE_SALT.into(), start: Duration::ZERO, answers: None };
    let serve = async move {
        let mut server = server;
        let _ = refuse(&mut server, *version, &params, user, &decoy, Duration::ZERO, answers).await;
    };
    tokio::join!(serve, transcript(&mut client, *probe)).1
}
