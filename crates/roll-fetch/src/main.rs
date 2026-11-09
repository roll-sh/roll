//! `roll-fetch`: runs beside customer code in an isolated task (docs/cloud/README.md, "Isolated
//! work"), from roll's own small image, with no AWS credentials of its own.
//!
//! - `fetch --url <get> [--delete <url>] --tree <dir> --env <file> [--chown <uid>]`: download the
//!   run's bundle through its presigned URL, delete the object, and unpack it (`roll_fetch::unpack`).
//!   The customer's container starts only once this exits 0.
//! - `deadline <seconds>`: exit after that long. It's an essential container of a check's task,
//!   so a check whose node went away still ends.
//! - `proxy --listen <port> --to <port> --idle <seconds>`: the fixed port fleet nodes reach a
//!   preview on, forwarded to the app's port; it exits (ending the task) once no byte has gone
//!   either way for `idle`, so a preview whose owner went away still stops.

use anyhow::{Context, bail};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn flags(args: &[String]) -> anyhow::Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let k = k.strip_prefix("--").with_context(|| format!("unexpected argument {k:?}"))?;
        out.insert(k.to_string(), it.next().with_context(|| format!("--{k} needs a value"))?.clone());
    }
    Ok(out)
}

fn need<'a>(f: &'a HashMap<String, String>, k: &str) -> anyhow::Result<&'a str> {
    f.get(k).map(String::as_str).with_context(|| format!("--{k} is required"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("fetch") => fetch(&args[1..]).await,
        Some("deadline") => deadline(&args[1..]).await,
        Some("proxy") => proxy(&args[1..]).await,
        _ => Err(anyhow::anyhow!("usage: roll-fetch fetch|deadline|proxy …")),
    };
    if let Err(e) = r {
        eprintln!("roll-fetch: {e:#}");
        std::process::exit(1);
    }
}

async fn fetch(args: &[String]) -> anyhow::Result<()> {
    let f = flags(args)?;
    let (url, tree, env) = (need(&f, "url")?, PathBuf::from(need(&f, "tree")?), PathBuf::from(need(&f, "env")?));
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let staging = env.with_extension("bundle");
    let mut last = None;
    for attempt in 0..5u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
        }
        match download(&client, url, &staging).await {
            Ok(()) => {
                last = None;
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    if let Some(e) = last {
        return Err(e.context("downloading the bundle"));
    }
    // The object holds the run's env: it goes as soon as it's here (rolld deletes it too).
    if let Some(del) = f.get("delete")
        && let Err(e) = client.delete(del).send().await.and_then(|r| r.error_for_status())
    {
        eprintln!("roll-fetch: deleting the bundle: {e}");
    }
    let file = std::fs::File::open(&staging)?;
    let n = roll_fetch::unpack(std::io::BufReader::new(file), &tree, &env, roll_fetch::Limits::default())?;
    std::fs::remove_file(&staging)?;
    if let Some(uid) = f.get("chown") {
        let uid: u32 = uid.parse().context("--chown is a uid")?;
        if let Some(dir) = env.parent() {
            std::os::unix::fs::chown(dir, Some(uid), None)?;
        }
        std::os::unix::fs::chown(&env, Some(uid), None)?;
    }
    eprintln!("roll-fetch: {} files, {} bytes", n.files, n.bytes);
    Ok(())
}

async fn download(client: &reqwest::Client, url: &str, to: &std::path::Path) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut r = client.get(url).send().await?.error_for_status()?;
    let mut f = tokio::fs::File::create(to).await?;
    let max = roll_fetch::Limits::default().max_bytes;
    let mut n = 0u64;
    while let Some(chunk) = r.chunk().await? {
        n += chunk.len() as u64;
        if n > max {
            bail!("the bundle is larger than {} MiB", max >> 20);
        }
        f.write_all(&chunk).await?;
    }
    f.flush().await?;
    Ok(())
}

async fn deadline(args: &[String]) -> anyhow::Result<()> {
    let secs: u64 = args.first().context("deadline <seconds>")?.parse().context("seconds")?;
    tokio::time::sleep(Duration::from_secs(secs)).await;
    eprintln!("roll-fetch: the run's deadline passed");
    Ok(())
}

async fn proxy(args: &[String]) -> anyhow::Result<()> {
    let f = flags(args)?;
    let listen: u16 = need(&f, "listen")?.parse()?;
    let to: u16 = need(&f, "to")?.parse()?;
    let idle = Duration::from_secs(need(&f, "idle")?.parse()?);
    let l = tokio::net::TcpListener::bind(("0.0.0.0", listen)).await?;
    let start = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            c = l.accept() => {
                let Ok((client, _)) = c else { continue };
                let last = last.clone();
                tokio::spawn(async move {
                    let Ok(upstream) = tokio::net::TcpStream::connect(("127.0.0.1", to)).await else { return };
                    pipe(client, upstream, start, last).await;
                });
            }
            _ = tick.tick() => {
                let quiet = start.elapsed().saturating_sub(Duration::from_millis(last.load(Ordering::Relaxed)));
                if quiet > idle {
                    eprintln!("roll-fetch: nothing for {}s; stopping the preview", idle.as_secs());
                    return Ok(());
                }
            }
        }
    }
}

/// Copy both ways, noting when bytes last moved.
async fn pipe(a: tokio::net::TcpStream, b: tokio::net::TcpStream, start: Instant, last: Arc<AtomicU64>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (ar, aw) = a.into_split();
    let (br, bw) = b.into_split();
    let one = |mut r: tokio::net::tcp::OwnedReadHalf, mut w: tokio::net::tcp::OwnedWriteHalf, last: Arc<AtomicU64>| async move {
        let mut buf = vec![0u8; 16 << 10];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    last.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                    if w.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = w.shutdown().await;
    };
    tokio::join!(one(ar, bw, last.clone()), one(br, aw, last));
}
