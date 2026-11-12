//! Running our cqlsh and the references' cqlsh side by side.
#![allow(dead_code)]

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub const SCRATCH: &str = "/tmp/roll-cqlsh-golden";

#[derive(Clone)]
pub struct Ref {
    pub dialect: &'static str,
    pub container: &'static str,
    pub host: &'static str,
    pub port: u16,
}

pub const SCYLLA: Ref = Ref { dialect: "scylla", container: "nosql-ref-scylla", host: "127.0.0.1", port: 19042 };
pub const CASSANDRA: Ref = Ref { dialect: "cassandra", container: "nosql-ref-cassandra", host: "127.0.0.1", port: 29042 };
pub const SCYLLA_AUTH: Ref = Ref { dialect: "scylla", container: "nosql-ref-scylla-auth", host: "127.0.0.1", port: 19043 };
pub const CASSANDRA_AUTH: Ref = Ref { dialect: "cassandra", container: "nosql-ref-cassandra-auth", host: "127.0.0.1", port: 29043 };

/// stdout, stderr, exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub out: String,
    pub err: String,
    pub code: i32,
}

impl Ref {
    pub fn reachable(&self) -> bool {
        let a: SocketAddr = format!("{}:{}", self.host, self.port).parse().unwrap();
        TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_ok()
    }

    /// The image's own cqlsh.
    pub fn real(&self, args: &[String]) -> Run {
        let o = Command::new("docker").arg("exec").arg(self.container).arg("cqlsh").args(args).output().expect("docker exec");
        Run {
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
            code: o.status.code().unwrap_or(-1),
        }
    }

    pub fn real_stdin(&self, args: &[String], stdin: &str) -> Run {
        let mut c = Command::new("docker")
            .arg("exec")
            .arg("-i")
            .arg(self.container)
            .arg("cqlsh")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("docker exec");
        c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let o = c.wait_with_output().unwrap();
        Run {
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
            code: o.status.code().unwrap_or(-1),
        }
    }
}

/// `roll nosql cqlsh --host … --port … <args>`.
pub fn ours(r: &Ref, args: &[String], stdin: Option<&str>) -> Run {
    let home = std::env::temp_dir().join("roll-cqlsh-home");
    let _ = std::fs::create_dir_all(&home);
    let mut c = Command::new(env!("CARGO_BIN_EXE_roll"));
    c.args(["nosql", "cqlsh", "--host", r.host, "--port", &r.port.to_string()])
        .args(args)
        .env("TZ", "UTC")
        .env("HOME", &home)
        .env_remove("CQLSH_HOST")
        .env_remove("CQLSH_PORT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().expect("roll");
    let input = stdin.unwrap_or("").to_string();
    let mut w = child.stdin.take().unwrap();
    std::thread::spawn(move || {
        let _ = w.write_all(input.as_bytes());
    });
    let o = child.wait_with_output().unwrap();
    Run {
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
        code: o.status.code().unwrap_or(-1),
    }
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Copies the corpus and support files to the scratch directory here and (when recording)
/// in the container.
pub fn stage_files(r: &Ref, in_container: bool) {
    let _ = std::fs::create_dir_all(SCRATCH);
    for sub in ["tests/cqlsh/corpus", "tests/cqlsh/support"] {
        for e in std::fs::read_dir(manifest().join(sub)).unwrap().flatten() {
            std::fs::copy(e.path(), Path::new(SCRATCH).join(e.file_name())).unwrap();
        }
    }
    if in_container {
        let _ = Command::new("docker").args(["exec", r.container, "mkdir", "-p", SCRATCH]).output();
        let _ = Command::new("docker").args(["cp", &format!("{SCRATCH}/."), &format!("{}:{SCRATCH}/", r.container)]).output();
    }
}

/// Removes files scripts create (CAPTURE appends).
pub fn reset_scratch(r: &Ref, in_container: bool) {
    let export = format!("export_{}.csv", r.dialect);
    for f in ["capture.txt", "capture2.txt", export.as_str()] {
        let _ = std::fs::remove_file(Path::new(SCRATCH).join(f));
        if in_container {
            let _ = Command::new("docker").args(["exec", r.container, "rm", "-f", &format!("{SCRATCH}/{f}")]).output();
        }
    }
}

fn golden_dir(r: &Ref) -> PathBuf {
    manifest().join("tests/cqlsh/golden").join(r.dialect)
}

pub fn write_golden(r: &Ref, name: &str, run: &Run) {
    let d = golden_dir(r);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(format!("{name}.out")), &run.out).unwrap();
    std::fs::write(d.join(format!("{name}.err")), &run.err).unwrap();
    std::fs::write(d.join(format!("{name}.code")), format!("{}\n", run.code)).unwrap();
}

pub fn read_golden(r: &Ref, name: &str) -> Run {
    let d = golden_dir(r);
    let read = |ext: &str| std::fs::read_to_string(d.join(format!("{name}.{ext}"))).unwrap_or_else(|_| panic!("no golden {name}.{ext}: record with ROLL_CQLSH_RECORD=1"));
    Run { out: read("out"), err: read("err"), code: read("code").trim().parse().unwrap() }
}

/// Masks what legitimately differs between the image's cqlsh and ours.
pub fn mask(r: &Run) -> Run {
    let host = regex::Regex::new(r"(?m)^Connected to (.*) at \S+$").unwrap();
    let progress = regex::Regex::new(r"Processed: \d+ rows; Rate: +\d+ rows/s; Avg\. rate: +\d+ rows/s\r+").unwrap();
    let took = regex::Regex::new(r"in [0-9a-z ,.]+ seconds").unwrap();
    let procs = regex::Regex::new(r"Using \d+ child processes").unwrap();
    let addr = regex::Regex::new(r"\{'[0-9a-fA-F.:\[\]]+:\d+': ").unwrap();
    let auth = regex::Regex::new(r"authenticate to [0-9a-fA-F.:\[\]]+:\d+:").unwrap();
    let hosts = regex::Regex::new(r"\['[0-9a-fA-F.:]+'\]\)").unwrap();
    let f = |s: &str| {
        let s = addr.replace_all(s, "{'ADDR': ");
        let s = auth.replace_all(&s, "authenticate to ADDR:");
        let s = hosts.replace_all(&s, "['HOST'])");
        let s = host.replace_all(&s, "Connected to $1 at HOST");
        let s = progress.replace_all(&s, "");
        let s = took.replace_all(&s, "in N seconds");
        procs.replace_all(&s, "Using N child processes").into_owned()
    };
    Run { out: f(&r.out), err: f(&r.err), code: r.code }
}

/// `None` when equal, else where they first differ.
pub fn compare(want: &Run, got: &Run) -> Option<String> {
    let mut d = vec![];
    for (what, a, b) in [("stdout", &want.out, &got.out), ("stderr", &want.err, &got.err)] {
        if a != b {
            let (la, lb): (Vec<&str>, Vec<&str>) = (a.split('\n').collect(), b.split('\n').collect());
            let i = la.iter().zip(&lb).position(|(x, y)| x != y).unwrap_or(la.len().min(lb.len()));
            d.push(format!(
                "{what} differs at line {}:\n  want: {:?}\n  got:  {:?}",
                i + 1,
                la.get(i).copied().unwrap_or("<end>"),
                lb.get(i).copied().unwrap_or("<end>")
            ));
        }
    }
    if want.code != got.code {
        d.push(format!("exit code: want {} got {}", want.code, got.code));
    }
    (!d.is_empty()).then(|| d.join("\n"))
}

/// The corpus scripts, `(name, text)`.
pub fn corpus() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cqlsh/corpus");
    let mut v: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "cql"))
        .map(|p| (p.file_stem().unwrap().to_string_lossy().into_owned(), std::fs::read_to_string(&p).unwrap()))
        .collect();
    v.sort();
    v
}

pub fn exec_cases() -> Vec<String> {
    let text = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cqlsh/exec.txt")).unwrap();
    text.split("\n----\n").map(|s| s.trim_end_matches('\n').to_string()).filter(|s| !s.is_empty()).collect()
}

/// Every golden case: `(name, cqlsh arguments)`, in order.
pub fn cases() -> Vec<(String, Vec<String>)> {
    let mut cases: Vec<(String, Vec<String>)> = vec![];
    for (name, _) in corpus() {
        cases.push((format!("{name}.f"), vec!["-f".into(), format!("{SCRATCH}/{name}.cql")]));
    }
    for (i, e) in exec_cases().into_iter().enumerate() {
        cases.push((format!("exec{i:02}.e"), vec!["-e".into(), e]));
    }
    cases
}
