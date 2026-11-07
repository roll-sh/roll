//! Power loss, for tests (the `powerloss` feature). With `ROLL_POWERLOSS=<root>:<shadow>`
//! set, every change `durable` makes under `<root>` is recorded in `<shadow>` as a power cut
//! would leave it, and `power_cut` rebuilds `<root>` from that record alone: every write not
//! covered by an fsync of its file is gone, and so is every create, rename and unlink not
//! covered by an fsync of its directory. (`keep`: a seeded random part of them stays, as a disk
//! may have written some of its cache before the power went.) Whatever is under `<root>` when
//! the process starts counts as durable.
//!
//! The shadow holds, per file id, `f<id>` (its bytes as of its last fsync), `p<id>` (its writes
//! since, numbered) and `a<id>` (the number of the last write an fsync covered); per directory
//! id, `d<id>` (its entries as of its last fsync) and `c<id>` (its entries now), as
//! `F <id> <name>` / `D <id> <name>` lines. The root is directory 0.

use super::durable::Change;
use parking_lot::{Mutex, MutexGuard};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A directory's entries: name to (id, is a directory).
type Entries = BTreeMap<String, (u64, bool)>;

struct State {
    root: PathBuf,
    shadow: PathBuf,
    next: u64,
    /// Every directory under the root (the root too), by path: its id and entries.
    dirs: HashMap<PathBuf, (u64, Entries)>,
    /// Per file id: writes recorded so far, and `p<id>` while it holds any not yet fsynced.
    writes: HashMap<u64, (u64, Option<File>)>,
    /// Per file id: the last write an fsync covered.
    applied: HashMap<u64, u64>,
}

static STATE: OnceLock<Option<Mutex<State>>> = OnceLock::new();

fn state() -> Option<MutexGuard<'static, State>> {
    STATE
        .get_or_init(|| {
            let v = std::env::var("ROLL_POWERLOSS").ok()?;
            let (root, shadow) = v.split_once(':')?;
            Some(Mutex::new(State::power_on(norm(Path::new(root)), norm(Path::new(shadow))).expect("powerloss: record the power-on state")))
        })
        .as_ref()
        .map(|m| m.lock())
}

fn norm(p: &Path) -> PathBuf {
    let p = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap().join(p) };
    p.components().collect()
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn listing(e: &Entries) -> String {
    e.iter().map(|(n, (id, dir))| format!("{} {id} {n}\n", if *dir { 'D' } else { 'F' })).collect()
}

fn parse_listing(text: &str) -> Entries {
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ' ');
            let (k, id, n) = (it.next()?, it.next()?.parse().ok()?, it.next()?);
            Some((n.to_string(), (id, k == "D")))
        })
        .collect()
}

impl State {
    fn power_on(root: PathBuf, shadow: PathBuf) -> io::Result<State> {
        let _ = std::fs::remove_dir_all(&shadow);
        std::fs::create_dir_all(&shadow)?;
        std::fs::create_dir_all(&root)?;
        let mut s = State { root: root.clone(), shadow, next: 1, dirs: HashMap::new(), writes: HashMap::new(), applied: HashMap::new() };
        s.scan(&root, 0)?;
        Ok(s)
    }

    /// Record `dir` (id `id`) and everything under it as durable.
    fn scan(&mut self, dir: &Path, id: u64) -> io::Result<()> {
        let mut entries = Entries::new();
        let mut subdirs = Vec::new();
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let n = self.next;
            self.next += 1;
            if e.file_type()?.is_dir() {
                subdirs.push((e.path(), n));
            } else {
                std::fs::copy(e.path(), self.shadow.join(format!("f{n}")))?;
            }
            entries.insert(e.file_name().to_string_lossy().into_owned(), (n, e.file_type()?.is_dir()));
        }
        self.put(&format!("d{id}"), &listing(&entries))?;
        self.put(&format!("c{id}"), &listing(&entries))?;
        self.dirs.insert(dir.to_path_buf(), (id, entries));
        for (p, n) in subdirs {
            self.scan(&p, n)?;
        }
        Ok(())
    }

    /// Replace a shadow file whole (a kill leaves the old one or the new one).
    fn put(&self, name: &str, text: &str) -> io::Result<()> {
        let tmp = self.shadow.join(format!("{name}.tmp"));
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, self.shadow.join(name))
    }

    /// The parent directory (when it is recorded) and the name of `p`.
    fn split(&self, p: &Path) -> Option<(PathBuf, String)> {
        let parent = p.parent()?;
        (p.starts_with(&self.root) && self.dirs.contains_key(parent)).then(|| (parent.to_path_buf(), name_of(p)))
    }

    /// `dir`'s entries changed: record them as they are now.
    fn changed(&self, dir: &Path) {
        if let Some((id, e)) = self.dirs.get(dir) {
            self.put(&format!("c{id}"), &listing(e)).expect("powerloss: record a directory");
        }
    }

    fn new_id(&mut self) -> u64 {
        self.next += 1;
        self.next - 1
    }

    /// Append write `kind` (0: `data` at `off`; 1: the length set to `off`) to `p<id>`.
    fn log(&mut self, id: u64, kind: u8, off: u64, data: &[u8]) {
        let shadow = &self.shadow;
        let (n, f) = self.writes.entry(id).or_insert((0, None));
        let f = f.get_or_insert_with(|| {
            let f = OpenOptions::new().create(true).append(true).open(shadow.join(format!("p{id}")));
            f.expect("powerloss: record a write")
        });
        *n += 1;
        let mut rec = Vec::with_capacity(21 + data.len());
        rec.extend_from_slice(&n.to_le_bytes());
        rec.push(kind);
        rec.extend_from_slice(&off.to_le_bytes());
        rec.extend_from_slice(&(data.len() as u32).to_le_bytes());
        rec.extend_from_slice(data);
        f.write_all(&rec).expect("powerloss: record a write");
    }

    /// An fsync of file `id` covered its writes up to `upto`: apply them to `f<id>`.
    fn apply(&mut self, id: u64, upto: u64) -> io::Result<()> {
        let from = self.applied.get(&id).copied().unwrap_or(0);
        if upto <= from {
            return Ok(());
        }
        let a = OpenOptions::new().create(true).write(true).truncate(false).open(self.shadow.join(format!("a{id}")))?;
        a.write_all_at(&upto.to_le_bytes(), 0)?;
        let f = OpenOptions::new().create(true).write(true).truncate(false).open(self.shadow.join(format!("f{id}")))?;
        for (n, kind, off, data) in pending(&std::fs::read(self.shadow.join(format!("p{id}")))?) {
            if n > from && n <= upto {
                if kind == 0 { f.write_all_at(data, off)? } else { f.set_len(off)? }
            }
        }
        self.applied.insert(id, upto);
        // (Every write fsynced: the next one starts `p<id>` again.)
        if let Some((n, p)) = self.writes.get_mut(&id)
            && *n == upto
        {
            *p = None;
            std::fs::remove_file(self.shadow.join(format!("p{id}")))?;
        }
        Ok(())
    }
}

/// The whole records of a `p<id>`: (number, kind, offset, data).
fn pending(b: &[u8]) -> Vec<(u64, u8, u64, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 21 <= b.len() {
        let n = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let off = u64::from_le_bytes(b[i + 9..i + 17].try_into().unwrap());
        let len = u32::from_le_bytes(b[i + 17..i + 21].try_into().unwrap()) as usize;
        let Some(data) = b.get(i + 21..i + 21 + len) else { break };
        out.push((n, b[i + 8], off, data));
        i += 21 + len;
    }
    out
}

/// `DFile::open`: the file's id (none when it isn't recorded).
pub(super) fn open(path: &Path, truncate: bool, f: impl FnOnce() -> io::Result<File>) -> io::Result<(File, Option<u64>)> {
    let Some(mut s) = state() else { return Ok((f()?, None)) };
    let p = norm(path);
    let Some((parent, name)) = s.split(&p) else { return Ok((f()?, None)) };
    let file = f()?;
    let id = match s.dirs[&parent].1.get(&name) {
        Some(&(id, false)) => id,
        _ => {
            let id = s.new_id();
            s.dirs.get_mut(&parent).unwrap().1.insert(name, (id, false));
            s.changed(&parent);
            id
        }
    };
    if truncate {
        s.log(id, 1, 0, &[]);
    }
    Ok((file, Some(id)))
}

/// The id of the file at `path`, if it is recorded.
pub(super) fn id_of(path: &Path) -> Option<u64> {
    let s = state()?;
    let (parent, name) = s.split(&norm(path))?;
    s.dirs[&parent].1.get(&name).filter(|e| !e.1).map(|e| e.0)
}

pub(super) fn wrote(id: Option<u64>, off: u64, data: &[u8]) {
    if let (Some(id), Some(mut s)) = (id, id.and_then(|_| state())) {
        s.log(id, 0, off, data);
    }
}

/// An fsync of file `id` (`f`): it covers the writes made before it began.
pub(super) fn synced(id: Option<u64>, f: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    let upto = id.and_then(|id| Some(state()?.writes.get(&id).map_or(0, |w| w.0)));
    f()?;
    if let (Some(id), Some(upto)) = (id, upto) {
        state().unwrap().apply(id, upto)?;
    }
    Ok(())
}

/// An fsync of directory `dir` (`f`): its entries now are durable. An entry made outside
/// `durable` is adopted as it is now, as if fsynced (and named in `<shadow>/adopted`: the
/// harness requires there be none it relies on); one removed outside it goes from the record.
pub(super) fn dir_synced(dir: &Path, f: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    f()?;
    let Some(mut s) = state() else { return Ok(()) };
    let p = norm(dir);
    let Some((id, entries)) = s.dirs.get(&p).cloned() else { return Ok(()) };
    let mut now = Entries::new();
    let mut adopted = Vec::new();
    for e in std::fs::read_dir(&p)? {
        let e = e?;
        let n = e.file_name().to_string_lossy().into_owned();
        match entries.get(&n) {
            Some(&v) => {
                now.insert(n, v);
            }
            None => {
                let cid = s.new_id();
                if e.file_type()?.is_dir() {
                    s.scan(&e.path(), cid)?;
                } else {
                    std::fs::copy(e.path(), s.shadow.join(format!("f{cid}")))?;
                }
                adopted.push(format!("{}\n", e.path().display()));
                now.insert(n, (cid, e.file_type()?.is_dir()));
            }
        }
    }
    for (n, (_, d)) in &entries {
        if *d && !now.contains_key(n) {
            let gone = p.join(n);
            s.dirs.retain(|k, _| !k.starts_with(&gone));
        }
    }
    if !adopted.is_empty() {
        let mut f = OpenOptions::new().create(true).append(true).open(s.shadow.join("adopted"))?;
        f.write_all(adopted.concat().as_bytes())?;
    }
    s.put(&format!("d{id}"), &listing(&now))?;
    s.put(&format!("c{id}"), &listing(&now))?;
    s.dirs.insert(p, (id, now));
    Ok(())
}

/// A rename, removal or `create_dir_all` (`f`), made under the record's lock.
pub(super) fn change<T>(what: Change<'_>, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let Some(mut s) = state() else { return f() };
    match what {
        Change::Rename(from, to) => {
            let (from, to) = (norm(from), norm(to));
            let r = f()?;
            let moved = s.split(&from).and_then(|(dir, n)| {
                let e = s.dirs.get_mut(&dir)?.1.remove(&n);
                s.changed(&dir);
                e
            });
            if let Some((dir, n)) = s.split(&to) {
                let e = match moved {
                    Some(e) => e,
                    // (From outside the record: new to it, written whole.)
                    None => {
                        let id = s.new_id();
                        let data = std::fs::read(&to)?;
                        s.log(id, 0, 0, &data);
                        (id, false)
                    }
                };
                if e.1 {
                    let inner: Vec<PathBuf> = s.dirs.keys().filter(|k| k.starts_with(&from)).cloned().collect();
                    for k in inner {
                        let v = s.dirs.remove(&k).unwrap();
                        s.dirs.insert(to.join(k.strip_prefix(&from).unwrap()), v);
                    }
                }
                s.dirs.get_mut(&dir).unwrap().1.insert(n, e);
                s.changed(&dir);
            }
            Ok(r)
        }
        Change::Remove(path) => {
            let p = norm(path);
            let r = f()?;
            if let Some((dir, n)) = s.split(&p) {
                s.dirs.get_mut(&dir).unwrap().1.remove(&n);
                s.dirs.retain(|k, _| !k.starts_with(&p));
                s.changed(&dir);
            }
            Ok(r)
        }
        Change::MakeDirs(path) => {
            let p = norm(path);
            let missing: Vec<PathBuf> = p.ancestors().take_while(|a| a.starts_with(&s.root) && !s.dirs.contains_key(*a)).map(Path::to_path_buf).collect();
            let r = f()?;
            for d in missing.into_iter().rev() {
                let Some((parent, n)) = s.split(&d) else { break };
                let id = s.new_id();
                s.dirs.get_mut(&parent).unwrap().1.insert(n, (id, true));
                s.dirs.insert(d.clone(), (id, Entries::new()));
                s.changed(&parent);
                s.changed(&d);
            }
            Ok(r)
        }
    }
}

/// Cut the power: rebuild `root` from the record in `shadow` of the process that changed it
/// (`ROLL_POWERLOSS`), which must be dead. `keep`: a seed, to keep a random part of what
/// wasn't fsynced (each pending write of a file, each entry of a directory as it is now
/// against as its fsync left it); none keeps nothing.
pub fn power_cut(root: &Path, shadow: &Path, keep: Option<u64>) -> io::Result<Cut> {
    let mut rng = keep.map(|s| s.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let stage = root.with_extension("powercut");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    let mut cut = Cut::default();
    rebuild(shadow, 0, &stage, &mut rng, &mut cut)?;
    std::fs::remove_dir_all(root)?;
    std::fs::rename(&stage, root)?;
    Ok(cut)
}

/// What a power cut took: writes no fsync covered, and directory entries (creates, renames,
/// removals) whose directory no fsync covered.
#[derive(Debug, Default)]
pub struct Cut {
    pub writes: usize,
    pub entries: usize,
}

fn coin(rng: &mut Option<u64>) -> bool {
    let Some(x) = rng else { return false };
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    (*x >> 32) & 1 == 1
}

fn rebuild(shadow: &Path, id: u64, at: &Path, rng: &mut Option<u64>, cut: &mut Cut) -> io::Result<()> {
    let read = |name: String| std::fs::read(shadow.join(name)).unwrap_or_default();
    let mut entries = parse_listing(&String::from_utf8_lossy(&read(format!("d{id}"))));
    let now = parse_listing(&String::from_utf8_lossy(&read(format!("c{id}"))));
    let names: std::collections::BTreeSet<String> = entries.keys().chain(now.keys()).cloned().collect();
    for n in names {
        if entries.get(&n) == now.get(&n) {
            continue;
        }
        if !coin(rng) {
            cut.entries += 1;
            continue;
        }
        match now.get(&n) {
            Some(e) => entries.insert(n, *e),
            None => entries.remove(&n),
        };
    }
    for (name, (cid, dir)) in entries {
        let to = at.join(&name);
        if dir {
            std::fs::create_dir_all(&to)?;
            rebuild(shadow, cid, &to, rng, cut)?;
            continue;
        }
        let mut data = read(format!("f{cid}"));
        let done = u64::from_le_bytes(read(format!("a{cid}")).get(..8).map_or([0; 8], |b| b.try_into().unwrap()));
        for (n, kind, off, w) in pending(&read(format!("p{cid}"))) {
            if n <= done {
                continue;
            }
            if !coin(rng) {
                cut.writes += 1;
            } else if kind == 1 {
                data.resize(off as usize, 0);
            } else {
                let end = off as usize + w.len();
                if data.len() < end {
                    data.resize(end, 0);
                }
                data[off as usize..end].copy_from_slice(w);
            }
        }
        std::fs::write(&to, data)?;
    }
    Ok(())
}
