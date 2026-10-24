//! Memory load harness for the git server: concurrent large pushes against a backend that
//! accepts and drops everything, so the server's own buffering is what's measured.
//!
//! ```text
//! push_load gen <file> <MiB>     # a receive-pack request body: one create + a pack of random blobs
//! push_load serve <port>         # serves /git/load.git with ServerConfig::default()
//! ```
//!
//! Run the server under a memory cap and post the body N times at once, e.g. with
//! `curl -X POST -T <file> http://127.0.0.1:<port>/git/load.git/git-receive-pack`, then read
//! `VmHWM` from `/proc/<pid>/status` (or the scope's `memory.peak`).

use async_trait::async_trait;
use code_git::{
    Access, Credentials, GitBackend, GitResult, Kind, Object, ObjectId, ReceiveOutcome, ReceiveRequest, RefList, RefStatus,
    ServerConfig, pack,
};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Accepts every push and drops its objects.
#[derive(Default)]
struct Null {
    pushes: AtomicU64,
}

#[async_trait]
impl GitBackend for Null {
    async fn authorize(&self, _repo: &str, _c: Option<&Credentials>, _a: Access) -> GitResult<Option<String>> {
        Ok(None)
    }
    async fn refs(&self, _repo: &str) -> GitResult<RefList> {
        Ok(RefList { head: Some("refs/heads/main".into()), refs: Vec::new() })
    }
    async fn read_object(&self, _repo: &str, _oid: &ObjectId) -> GitResult<Option<Object>> {
        Ok(None)
    }
    async fn receive(&self, _repo: &str, req: ReceiveRequest) -> GitResult<ReceiveOutcome> {
        let n = self.pushes.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("push {n}: {} objects", req.objects.len());
        Ok(ReceiveOutcome {
            refs: req.updates.iter().map(|u| RefStatus { name: u.name.clone(), result: Ok(()) }).collect(),
            messages: Vec::new(),
        })
    }
}

fn generate(path: &str, mib: usize) {
    // xorshift: incompressible, so the pack is as large as the objects.
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let blob = 4 << 20;
    let objects: Vec<Object> = (0..(mib << 20).div_ceil(blob))
        .map(|_| {
            let mut d = vec![0u8; blob];
            for c in d.chunks_mut(8) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                c.copy_from_slice(&x.to_le_bytes()[..c.len()]);
            }
            Object::new(Kind::Blob, d)
        })
        .collect();
    let line = format!("{} {} refs/heads/load\0report-status side-band-64k\n", ObjectId::ZERO, objects[0].id);
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    write!(f, "{:04x}{line}0000", line.len() + 4).unwrap();
    f.write_all(&pack::write(&objects)).unwrap();
    f.flush().unwrap();
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gen") => generate(&args[2], args[3].parse().unwrap()),
        Some("serve") => {
            let app = code_git::router(Arc::new(Null::default()), ServerConfig::default());
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", args[2].parse::<u16>().unwrap())).await.unwrap();
            println!("ready {}", std::process::id());
            axum::serve(listener, app).await.unwrap();
        }
        _ => eprintln!("usage: push_load gen <file> <MiB> | push_load serve <port>"),
    }
}
