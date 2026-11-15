//! `roll run`'s masked output (docs/secrets.md, "`roll secrets` and `roll run`"), as 1Password's
//! `op run` and GitHub Actions do: what the command prints goes through roll, which writes
//! `<concealed by roll>` in place of any value it put in the command's environment, so a debug print
//! doesn't put a key in a build log.
//!
//! Values shorter than [`MIN`] aren't masked (`true`, a port). A value is matched as it is, line by
//! line, and as JSON and URL encode it. Output passes through as it comes, except for a tail that
//! could be the start of a value: that's held until more arrives or the stream ends.

use aho_corasick::{AhoCorasick, MatchKind};
use std::io::IsTerminal;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The shortest value that's masked.
pub const MIN: usize = 8;
pub const CONCEALED: &[u8] = b"<concealed by roll>";
/// After the command exits, how long roll goes on reading its output (a process it left running
/// in the background can hold the pipe open).
const AFTER_EXIT: Duration = Duration::from_secs(1);

/// Replaces values in a stream of bytes, however the stream is split into reads.
#[derive(Clone)]
pub struct Masker {
    ac: AhoCorasick,
    patterns: Vec<Vec<u8>>,
    longest: usize,
    /// The values by their first two bytes (a set of the pairs, then the values), and by their
    /// first byte: [`Masker::partial`] tries every position of a tail as long as the longest value,
    /// and most positions start no value at all.
    pairs: Vec<u64>,
    by_pair: std::collections::HashMap<u16, Vec<usize>>,
    by_first: Vec<Vec<usize>>,
    held: Vec<u8>,
}

/// What's matched for `values`: each value, each of its lines, and its JSON and URL encodings, when
/// at least [`MIN`] bytes long.
fn patterns<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![];
    let mut add = |p: &str| {
        if p.len() >= MIN && !out.iter().any(|o| o == p.as_bytes()) {
            out.push(p.as_bytes().to_vec());
        }
    };
    for v in values {
        add(v);
        // A line of a PEM key or a token list, not a line of structure (`"type": "service_account",`
        // in a JSON key file would be masked wherever it's printed).
        if v.contains('\n') {
            v.lines().map(str::trim).filter(|l| !l.contains(char::is_whitespace)).for_each(&mut add);
        }
        // A JSON value (a cloud key file, minified or not): its secret fields, and their lines
        // (the private key's, once its `\n`s are read).
        if v.trim_start().starts_with(['{', '[']) && let Ok(json) = serde_json::from_str::<serde_json::Value>(v) {
            let mut strings = vec![];
            secret_fields(&json, false, &mut strings);
            for s in strings {
                add(s);
                s.lines().map(str::trim).filter(|l| !l.contains(char::is_whitespace)).for_each(&mut add);
            }
        }
        let json = serde_json::to_string(v).expect("a string");
        add(&json[1..json.len() - 1]);
        add(&urlencoding::encode(v));
    }
    out
}

/// The strings of a JSON value under a field whose name says it's secret (`private_key`,
/// `client_secret`, `password`, `token`), at any depth; not `token_uri`, `client_id` or the type:
/// those are printed in the open and would be masked everywhere.
fn secret_fields<'a>(v: &'a serde_json::Value, secret: bool, out: &mut Vec<&'a str>) {
    match v {
        serde_json::Value::String(s) if secret => out.push(s),
        serde_json::Value::Array(a) => a.iter().for_each(|x| secret_fields(x, secret, out)),
        serde_json::Value::Object(o) => o.iter().for_each(|(k, x)| {
            let k = k.to_ascii_lowercase();
            let named = ["key", "secret", "password", "token", "credential"].iter().any(|w| k.contains(w)) && !["_uri", "_url", "_id", "_email"].iter().any(|e| k.ends_with(e));
            secret_fields(x, named, out)
        }),
        _ => {}
    }
}

impl Masker {
    /// None when no value is long enough to mask.
    pub fn new<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<Masker> {
        let patterns = patterns(values);
        let longest = patterns.iter().map(Vec::len).max()?;
        // A DFA: twice the NFA's speed over output that shares the values' alphabet.
        let ac = AhoCorasick::builder().match_kind(MatchKind::LeftmostLongest).kind(Some(aho_corasick::AhoCorasickKind::DFA)).build(&patterns).expect("patterns");
        let (mut pairs, mut by_pair, mut by_first) = (vec![0u64; 1024], std::collections::HashMap::<u16, Vec<usize>>::new(), vec![vec![]; 256]);
        for (i, p) in patterns.iter().enumerate() {
            let pair = u16::from_be_bytes([p[0], p[1]]);
            pairs[pair as usize >> 6] |= 1 << (pair & 63);
            by_pair.entry(pair).or_default().push(i);
            by_first[p[0] as usize].push(i);
        }
        Some(Masker { ac, patterns, longest, pairs, by_pair, by_first, held: vec![] })
    }

    /// Whether a tail is waiting for more.
    #[cfg(test)]
    pub fn holding(&self) -> bool {
        !self.held.is_empty()
    }

    /// Takes `chunk` and appends to `out` what can be written now.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.held.extend_from_slice(chunk);
        let keep = self.partial();
        let mut at = 0;
        for m in self.ac.find_iter(&self.held) {
            // Any match starting before `keep` is final: no longer value can start there.
            if m.start() >= keep {
                break;
            }
            out.extend_from_slice(&self.held[at..m.start()]);
            out.extend_from_slice(CONCEALED);
            at = m.end();
        }
        let to = keep.max(at);
        out.extend_from_slice(&self.held[at..to]);
        self.held.drain(..to);
    }

    /// Appends everything held, with complete values masked.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        let mut at = 0;
        for m in self.ac.find_iter(&self.held) {
            out.extend_from_slice(&self.held[at..m.start()]);
            out.extend_from_slice(CONCEALED);
            at = m.end();
        }
        out.extend_from_slice(&self.held[at..]);
        self.held.clear();
    }

    /// Where the held bytes stop being safe: the start of the longest tail that a value could
    /// still grow from (a proper prefix of one), or the end.
    fn partial(&self) -> usize {
        let len = self.held.len();
        (len.saturating_sub(self.longest - 1)..len).find(|&q| self.could_start(&self.held[q..])).unwrap_or(len)
    }

    /// Whether `tail` is a proper prefix of a value.
    fn could_start(&self, tail: &[u8]) -> bool {
        let candidates = if tail.len() >= 2 {
            let pair = u16::from_be_bytes([tail[0], tail[1]]);
            if self.pairs[pair as usize >> 6] & (1 << (pair & 63)) == 0 {
                return false;
            }
            &self.by_pair[&pair]
        } else {
            &self.by_first[tail[0] as usize]
        };
        candidates.iter().any(|&i| self.patterns[i].len() > tail.len() && self.patterns[i].starts_with(tail))
    }
}

/// Copies `from` to `to` through `m` until `from` ends, `to` fails, or [`AFTER_EXIT`] has passed
/// since the command exited (in all: a process it left behind that keeps writing doesn't hold roll).
/// A tail that could start a value waits for more output or the end, never for a timer: a program
/// that block-buffers can pause mid-value, and its first half mustn't go out alone.
async fn pump(mut from: impl AsyncRead + Unpin, mut to: impl std::io::Write, mut m: Masker, mut exited: watch::Receiver<bool>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = vec![];
    let after = || tokio::time::Instant::now() + AFTER_EXIT;
    let mut deadline = if *exited.borrow() { Some(after()) } else { None };
    loop {
        let read = match deadline {
            Some(d) => tokio::time::timeout_at(d, from.read(&mut buf)).await.unwrap_or(Ok(0)),
            None => tokio::select! {
                r = from.read(&mut buf) => r,
                // A dropped sender counts as an exit too.
                _ = exited.changed() => {
                    deadline = Some(after());
                    continue;
                }
            },
        };
        // A failed read ends the stream as its end does.
        let n = read.unwrap_or(0);
        if n == 0 {
            m.flush(&mut out);
            let _ = write(&mut to, &mut out);
            return;
        }
        m.push(&buf[..n], &mut out);
        if write(&mut to, &mut out).is_err() {
            return;
        }
    }
}

/// Written in place: tokio's stdout hands each write to another thread, which cost more than the
/// masking (about 25,000 round trips for 400 MB). The pump is its own task on a multi-threaded
/// runtime, so blocking here holds up nothing else.
fn write(to: &mut impl std::io::Write, out: &mut Vec<u8>) -> std::io::Result<()> {
    if !out.is_empty() {
        tokio::task::block_in_place(|| {
            to.write_all(out)?;
            to.flush()
        })?;
        out.clear();
    }
    Ok(())
}

/// Which of the command's streams go through roll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Streams {
    pub stdout: bool,
    pub stderr: bool,
}

impl Streams {
    /// `--no-mask`: neither. `--mask` (or `ROLL_MASK=1`, for a pod's or a unit's environment):
    /// both. Otherwise stderr when it isn't a terminal, or in CI (`CI` set) where a PTY is a log
    /// too: it's diagnostics, never data. stdout is data as often as a log (`pg_dump > dump.sql`,
    /// `| gzip`, over ssh): it's masked only in CI, and never into a file, so nothing written as data
    /// is ever changed unasked.
    pub fn choose(force: bool, off: bool) -> Streams {
        if off {
            return Streams { stdout: false, stderr: false };
        }
        if force || std::env::var("ROLL_MASK").is_ok_and(|v| v == "1" || v == "true") {
            return Streams { stdout: true, stderr: true };
        }
        let ci = ci();
        let stdout = std::io::stdout();
        Streams { stdout: ci && !regular_file(&stdout), stderr: ci || !std::io::stderr().is_terminal() }
    }

    pub fn any(self) -> bool {
        self.stdout || self.stderr
    }
}

fn ci() -> bool {
    std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "false" && v != "0")
}

/// Whether `f` is a regular file (output redirected to one is data, not a log).
fn regular_file(f: &impl std::os::fd::AsFd) -> bool {
    f.as_fd().try_clone_to_owned().ok().map(std::fs::File::from).and_then(|f| f.metadata().ok()).is_some_and(|m| m.is_file())
}

/// A command started with `env`, its masked streams copied by tasks of their own.
pub struct Running {
    pub child: tokio::process::Child,
    pumps: Vec<JoinHandle<()>>,
    exited: watch::Sender<bool>,
}

/// `conceal`: the values roll added (an env file's own plain values aren't secrets).
pub fn spawn(command: &[String], env: &[(String, String)], conceal: &[&str], streams: Streams) -> std::io::Result<Running> {
    let masker = || Masker::new(conceal.iter().copied());
    let streams = if masker().is_some() { streams } else { Streams { stdout: false, stderr: false } };
    let mut cmd = tokio::process::Command::new(&command[0]);
    cmd.args(&command[1..]).envs(env.iter().cloned());
    if streams.stdout {
        cmd.stdout(Stdio::piped());
    }
    if streams.stderr {
        cmd.stderr(Stdio::piped());
    }
    let mut child = cmd.spawn()?;
    let (exited, rx) = watch::channel(false);
    let mut pumps = vec![];
    if let (Some(from), Some(m)) = (child.stdout.take(), masker()) {
        pumps.push(tokio::spawn(pump(from, std::io::stdout(), m, rx.clone())));
    }
    if let (Some(from), Some(m)) = (child.stderr.take(), masker()) {
        pumps.push(tokio::spawn(pump(from, std::io::stderr(), m, rx)));
    }
    Ok(Running { child, pumps, exited })
}

impl Running {
    /// After the command exited: writes what's left of its output.
    pub async fn drain(self) {
        let _ = self.exited.send(true);
        for p in self.pumps {
            let _ = p.await;
        }
    }
}

/// The signals roll passes on to its command: SIGTERM and SIGHUP (`docker stop`, systemd, a closed
/// terminal), and SIGINT unless roll's terminal sent it, since the terminal sends it to the
/// command too (twice would be "exit immediately" to terraform or npm).
#[cfg(unix)]
pub struct Signals {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    pub fn new() -> std::io::Result<Signals> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Signals { term: signal(SignalKind::terminate())?, hup: signal(SignalKind::hangup())?, int: signal(SignalKind::interrupt())? })
    }

    /// The next signal to pass on.
    pub async fn next(&mut self) -> i32 {
        loop {
            tokio::select! {
                _ = self.term.recv() => return libc::SIGTERM,
                _ = self.hup.recv() => return libc::SIGHUP,
                _ = self.int.recv() => {
                    if !foreground() {
                        return libc::SIGINT;
                    }
                }
            }
        }
    }

    /// Passes `sig` to `child`.
    pub fn pass(child: &tokio::process::Child, sig: i32) {
        if let Some(pid) = child.id() {
            // SAFETY: kill(2) on our own child's pid.
            unsafe { libc::kill(pid as libc::pid_t, sig) };
        }
    }
}

/// Whether roll is in its controlling terminal's foreground group (then a Ctrl-C reached the
/// command as well). No controlling terminal: no.
#[cfg(unix)]
fn foreground() -> bool {
    use std::os::fd::AsRawFd;
    let Ok(tty) = std::fs::File::open("/dev/tty") else { return false };
    // SAFETY: tcgetpgrp and getpgrp only read; the fd is open for the call.
    unsafe { libc::tcgetpgrp(tty.as_raw_fd()) == libc::getpgrp() }
}

/// Exits as the command did: with its code, or by the same SIGINT, SIGTERM or SIGHUP, so a shell
/// loop around `roll run` stops on Ctrl-C as it would around the command.
pub fn exit_as(status: std::process::ExitStatus) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal().filter(|s| [libc::SIGINT, libc::SIGTERM, libc::SIGHUP].contains(s)) {
            // SAFETY: back to the default action, then raised on ourselves.
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
        }
    }
    std::process::exit(code(status))
}

/// The exit code a shell would give: the command's own, or 128 plus the signal that ended it.
pub fn code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `input` split at every pair of points (and whole) through a masker, flushing at the end.
    fn masked(values: &[&str], input: &str) -> String {
        let base = Masker::new(values.iter().copied()).unwrap();
        let mut whole = vec![];
        let mut m = base.clone();
        m.push(input.as_bytes(), &mut whole);
        m.flush(&mut whole);
        let b = input.as_bytes();
        for i in 0..=b.len() {
            for j in i..=b.len() {
                let mut out = vec![];
                let mut m = base.clone();
                for part in [&b[..i], &b[i..j], &b[j..]] {
                    m.push(part, &mut out);
                }
                m.flush(&mut out);
                assert_eq!(out, whole, "split at {i} and {j}");
            }
        }
        String::from_utf8(whole).unwrap()
    }

    #[test]
    fn values_are_concealed_however_the_output_is_split() {
        let key = "sk_live_51Habc";
        assert_eq!(masked(&[key, "true"], &format!("key={key} debug=true\n")), "key=<concealed by roll> debug=true\n");
        // The longer of two values that share a start wins, even when the shorter one ends a read.
        assert_eq!(masked(&["password", "password123"], "a password123 b password c"), "a <concealed by roll> b <concealed by roll> c");
        // Each line of a multi-line value, and its JSON and URL encodings.
        let pem = "-----BEGIN KEY-----\nMIIBVgIBADANBgkqhkiG9w0BAQEFAASCAUAwggE8\n-----END KEY-----";
        assert_eq!(masked(&[pem], "MIIBVgIBADANBgkqhkiG9w0BAQEFAASCAUAwggE8"), "<concealed by roll>");
        // A line of structure in a multi-line value isn't a value: printed alone, it stays.
        let key_file = "{\n  \"type\": \"service_account\",\n  \"key\": \"abcdefgh12345678\"\n}";
        assert_eq!(masked(&[key_file], "  \"type\": \"service_account\",\n"), "  \"type\": \"service_account\",\n");
        // Its string fields are values of their own (a cloud key file's key, printed alone), minified too.
        assert_eq!(masked(&[key_file], "key=abcdefgh12345678"), "key=<concealed by roll>");
        assert_eq!(masked(&[r#"{"type":"service_account","private_key":"zyxwvuts98765432"}"#], "pk zyxwvuts98765432"), "pk <concealed by roll>");
        assert_eq!(masked(&["p@ss w/rd\"x"], r#"{"pw":"p@ss w/rd\"x"} ?pw=p%40ss%20w%2Frd%22x"#), r#"{"pw":"<concealed by roll>"} ?pw=<concealed by roll>"#);
        assert!(Masker::new(["short", "true"]).is_none());
    }

    #[test]
    fn output_that_cant_start_a_value_isnt_held() {
        let mut m = Masker::new(["sk_live_51Habc"]).unwrap();
        let mut out = vec![];
        m.push(b"listening on :8080\n", &mut out);
        assert_eq!(out, b"listening on :8080\n");
        assert!(!m.holding());
        out.clear();
        m.push(b"key sk_li", &mut out);
        assert_eq!(out, b"key ");
        assert!(m.holding());
    }
}
