//! `roll secrets mount` (docs/secrets.md, "A local `.env`"): a folder's values as a named pipe at
//! `.env`, for the tools that read only a file (Next.js, Vite, Prisma, Compose's `env_file`, an
//! IDE's run configuration), as 1Password Environments and Doppler's `run --mount` do. Nothing is
//! ever on disk: each open of the pipe gets the current values, written once from this process,
//! then the pipe closes.
//!
//! The values come from the local agent's cache (`local_agent::Store`): the folder's digest is
//! followed, so a change reaches the next open without logging in again, and roll's refusal (a 401
//! after logging in again, a 403 or a 404) stops serving and removes the pipe, as it drops a folder
//! from the agent. A value never goes in a log, an error, argv or a temporary file.

/// `roll secrets mount`'s arguments (its help is `Secrets::Mount`'s).
#[derive(clap::Args)]
pub struct Mount {
    /// Where the pipe goes: usually `.env`. It mustn't exist, unless it's a pipe a mount that has
    /// ended left behind.
    #[arg(value_name = "PATH")]
    target: std::path::PathBuf,
    /// What each read gets.
    #[arg(long, value_enum, default_value_t = Format::Dotenv)]
    format: Format,
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum Format {
    /// `KEY=value` lines, quoted where needed.
    Dotenv,
    /// One JSON object of keys to values.
    Json,
}

#[cfg(not(unix))]
pub async fn run(_: &crate::Api, _: &super::AtArgs, _: Mount) -> anyhow::Result<()> {
    anyhow::bail!("roll secrets mount serves a named pipe, which Windows doesn't have in this sense: run the tool with the values in its environment instead, with no file: roll run -- <command>")
}

#[cfg(unix)]
pub use unix::run;

#[cfg(unix)]
mod unix {
    use super::{Format, Mount};
    use crate::Api;
    use crate::secrets::AtArgs;
    use crate::secrets::local_agent::{Folder, READ_TIMEOUT, Store, refused, unresolved};
    use anyhow::{Context, bail};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    /// How long the mount answers from memory after roll last confirmed the folder (the digest's long
    /// poll confirms it every 25 s while roll answers): the local agent's default.
    const TTL: Duration = Duration::from_secs(300);
    /// How long one read may take to drain what it's given before the mount moves on.
    const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

    pub async fn run(api: &Api, args: &AtArgs, m: Mount) -> anyhow::Result<()> {
        // A reader that goes away mid-write is an error to log, not the end of the mount.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
        let target = absolute(&m.target)?;
        let stale = check_target(&target)?;
        let (at, project) = crate::secrets::locate(api, args).await?;
        let f = Folder { env: at.env.clone(), path: at.path.clone() };
        let store = Arc::new(Store::new("roll secrets mount", api.with_header(None), &project, TTL, 1));
        let mut refusals = store.refusals();
        // The first read: a login or folder roll refuses fails here, before anything is made.
        let first = tokio::time::timeout(READ_TIMEOUT, store.read(&f)).await.map_err(|_| anyhow::anyhow!("reading {f}: no answer from roll in {} s", READ_TIMEOUT.as_secs()))?.with_context(|| format!("reading {f}"))?;
        if !first.problems.is_empty() {
            bail!("{}: the mount serves only a folder whose every key resolves (roll secrets ls shows them)", unresolved(&f, &first));
        }
        drop(first);
        ignore_in_git(&target);
        let mut pipe = Pipe::make(&target, stale)?;
        let format = match m.format {
            Format::Dotenv => "dotenv",
            Format::Json => "json",
        };
        eprintln!("roll secrets mount: {} serves {f} of {} as {format}; each read gets the current values. Ctrl-C removes it.", target.display(), at.label.as_deref().unwrap_or(&project));
        let ended = tokio::select! {
            r = serve(&store, &f, &mut pipe, m.format) => r,
            why = refusal(&mut refusals) => Err(anyhow::anyhow!("{why}")),
            r = stop_signal() => r,
        };
        pipe.remove();
        match ended {
            Ok(()) => {
                eprintln!("roll secrets mount: removed {}", target.display());
                Ok(())
            }
            Err(e) => Err(e.context(format!("removed {} and stopped serving it", target.display()))),
        }
    }

    fn absolute(p: &Path) -> anyhow::Result<PathBuf> {
        let dir = match p.parent().filter(|d| !d.as_os_str().is_empty()) {
            Some(d) => d.canonicalize().with_context(|| format!("{}: no such directory", d.display()))?,
            None => std::env::current_dir()?,
        };
        let name = p.file_name().with_context(|| format!("{} names no file", p.display()))?;
        Ok(dir.join(name))
    }

    /// The pipe a mount serves at its path, by its device and inode (a new one for each reader:
    /// `Pipe::swap`), and the file that says this mount made it (under `$XDG_STATE_HOME/roll/mounts`,
    /// holding the mount's pid).
    struct Pipe {
        path: PathBuf,
        id: (u64, u64),
        record: Option<PathBuf>,
    }

    fn records() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))?;
        Some(base.join("roll").join("mounts"))
    }

    fn record_of(id: (u64, u64)) -> Option<PathBuf> {
        records().map(|d| d.join(format!("{}-{}", id.0, id.1)))
    }

    /// A named pipe (0600) at `p`, by its device and inode; an error if anything is there.
    fn mkfifo(p: &Path) -> anyhow::Result<(u64, u64)> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).context("a path with a NUL byte")?;
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("making the pipe {}", p.display()));
        }
        Ok(id_of(&std::fs::symlink_metadata(p)?))
    }

    fn id_of(m: &std::fs::Metadata) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino())
    }

    /// Nothing at `target`, or a pipe an ended mount left (returned, to be made again). Anything else
    /// is refused: a regular file is never overwritten, and a pipe another mount still serves, or one
    /// no mount made, is someone else's.
    fn check_target(target: &Path) -> anyhow::Result<Option<(u64, u64)>> {
        use std::os::unix::fs::FileTypeExt;
        let m = match std::fs::symlink_metadata(target) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("checking {}", target.display())),
        };
        let t = target.display();
        if !m.file_type().is_fifo() {
            bail!("{t} exists and isn't a pipe roll secrets mount made: it's never overwritten. Move it away first (a .env with values: roll secrets import {t}, then delete it)");
        }
        let id = id_of(&m);
        let pid = record_of(id).and_then(|r| std::fs::read_to_string(r).ok()).and_then(|p| p.trim().parse::<i32>().ok());
        match pid {
            None => bail!("{t} is a pipe roll secrets mount didn't make: remove it first if it's yours"),
            // Signal 0 checks the process exists (EPERM: it does, as someone else).
            Some(pid) if unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) => {
                bail!("{t} is served by another roll secrets mount (pid {pid}): stop that one first (or remove {t}, if that pid is something else now)")
            }
            Some(_) => Ok(Some(id)),
        }
    }

    impl Pipe {
        /// `mkfifo` (0600) at `target`, after removing the pipe an ended mount left there.
        fn make(target: &Path, stale: Option<(u64, u64)>) -> anyhow::Result<Pipe> {
            if let Some(old) = stale {
                // Only the very pipe that was checked.
                if std::fs::symlink_metadata(target).is_ok_and(|m| id_of(&m) == old) {
                    std::fs::remove_file(target).with_context(|| format!("removing the pipe an ended mount left at {}", target.display()))?;
                }
                if let Some(r) = record_of(old) {
                    let _ = std::fs::remove_file(r);
                }
            }
            let id = mkfifo(target)?;
            let record = record_of(id).filter(|r| {
                let made = r.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && note(r);
                if !made {
                    eprintln!("roll secrets mount: couldn't note the pipe in {} (a later mount won't take it over if this one is killed)", r.display());
                }
                made
            });
            Ok(Pipe { path: target.to_path_buf(), id, record })
        }

        /// Puts a new pipe at the path, so the reader that has just opened this one is the only one
        /// it ever has.
        ///
        /// A reader sees the end of its copy only when it reads while no writer holds its pipe. If
        /// the next read were served by opening the same pipe again, that open would succeed at once
        /// while a slow reader still held it (one that hasn't read the end yet), and that reader would
        /// get the next copy appended to its own. Nothing tells a writer that a reader has gone: an
        /// open for writing without blocking succeeds while any reader holds the pipe, and is itself
        /// a writer that a new reader waiting in `open` takes for its own (an empty read); waiting
        /// for the reader to close before closing deadlocks, as it closes only after the end. So the
        /// old pipe leaves the path the moment its reader has it: made under a temporary name next to
        /// the path, then renamed over it (atomic on Linux and macOS). Nobody can open the old pipe
        /// again, so its reader gets one copy and the end once this mount closes it, however late it
        /// reads.
        fn swap(&mut self) -> anyhow::Result<()> {
            let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let tmp = self.path.with_file_name(format!(".{name}.roll-mount-{}", std::process::id()));
            let id = mkfifo(&tmp)?;
            // Never rename over something that took the pipe's place.
            if !std::fs::symlink_metadata(&self.path).is_ok_and(|m| self.ours(&m)) {
                let _ = std::fs::remove_file(&tmp);
                bail!("{} isn't the pipe this mount made any more", self.path.display());
            }
            if let Err(e) = std::fs::rename(&tmp, &self.path) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e).with_context(|| format!("putting a new pipe at {}", self.path.display()));
            }
            self.id = id;
            if let Some(old) = self.record.take() {
                self.record = record_of(id).filter(|r| note(r));
                let _ = std::fs::remove_file(old);
            }
            Ok(())
        }

        /// Whether `target` is still this pipe.
        fn ours(&self, m: &std::fs::Metadata) -> bool {
            use std::os::unix::fs::FileTypeExt;
            m.file_type().is_fifo() && id_of(m) == self.id
        }

        /// Removes the pipe (if it's still this one) and its record. An open blocked on it stays in its
        /// own thread until the process exits.
        fn remove(&self) {
            if std::fs::symlink_metadata(&self.path).is_ok_and(|m| self.ours(&m)) {
                let _ = std::fs::remove_file(&self.path);
            }
            if let Some(r) = &self.record {
                let _ = std::fs::remove_file(r);
            }
        }

        /// The next reader's end, opened in a thread of its own (an open of a pipe blocks until a
        /// reader comes, and a process ends without waiting for such a thread).
        async fn next_reader(&self) -> anyhow::Result<std::fs::File> {
            use std::os::unix::fs::OpenOptionsExt;
            let (tx, rx) = tokio::sync::oneshot::channel();
            let path = self.path.clone();
            std::thread::spawn(move || {
                let _ = tx.send(std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(path));
            });
            let file = rx.await.context("opening the pipe")?.with_context(|| format!("opening {}", self.path.display()))?;
            // Never write into something that took the pipe's place.
            anyhow::ensure!(file.metadata().is_ok_and(|m| self.ours(&m)), "{} isn't the pipe this mount made any more", self.path.display());
            Ok(file)
        }
    }

    /// This process's pid, in a mount's record.
    fn note(r: &Path) -> bool {
        std::fs::write(r, format!("{}\n", std::process::id())).is_ok()
    }

    /// One open, one copy of the current values, until a read is refused.
    async fn serve(store: &Arc<Store>, f: &Folder, pipe: &mut Pipe, format: Format) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        loop {
            let file = pipe.next_reader().await?;
            pipe.swap()?;
            let text = match tokio::time::timeout(READ_TIMEOUT, store.read(f)).await {
                Ok(Ok(r)) if r.problems.is_empty() => render(&r.values, format),
                Ok(Ok(r)) => {
                    let why = unresolved(f, &r);
                    eprintln!("roll secrets mount: {why}; this read got no values");
                    match format {
                        Format::Dotenv => format!("# roll secrets mount: {why}\n"),
                        Format::Json => String::new(),
                    }
                }
                Ok(Err(e)) if refused(&e) => return Err(e.context(format!("roll refused reading {f}"))),
                Ok(Err(e)) => {
                    eprintln!("roll secrets mount: reading {f}: {e:#}; this read got no values");
                    String::new()
                }
                Err(_) => {
                    eprintln!("roll secrets mount: reading {f}: no answer from roll in {} s; this read got no values", READ_TIMEOUT.as_secs());
                    String::new()
                }
            };
            let mut w = tokio::net::unix::pipe::Sender::from_file(file).context("serving the pipe")?;
            match tokio::time::timeout(WRITE_TIMEOUT, w.write_all(text.as_bytes())).await {
                Ok(Ok(())) => {}
                // An I/O error names no value.
                Ok(Err(e)) => eprintln!("roll secrets mount: a reader went away before it read everything ({e})"),
                Err(_) => eprintln!("roll secrets mount: a reader didn't read everything in {} s; closed it", WRITE_TIMEOUT.as_secs()),
            }
        }
    }

    fn render(values: &std::collections::BTreeMap<String, String>, format: Format) -> String {
        match format {
            Format::Dotenv => values.iter().map(|(k, v)| format!("{k}={}\n", crate::secrets::dotenv_quote(v))).collect(),
            Format::Json => serde_json::to_string_pretty(values).expect("strings") + "\n",
        }
    }

    /// roll's next refusal of the followed folder.
    async fn refusal(rx: &mut tokio::sync::watch::Receiver<Option<String>>) -> String {
        loop {
            if rx.changed().await.is_err() {
                return std::future::pending().await;
            }
            if let Some(why) = rx.borrow_and_update().clone() {
                return why;
            }
        }
    }

    /// Ctrl-C, SIGTERM (`kill`, systemd) or SIGHUP (the terminal closed).
    async fn stop_signal() -> anyhow::Result<()> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut hup = signal(SignalKind::hangup())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
        Ok(())
    }

    /// When `target` is in a git work tree and not ignored, appends it to the work tree's `.gitignore`
    /// and says so. git decides what's ignored (`check-ignore`); without git, or outside a work tree,
    /// nothing happens.
    fn ignore_in_git(target: &Path) {
        let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else { return };
        let git = |args: &[&std::ffi::OsStr]| std::process::Command::new("git").arg("-C").arg(dir).args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok();
        let Some(top) = git(&["rev-parse".as_ref(), "--show-toplevel".as_ref()]).filter(|o| o.status.success()) else { return };
        let ignored = || git(&["check-ignore".as_ref(), "-q".as_ref(), "--".as_ref(), name]).and_then(|o| o.status.code());
        if ignored() != Some(1) {
            // 0: ignored already; anything else: git couldn't say.
            return;
        }
        let top = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim_end_matches(['\n', '\r']));
        let Some(rel) = top.canonicalize().ok().and_then(|t| target.strip_prefix(t).ok().map(Path::to_path_buf)) else { return };
        let pattern = format!("/{}", gitignore_escape(&rel.to_string_lossy()));
        let file = top.join(".gitignore");
        let mut text = std::fs::read_to_string(&file).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("# roll secrets mount: a pipe, never a file to commit\n{pattern}\n"));
        match std::fs::write(&file, text) {
            Ok(()) if ignored() == Some(0) => eprintln!("roll secrets mount: added {pattern} to {}", file.display()),
            Ok(()) => eprintln!("roll secrets mount: added {pattern} to {}, but git still doesn't ignore it: check that file", file.display()),
            Err(e) => eprintln!("roll secrets mount: {} isn't ignored by git, and adding it to {} failed ({e}): add it yourself", target.display(), file.display()),
        }
    }

    /// A path as a `.gitignore` pattern that matches only itself.
    fn gitignore_escape(p: &str) -> String {
        let mut out = String::new();
        for (i, c) in p.chars().enumerate() {
            if matches!(c, '*' | '?' | '[' | '\\') || (i == 0 && matches!(c, '#' | '!')) {
                out.push('\\');
            }
            out.push(c);
        }
        if out.ends_with(' ') {
            out.pop();
            out.push_str("\\ ");
        }
        out
    }
}
