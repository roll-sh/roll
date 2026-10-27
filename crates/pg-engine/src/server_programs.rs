//! Server programs (`COPY ... FROM / TO PROGRAM`, file_fdw's `program` option), opt in.
//!
//! By default the engine refuses to run a program for every role, with PostgreSQL's error for
//! a role without `pg_execute_server_program`: a tenant running commands on the host would
//! own the machine. A server started with `--allow-server-programs`
//! (`EngineOptions::server_programs`) lets the roles PostgreSQL allows (superusers, members
//! of pg_execute_server_program) run them as PostgreSQL does: through `/bin/sh -c` (popen,
//! `OpenPipeStream`), in the directory standing for the data directory (the server files
//! directory, else the engine's data directory), with the server's environment, its stdin
//! the null device when the program's output is read. Failures are PostgreSQL's
//! (`ClosePipeFromProgram`, `ClosePipeToProgram`, `wait_result_to_str`).

use crate::error::{Error, Result, code};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

/// SIGPIPE.
const SIGPIPE: i32 = 13;

/// A program run by the server, its output read (COPY FROM) or its input written (COPY TO).
pub struct Pipe {
    /// None once closed.
    child: Option<Child>,
    out: Option<ChildStdout>,
    input: Option<std::io::BufWriter<ChildStdin>>,
    command: String,
    /// Its output was read to the end (CopyFromState's raw_reached_eof).
    eof: bool,
}

/// OpenPipeStream(command, "r").
pub fn open_read(dir: &Path, command: &str) -> Result<Pipe> {
    let mut child = shell(dir, command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| exec_error(&e, command))?;
    Ok(Pipe {
        out: child.stdout.take(),
        child: Some(child),
        input: None,
        command: command.to_string(),
        eof: false,
    })
}

/// OpenPipeStream(command, "w").
pub fn open_write(dir: &Path, command: &str) -> Result<Pipe> {
    let mut child = shell(dir, command)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| exec_error(&e, command))?;
    Ok(Pipe {
        input: child.stdin.take().map(std::io::BufWriter::new),
        child: Some(child),
        out: None,
        command: command.to_string(),
        eof: false,
    })
}

fn shell(dir: &Path, command: &str) -> Command {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(command).current_dir(dir);
    c
}

fn exec_error(e: &std::io::Error, command: &str) -> Error {
    crate::server_files::file_error(e, &format!("could not execute command \"{command}\""))
}

impl Pipe {
    /// The program's output, up to `buf.len()` bytes of it (0: its end).
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let Some(out) = &mut self.out else { return Ok(0) };
        let n = loop {
            match out.read(buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(crate::server_files::file_error(&e, "could not read from COPY file"));
                }
            }
        };
        if n == 0 {
            self.eof = true;
        }
        Ok(n)
    }

    /// Data for the program's input (CopySendEndOfRow): a program that stopped reading fails
    /// the COPY with its exit status, or else with the broken pipe.
    pub fn write(&mut self, d: &[u8]) -> Result<()> {
        let Some(w) = &mut self.input else { return Ok(()) };
        match w.write_all(d) {
            Ok(()) => Ok(()),
            Err(e) => Err(self.write_error(e)),
        }
    }

    fn write_error(&mut self, e: std::io::Error) -> Error {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            // The buffered data can't go anywhere now.
            if let Some(w) = self.input.take() {
                let _ = w.into_parts();
            }
            if let Err(failed) = self.close() {
                return failed;
            }
        }
        crate::server_files::file_error(&e, "could not write to COPY program")
    }

    /// ClosePipeFromProgram / ClosePipeToProgram: its input ended (and flushed) or its output
    /// closed, then its exit awaited. A program that didn't exit with 0 fails ("program ...
    /// failed"), except one ended by SIGPIPE before its output was read to the end.
    pub fn close(&mut self) -> Result<()> {
        if let Some(mut w) = self.input.take()
            && let Err(e) = w.flush()
        {
            return Err(self.write_error(e));
        }
        self.out = None;
        let Some(mut child) = self.child.take() else { return Ok(()) };
        let status = child.wait().map_err(|e| {
            crate::server_files::file_error(&e, "could not close pipe to external command")
        })?;
        if status.success() || (!self.eof && signal(status) == Some(SIGPIPE)) {
            return Ok(());
        }
        Err(Error::new(code::EXTERNAL_ROUTINE_EXCEPTION, format!("program \"{}\" failed", self.command))
            .detail(wait_result_to_str(status)))
    }
}

/// A pipe left open (the statement failed, or stopped reading early): the program is ended
/// and reaped, without waiting for it to finish.
impl Drop for Pipe {
    fn drop(&mut self) {
        // (Not flushed: the program may not be reading.)
        if let Some(w) = self.input.take() {
            let _ = w.into_parts();
        }
        self.out = None;
        if let Some(mut c) = self.child.take() {
            if !matches!(c.try_wait(), Ok(Some(_))) {
                let _ = c.kill();
            }
            let _ = c.wait();
        }
    }
}

fn signal(s: ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(&s)
}

/// wait_result_to_str: an exit status as PostgreSQL describes it.
pub fn wait_result_to_str(s: ExitStatus) -> String {
    if let Some(c) = s.code() {
        return match c {
            126 => "command not executable".into(),
            127 => "command not found".into(),
            c => format!("child process exited with exit code {c}"),
        };
    }
    match signal(s) {
        Some(n) => format!("child process was terminated by signal {n}: {}", strsignal(n)),
        None => format!(
            "child process exited with unrecognized status {}",
            std::os::unix::process::ExitStatusExt::into_raw(s)
        ),
    }
}

/// pg_strsignal: glibc's strsignal (Linux numbering).
fn strsignal(n: i32) -> String {
    const NAMES: [&str; 31] = [
        "Hangup",
        "Interrupt",
        "Quit",
        "Illegal instruction",
        "Trace/breakpoint trap",
        "Aborted",
        "Bus error",
        "Floating point exception",
        "Killed",
        "User defined signal 1",
        "Segmentation fault",
        "User defined signal 2",
        "Broken pipe",
        "Alarm clock",
        "Terminated",
        "Stack fault",
        "Child exited",
        "Continued",
        "Stopped (signal)",
        "Stopped",
        "Stopped (tty input)",
        "Stopped (tty output)",
        "Urgent I/O condition",
        "CPU time limit exceeded",
        "File size limit exceeded",
        "Virtual timer expired",
        "Profiling timer expired",
        "Window changed",
        "I/O possible",
        "Power failure",
        "Bad system call",
    ];
    match n {
        1..=31 => NAMES[n as usize - 1].into(),
        34..=64 => format!("Real-time signal {}", n - 34),
        _ => format!("Unknown signal {n}"),
    }
}
