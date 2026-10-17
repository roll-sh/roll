//! WAL safekeeper: streams WAL from a running compute over physical replication, keeps it
//! durable in a local spool, and ships it to object storage.
//!
//! A replication slot on the compute guarantees no gaps across restarts of either side. The
//! position we acknowledge (flush) is what's fsynced locally; completed segments are uploaded
//! immediately and the segment in progress is refreshed in object storage every few seconds.

use crate::lsn::{Lsn, SEG_SIZE, history_file, parse_segment_file, segment_file};
use crate::objects::{Objects, wal_key};
use crate::repl::{Copy, ReplConn, pg_code};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::fs::{File, OpenOptions};
use std::net::SocketAddr;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

pub const SLOT: &str = "roll_safekeeper";
const STATUS_EVERY: Duration = Duration::from_secs(1);
const PARTIAL_UPLOAD_EVERY: Duration = Duration::from_secs(10);
/// Microseconds between the Unix and Postgres (2000-01-01) epochs.
const PG_EPOCH_OFFSET_US: i64 = 946_684_800_000_000;

pub struct ReceiverArgs {
    pub branch: String,
    pub addr: SocketAddr,
    pub user: String,
    pub password: String,
    pub spool: PathBuf,
    pub objects: Objects,
    /// Durable position from a previous run on this branch, if any.
    pub resume: Option<(u32, Lsn)>,
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    /// Streaming started. `fresh` means history restarts here (new slot / new timeline), so a new
    /// base image is needed before this point can be restored.
    Streaming { timeline: u32, start: Lsn, fresh: bool },
    /// WAL up to `lsn` is durable.
    Flushed { timeline: u32, lsn: Lsn },
}

fn now_pg_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as i64 - PG_EPOCH_OFFSET_US
}

/// Run until the compute goes away. Returns the final durable position.
pub async fn run(args: ReceiverArgs, on_event: impl Fn(Event) + Send + Sync) -> anyhow::Result<Option<(u32, Lsn)>> {
    std::fs::create_dir_all(&args.spool)?;
    let mut uploads = JoinSet::new();
    sweep_spool(&args, &mut uploads)?;

    let mut conn = connect_with_retry(&args).await?;
    let ident = conn.query("IDENTIFY_SYSTEM").await?;
    let row = ident.first().ok_or_else(|| anyhow::anyhow!("IDENTIFY_SYSTEM returned nothing"))?;
    let timeline: u32 = row.get(1).cloned().flatten().unwrap_or_default().parse()?;
    let xlogpos: Lsn = row.get(2).cloned().flatten().unwrap_or_default().parse()?;

    if timeline > 1 {
        upload_history(&args, &mut conn, timeline).await?;
    }

    let mut fresh = false;
    let mut start = match args.resume {
        Some((t, lsn)) if t == timeline => Some(lsn.segment_start()),
        _ => None,
    };
    if let Some(s) = start {
        let cmd = format!("START_REPLICATION SLOT {SLOT} PHYSICAL {s} TIMELINE {timeline}");
        if let Err(e) = conn.start_copy(&cmd).await {
            warn!(branch = %args.branch, error = %e, code = ?pg_code(&e), "can't resume WAL stream; starting fresh");
            start = None;
        }
    }
    let start = match start {
        Some(s) => s,
        None => {
            fresh = true;
            let _ = conn.query(&format!("DROP_REPLICATION_SLOT {SLOT}")).await;
            conn.query(&format!("CREATE_REPLICATION_SLOT {SLOT} PHYSICAL RESERVE_WAL")).await?;
            // Physical slots report no consistent point; the slot's restart_lsn is where WAL is kept from.
            let rows = conn.query(&format!("READ_REPLICATION_SLOT {SLOT}")).await?;
            let point = rows
                .first()
                .and_then(|r| r.get(1).cloned().flatten())
                .and_then(|s| s.parse::<Lsn>().ok())
                .filter(|l| l.0 != 0);
            let s = point.unwrap_or(xlogpos).segment_start();
            conn.start_copy(&format!("START_REPLICATION SLOT {SLOT} PHYSICAL {s} TIMELINE {timeline}")).await?;
            s
        }
    };
    info!(branch = %args.branch, timeline, %start, fresh, "WAL streaming started");
    on_event(Event::Streaming { timeline, start, fresh });

    let mut seg = SegmentWriter::new(&args, timeline);
    let mut received = start;
    let mut flushed = start;
    let mut last_status = Instant::now();
    let mut last_partial = Instant::now();

    let result: anyhow::Result<()> = async {
        loop {
            let msg = tokio::time::timeout(STATUS_EVERY, conn.recv_copy()).await;
            let mut reply_now = false;
            match msg {
                Err(_) => {} // idle tick
                Ok(Ok(Copy::Done)) => break,
                Ok(Err(e)) => {
                    debug!(branch = %args.branch, error = %e, "WAL stream ended");
                    break;
                }
                Ok(Ok(Copy::Data(mut body))) => match body.get_u8() {
                    b'w' if body.len() >= 24 => {
                        let data_start = Lsn(body.get_u64());
                        body.advance(16); // walEnd, sendTime
                        seg.write(data_start, &body, &mut uploads)?;
                        received = received.max(Lsn(data_start.0 + body.len() as u64));
                    }
                    b'k' if body.len() >= 17 => {
                        body.advance(16);
                        reply_now = body.get_u8() == 1;
                    }
                    _ => {}
                },
            }
            if reply_now || last_status.elapsed() >= STATUS_EVERY {
                seg.sync()?;
                if received > flushed {
                    flushed = received;
                    on_event(Event::Flushed { timeline, lsn: flushed });
                }
                conn.send_copy(&status_update(flushed)).await?;
                last_status = Instant::now();
            }
            if last_partial.elapsed() >= PARTIAL_UPLOAD_EVERY {
                seg.upload_partial(&mut uploads);
                last_partial = Instant::now();
            }
            while let Some(r) = uploads.try_join_next() {
                if let Err(e) = r? {
                    warn!(branch = %args.branch, error = %e, "WAL upload failed; will retry from spool");
                }
            }
        }
        Ok(())
    }
    .await;

    seg.sync()?;
    if received > flushed {
        flushed = received;
        on_event(Event::Flushed { timeline, lsn: flushed });
    }
    seg.upload_partial(&mut uploads);
    while let Some(r) = uploads.join_next().await {
        if let Err(e) = r? {
            warn!(branch = %args.branch, error = %e, "WAL upload failed; will retry from spool");
        }
    }
    info!(branch = %args.branch, %flushed, "WAL streaming stopped");
    result?;
    Ok(Some((timeline, flushed)))
}

async fn connect_with_retry(args: &ReceiverArgs) -> anyhow::Result<ReplConn> {
    let mut delay = Duration::from_millis(100);
    let mut attempts = 0;
    loop {
        match ReplConn::connect(args.addr, &args.user, &args.password, "roll_safekeeper").await {
            Ok(c) => return Ok(c),
            Err(e) if attempts < 8 => {
                debug!(error = %e, "replication connect failed; retrying");
                attempts += 1;
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
            Err(e) => return Err(e),
        }
    }
}

async fn upload_history(args: &ReceiverArgs, conn: &mut ReplConn, timeline: u32) -> anyhow::Result<()> {
    let key = wal_key(&args.branch, &history_file(timeline));
    if args.objects.exists(&key).await? {
        return Ok(());
    }
    let rows = conn.query(&format!("TIMELINE_HISTORY {timeline}")).await?;
    if let Some(content) = rows.first().and_then(|r| r.get(1).cloned().flatten()) {
        args.objects.put(&key, Bytes::from(content)).await?;
    }
    Ok(())
}

fn status_update(lsn: Lsn) -> Bytes {
    let mut b = BytesMut::with_capacity(34);
    b.put_u8(b'r');
    b.put_u64(lsn.0); // written
    b.put_u64(lsn.0); // flushed
    b.put_u64(lsn.0); // applied
    b.put_i64(now_pg_us());
    b.put_u8(0);
    b.freeze()
}

/// Upload completed segments a previous run left behind (e.g. S3 was unreachable).
fn sweep_spool(args: &ReceiverArgs, uploads: &mut JoinSet<anyhow::Result<()>>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(&args.spool)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if parse_segment_file(&name).is_some() {
            uploads.spawn(upload_segment(args.objects.clone(), args.branch.clone(), entry.path(), name, false));
        }
    }
    Ok(())
}

async fn upload_segment(objects: Objects, branch: String, path: PathBuf, name: String, partial: bool) -> anyhow::Result<()> {
    let p = path.clone();
    let compressed = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let raw = std::fs::read(&p)?;
        Ok(zstd::encode_all(&raw[..], 3)?)
    })
    .await??;
    if partial {
        objects.put(&wal_key(&branch, &format!("{name}.partial.zst")), compressed.into()).await?;
    } else {
        objects.put(&wal_key(&branch, &format!("{name}.zst")), compressed.into()).await?;
        objects.delete(&wal_key(&branch, &format!("{name}.partial.zst"))).await?;
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

struct OpenSegment {
    segno: u64,
    name: String,
    file: File,
    end: u64,
    dirty: bool,
    synced: bool,
}

struct SegmentWriter {
    spool: PathBuf,
    branch: String,
    objects: Objects,
    timeline: u32,
    cur: Option<OpenSegment>,
}

impl SegmentWriter {
    fn new(args: &ReceiverArgs, timeline: u32) -> Self {
        Self { spool: args.spool.clone(), branch: args.branch.clone(), objects: args.objects.clone(), timeline, cur: None }
    }

    fn partial_path(&self, name: &str) -> PathBuf {
        self.spool.join(format!("{name}.partial"))
    }

    fn write(&mut self, start: Lsn, mut data: &[u8], uploads: &mut JoinSet<anyhow::Result<()>>) -> anyhow::Result<()> {
        let mut pos = start.0;
        while !data.is_empty() {
            let segno = pos / SEG_SIZE;
            let off = pos % SEG_SIZE;
            let n = data.len().min((SEG_SIZE - off) as usize);
            if self.cur.as_ref().map(|c| c.segno) != Some(segno) {
                self.open(segno)?;
            }
            let cur = self.cur.as_mut().unwrap();
            cur.file.write_all_at(&data[..n], off)?;
            cur.end = cur.end.max(off + n as u64);
            cur.dirty = true;
            cur.synced = false;
            if cur.end == SEG_SIZE {
                self.complete(uploads)?;
            }
            pos += n as u64;
            data = &data[n..];
        }
        Ok(())
    }

    fn open(&mut self, segno: u64) -> anyhow::Result<()> {
        self.sync()?;
        let name = segment_file(self.timeline, segno);
        let path = self.partial_path(&name);
        let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path)?;
        if file.metadata()?.len() < SEG_SIZE {
            file.set_len(SEG_SIZE)?;
        }
        self.cur = Some(OpenSegment { segno, name, file, end: 0, dirty: false, synced: true });
        Ok(())
    }

    fn sync(&mut self) -> anyhow::Result<()> {
        if let Some(c) = self.cur.as_mut().filter(|c| !c.synced) {
            c.file.sync_data()?;
            c.synced = true;
        }
        Ok(())
    }

    fn complete(&mut self, uploads: &mut JoinSet<anyhow::Result<()>>) -> anyhow::Result<()> {
        self.sync()?;
        let cur = self.cur.take().unwrap();
        let full = self.spool.join(&cur.name);
        std::fs::rename(self.partial_path(&cur.name), &full)?;
        uploads.spawn(upload_segment(self.objects.clone(), self.branch.clone(), full, cur.name, false));
        Ok(())
    }

    fn upload_partial(&mut self, uploads: &mut JoinSet<anyhow::Result<()>>) {
        if let Some(cur) = self.cur.as_mut().filter(|c| c.dirty) {
            cur.dirty = false;
            let path = self.spool.join(format!("{}.partial", cur.name));
            uploads.spawn(upload_segment(self.objects.clone(), self.branch.clone(), path, cur.name.clone(), true));
        }
    }
}

/// Fetch a WAL file for `restore_command`: local spool first (newest), then object storage.
/// Partial segments are returned zero-padded, which Postgres treats as end of WAL.
pub async fn fetch_wal(objects: &Objects, spool: &Path, branch: &str, file: &str) -> anyhow::Result<Option<Bytes>> {
    if file.ends_with(".history") {
        return objects.get(&wal_key(branch, file)).await;
    }
    anyhow::ensure!(parse_segment_file(file).is_some(), "not a WAL file name: {file}");
    for local in [spool.join(file), spool.join(format!("{file}.partial"))] {
        if let Ok(data) = tokio::fs::read(&local).await {
            return Ok(Some(data.into()));
        }
    }
    for key in [format!("{file}.zst"), format!("{file}.partial.zst")] {
        if let Some(data) = objects.get(&wal_key(branch, &key)).await? {
            let raw = tokio::task::spawn_blocking(move || zstd::decode_all(&data[..])).await??;
            return Ok(Some(raw.into()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::tests::temp_objects;

    fn args(objects: Objects, spool: PathBuf) -> ReceiverArgs {
        ReceiverArgs {
            branch: "br_t".into(),
            addr: ([127, 0, 0, 1], 1).into(),
            user: "roll".into(),
            password: String::new(),
            spool,
            objects,
            resume: None,
        }
    }

    #[tokio::test]
    async fn segments_complete_upload_and_serve() {
        let (objects, dir) = temp_objects("segments");
        let spool = dir.join("spool");
        std::fs::create_dir_all(&spool).unwrap();
        let a = args(objects.clone(), spool.clone());
        let mut w = SegmentWriter::new(&a, 1);
        let mut uploads = JoinSet::new();

        // Write across the boundary between segments 1 and 2.
        let start = Lsn(SEG_SIZE * 2 - 100);
        let data: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        w.write(start, &data, &mut uploads).unwrap();
        w.sync().unwrap();
        w.upload_partial(&mut uploads);
        while let Some(r) = uploads.join_next().await {
            r.unwrap().unwrap();
        }

        let seg1 = segment_file(1, 1);
        let seg2 = segment_file(1, 2);
        // The completed segment went to object storage and left the spool.
        assert!(objects.exists(&wal_key("br_t", &format!("{seg1}.zst"))).await.unwrap());
        assert!(!spool.join(&seg1).exists());
        // The open one is in the spool and mirrored as a partial.
        assert!(spool.join(format!("{seg2}.partial")).exists());
        assert!(objects.exists(&wal_key("br_t", &format!("{seg2}.partial.zst"))).await.unwrap());

        // restore_command view: full segment from S3, partial from the spool, zero padded.
        let full = fetch_wal(&objects, &spool, "br_t", &seg1).await.unwrap().unwrap();
        assert_eq!(full.len() as u64, SEG_SIZE);
        assert_eq!(&full[(SEG_SIZE - 100) as usize..], &data[..100]);
        let partial = fetch_wal(&objects, &spool, "br_t", &seg2).await.unwrap().unwrap();
        assert_eq!(partial.len() as u64, SEG_SIZE);
        assert_eq!(&partial[..200], &data[100..]);
        assert!(partial[200..].iter().all(|b| *b == 0));

        // With the spool gone (host loss), the partial comes from object storage.
        std::fs::remove_dir_all(&spool).unwrap();
        let from_s3 = fetch_wal(&objects, &spool, "br_t", &seg2).await.unwrap().unwrap();
        assert_eq!(&from_s3[..200], &data[100..]);

        assert!(fetch_wal(&objects, &spool, "br_t", &segment_file(1, 9)).await.unwrap().is_none());
        assert!(fetch_wal(&objects, &spool, "br_t", "../../etc/passwd").await.is_err());
    }

    #[tokio::test]
    async fn history_files_are_served() {
        let (objects, dir) = temp_objects("history");
        objects.put(&wal_key("br_t", "00000002.history"), Bytes::from_static(b"1\t0/3000000\tno recovery target specified\n")).await.unwrap();
        let h = fetch_wal(&objects, &dir, "br_t", "00000002.history").await.unwrap().unwrap();
        assert!(h.starts_with(b"1\t0/3000000"));
        assert!(fetch_wal(&objects, &dir, "br_t", "00000003.history").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn spool_sweep_uploads_leftovers() {
        let (objects, dir) = temp_objects("sweep");
        let spool = dir.join("spool");
        std::fs::create_dir_all(&spool).unwrap();
        let name = segment_file(1, 5);
        std::fs::write(spool.join(&name), vec![1u8; 1024]).unwrap();
        std::fs::write(spool.join("not-a-segment"), b"x").unwrap();
        let mut uploads = JoinSet::new();
        sweep_spool(&args(objects.clone(), spool.clone()), &mut uploads).unwrap();
        while let Some(r) = uploads.join_next().await {
            r.unwrap().unwrap();
        }
        assert!(objects.exists(&wal_key("br_t", &format!("{name}.zst"))).await.unwrap());
        assert!(!spool.join(&name).exists());
        assert!(spool.join("not-a-segment").exists());
    }

    #[test]
    fn status_update_layout() {
        let b = status_update(Lsn(0x0102_0304_0506_0708));
        assert_eq!(b.len(), 34);
        assert_eq!(b[0], b'r');
        assert_eq!(&b[1..9], &0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(&b[9..17], &b[1..9]);
        assert_eq!(b[33], 0);
    }
}
