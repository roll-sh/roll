//! PTYs for terminals on the canvas (agent-proto `terminal.*`).
//!
//! T3 Code's environment server owns PTYs and their retained output the same way
//! (`docs/internals/terminal-runtime.md`, `apps/server/src/terminal/Manager.ts`): output is
//! appended to a bounded history and sent live as chunks; a client that opens a terminal later
//! reads the history once. Here the history is a 256 KiB byte ring, kept in memory (a terminal
//! doesn't outlive the daemon).
//!
//! A terminal is a child in its own session (`setsid`) with the PTY as its controlling terminal,
//! running the login shell or `$SHELL -lc <command>`. Both PTY ends are close-on-exec, so other
//! children of the daemon (git, providers) never hold them open.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Output a terminal keeps for clients that attach later.
pub const SCROLLBACK: usize = 256 << 10;
const READ_CHUNK: usize = 64 << 10;

/// Retained output: the newest [`SCROLLBACK`] bytes.
#[derive(Default)]
pub struct Ring {
    buf: VecDeque<u8>,
}

impl Ring {
    pub fn push(&mut self, data: &[u8]) {
        let data = &data[data.len().saturating_sub(SCROLLBACK)..];
        let over = (self.buf.len() + data.len()).saturating_sub(SCROLLBACK);
        self.buf.drain(..over);
        self.buf.extend(data);
    }

    pub fn bytes(&self) -> Vec<u8> {
        let (a, b) = self.buf.as_slices();
        [a, b].concat()
    }
}

/// What to run.
pub struct Spawn<'a> {
    pub cwd: &'a Path,
    /// Run this through the shell instead of an interactive login shell.
    pub command: Option<&'a str>,
    /// More environment for it (a project script's `ROLL_PROJECT_ROOT`, …).
    pub env: &'a [(&'static str, String)],
    pub cols: u16,
    pub rows: u16,
}

/// A running PTY: write input, resize, kill. Output and the exit arrive on [`Spawned`].
pub struct Pty {
    master: Arc<Mutex<File>>,
    pid: i32,
}

pub struct Spawned {
    pub pty: Pty,
    /// Output chunks until the PTY closes.
    pub output: mpsc::Receiver<Vec<u8>>,
    pub child: tokio::process::Child,
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows.max(1), ws_col: cols.max(1), ws_xpixel: 0, ws_ypixel: 0 }
}

fn cvt(ret: libc::c_int) -> std::io::Result<libc::c_int> {
    if ret < 0 { Err(std::io::Error::last_os_error()) } else { Ok(ret) }
}

/// The user's shell: `$SHELL`, else `/bin/sh`.
pub fn shell() -> String {
    std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into())
}

pub fn spawn(spec: &Spawn<'_>) -> std::io::Result<Spawned> {
    let (mut master, mut slave) = (0, 0);
    let ws = winsize(spec.cols, spec.rows);
    // SAFETY: out-pointers to two ints; no name buffer; default termios; a valid winsize.
    cvt(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) })?;
    // SAFETY: openpty returned two fresh descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        // SAFETY: fcntl on a descriptor we own.
        cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
    }

    let shell = shell();
    let mut cmd = tokio::process::Command::new(&shell);
    match spec.command {
        Some(command) => cmd.arg("-lc").arg(command),
        None => cmd.arg("-l"),
    };
    cmd.current_dir(spec.cwd)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(slave.try_clone()?)
        .stdout(slave.try_clone()?)
        .stderr(slave)
        .kill_on_drop(false);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            cvt(libc::setsid())?;
            cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    // The command (and its copies of the slave end) is gone: the master reads EOF/EIO once the
    // child and everything it started have closed the terminal.
    drop(cmd);
    let pid = child.id().map(|p| p as i32).unwrap_or(0);

    let master = File::from(master);
    let mut reader = master.try_clone()?;
    let (tx, output) = mpsc::channel(256);
    std::thread::Builder::new().name("roll-pty".into()).spawn(move || {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // EIO: the other side closed.
                Err(_) => break,
            }
        }
    })?;
    Ok(Spawned { pty: Pty { master: Arc::new(Mutex::new(master)), pid }, output, child })
}

impl Pty {
    /// Writes input (blocking until the terminal takes it; call off the async runtime).
    pub fn write(&self, data: &[u8]) -> std::io::Result<()> {
        let mut master = self.master.lock().unwrap_or_else(|e| e.into_inner());
        master.write_all(data)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let ws = winsize(cols, rows);
        let master = self.master.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: TIOCSWINSZ on our PTY master with a valid winsize.
        cvt(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) })?;
        Ok(())
    }

    /// Signals the terminal's whole session (`SIGHUP`, as closing a terminal window does).
    pub fn hangup(&self) {
        self.signal(libc::SIGHUP);
    }

    pub fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    fn signal(&self, signal: libc::c_int) {
        if self.pid > 0 {
            // SAFETY: kill(2) on the process group the child leads (it called setsid).
            unsafe { libc::kill(-self.pid, signal) };
        }
    }

    pub fn clone_handle(&self) -> Pty {
        Pty { master: self.master.clone(), pid: self.pid }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_bytes() {
        let mut ring = Ring::default();
        ring.push(b"abc");
        assert_eq!(ring.bytes(), b"abc");
        ring.push(&vec![b'x'; SCROLLBACK]);
        ring.push(b"end");
        let bytes = ring.bytes();
        assert_eq!(bytes.len(), SCROLLBACK);
        assert!(bytes.ends_with(b"xend"));
        ring.push(&vec![b'y'; SCROLLBACK * 2]);
        assert_eq!(ring.bytes(), vec![b'y'; SCROLLBACK]);
    }
}
