//! `roll secrets agent --install-service` on Windows (docs/secrets.md, "A local agent", "On
//! Windows"): the agent as a service that starts with the machine, as a systemd unit is on Linux.
//!
//! The service runs as its own virtual account (`NT SERVICE\<name>`), never as LocalSystem. Its
//! files are under `%ProgramData%\roll\agent`, a directory only the Administrators and the
//! service may open: the request token file (`agent-token`, readable by the service and by the
//! accounts named with `--token-reader`), the service's login (`<name>.login`: `ROLL_TOKEN`, or a
//! workload identity's `ROLL_IDENTITY` and what it logs in with, readable by the service alone), a
//! machine identity's credential (`<name>.machine.json`) and its log (`<name>.log`, which, like every log of the agent's, holds no value). The service's
//! command line names the project, environment and flags, never a credential: the login is read
//! from its file before the flags read the environment ([`load_login`]).
//!
//! Under the service control manager the agent reports itself running once it listens, stops on
//! the manager's stop or the machine's shutdown, and reports a failure (which the service's
//! recovery restarts) when it ends with an error.

use crate::sys::{acl, quote};
use anyhow::{Context, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, ERROR_SERVICE_EXISTS, ERROR_SERVICE_NOT_ACTIVE, ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR};
use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, RegisterServiceCtrlHandlerExW, SC_ACTION, SC_ACTION_RESTART, SC_HANDLE,
    SC_MANAGER_ALL_ACCESS, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW,
};

/// The hidden flag `--install-service` puts on the service's command line, before its name.
pub const FLAG: &str = "--windows-service";

/// What the login file may set: the ways `roll secrets` logs in (a scoped token, or a workload
/// identity: an OIDC token file, a VM's metadata, a bare-metal machine's bootstrap token), nothing
/// else.
const LOGIN_KEYS: [&str; 9] = [
    "ROLL_TOKEN",
    "ROLL_IDENTITY",
    "ROLL_IDENTITY_KIND",
    "ROLL_OIDC_TOKEN_FILE",
    "ROLL_OIDC_AUDIENCE",
    "ROLL_AZURE_CLIENT_ID",
    "ROLL_BOOTSTRAP_TOKEN",
    "ROLL_BOOTSTRAP_TOKEN_FILE",
    "ROLL_MACHINE_CREDENTIAL_FILE",
];

/// `%ProgramData%\roll\agent`.
pub fn dir() -> PathBuf {
    std::env::var_os("ProgramData").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData")).join("roll").join("agent")
}

fn login_file(name: &str) -> PathBuf {
    dir().join(format!("{name}.login"))
}

/// A service name: what `sc.exe` and a file name both take.
fn check_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 80 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') || name.starts_with('.') {
        bail!("--service-name is letters, digits, '-', '_' and '.' (up to 80): {name:?} isn't");
    }
    Ok(())
}

/// In the service's own process (its command line has [`FLAG`] after `secrets … agent`): sets
/// the login from its file, before anything reads the environment. Elsewhere, nothing.
pub fn load_login() -> anyhow::Result<()> {
    let args: Vec<OsString> = std::env::args_os().collect();
    let Some(i) = args.iter().position(|a| a == FLAG) else { return Ok(()) };
    let before = &args[..i];
    if !(before.iter().any(|a| a == "secrets") && before.iter().any(|a| a == "agent")) {
        return Ok(());
    }
    let name = args.get(i + 1).and_then(|n| n.to_str()).context("--windows-service takes the service's name")?;
    check_name(name)?;
    let file = login_file(name);
    let text = std::fs::read_to_string(&file).with_context(|| format!("reading the service's login, {}", file.display()))?;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (k, v) = line.split_once('=').with_context(|| format!("{} isn't KEY=value lines", file.display()))?;
        if !LOGIN_KEYS.contains(&k) {
            bail!("{} sets {k}, which isn't a login", file.display());
        }
        // SAFETY: first thing in `main`, before any task reads the environment (and Windows'
        // environment is locked by the system anyway).
        unsafe { std::env::set_var(k, v) };
    }
    // A machine identity's credential stays with the service's other files.
    if std::env::var_os("ROLL_MACHINE_CREDENTIAL_FILE").is_none() {
        // SAFETY: as above.
        unsafe { std::env::set_var("ROLL_MACHINE_CREDENTIAL_FILE", dir().join(format!("{name}.machine.json"))) };
    }
    Ok(())
}

/// A handle from the service control manager, closed when dropped.
struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenSCManagerW, OpenServiceW or CreateServiceW.
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn manager() -> anyhow::Result<Sc> {
    // SAFETY: no machine or database name (this machine's); the handle is closed by `Sc`.
    let h = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS) };
    if h.is_null() {
        return Err(std::io::Error::last_os_error()).context("opening the service control manager (run this as an administrator)");
    }
    Ok(Sc(h))
}

/// What the service runs with.
pub struct Install<'a> {
    pub name: &'a str,
    pub api: &'a str,
    pub project: &'a str,
    pub env: Option<&'a str>,
    pub path: Option<&'a str>,
    pub port: u16,
    pub ttl: &'a str,
    pub cache_size: usize,
    pub prefetch: &'a [String],
    pub token_file: Option<&'a Path>,
    pub readers: &'a [String],
}

/// Writes `text` to a new `path` whose DACL is `sddl`'s from before its first byte.
fn write_new(path: &Path, sddl: &str, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    acl::exclusive(&mut o);
    let mut f = o.open(path).with_context(|| format!("creating {}", path.display()))?;
    acl::set(&f, sddl).with_context(|| format!("setting who may open {}", path.display()))?;
    f.write_all(text.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `--install-service`: the login from this environment, the files, the service (started).
pub fn install(i: Install<'_>) -> anyhow::Result<()> {
    check_name(i.name)?;
    let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let login: Vec<(&str, String)> = match (env("ROLL_TOKEN"), env("ROLL_IDENTITY")) {
        (Some(t), _) => vec![("ROLL_TOKEN", t)],
        (None, Some(_)) => LOGIN_KEYS[1..].iter().filter_map(|k| env(k).map(|v| (*k, v))).collect(),
        (None, None) => bail!("the service logs in as a scoped token (ROLL_TOKEN) or a workload identity (ROLL_IDENTITY with ROLL_OIDC_TOKEN_FILE): set one for this command, which keeps it for the service"),
    };
    if login.iter().any(|(_, v)| v.contains(['\r', '\n'])) {
        bail!("a login variable holds a line break");
    }
    let exe = std::env::current_exe().context("finding this roll.exe")?;
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|p| !p.is_empty())
        && exe.starts_with(&profile)
    {
        bail!(
            "{} is in your profile, which the service's account can't read: install roll for the machine (install.ps1 -System, `winget install --scope machine`, or `scoop install -g`) and run --install-service with that roll.exe",
            exe.display()
        );
    }
    let dir = dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let token_file = i.token_file.map(Path::to_path_buf).unwrap_or_else(|| dir.join("agent-token"));

    let mut cmd = vec![quote(&exe.to_string_lossy()), "--api".into(), quote(i.api), "secrets".into(), "-p".into(), quote(i.project)];
    if let Some(e) = i.env {
        cmd.extend(["-e".into(), quote(e)]);
    }
    if let Some(p) = i.path {
        cmd.extend(["--path".into(), quote(p)]);
    }
    cmd.extend(["agent".into(), FLAG.into(), i.name.into(), "--token-file".into(), quote(&token_file.to_string_lossy())]);
    cmd.extend(["--port".into(), i.port.to_string(), "--ttl".into(), quote(i.ttl), "--cache-size".into(), i.cache_size.to_string()]);
    if !i.prefetch.is_empty() {
        cmd.extend(["--prefetch".into(), quote(&i.prefetch.join(","))]);
    }
    let bin = acl::wide(cmd.join(" "));
    let account = format!(r"NT SERVICE\{}", i.name);
    let scm = manager()?;
    let (name_w, display_w, account_w) = (acl::wide(i.name), acl::wide("roll secrets agent"), acl::wide(&account));
    // SAFETY: every string is NUL-terminated and outlives the call; no password (a virtual
    // account has none).
    let svc = unsafe {
        CreateServiceW(
            scm.0,
            name_w.as_ptr(),
            display_w.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            bin.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            account_w.as_ptr(),
            std::ptr::null(),
        )
    };
    if svc.is_null() {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_SERVICE_EXISTS as i32) {
            bail!("there's already a service named {}: remove it with --uninstall-service, or pick another --service-name", i.name);
        }
        return Err(e).context("creating the service (run this as an administrator)");
    }
    let svc = Sc(svc);
    let made = (|| -> anyhow::Result<()> {
        let mut text = acl::wide("Serves roll secrets to apps on this machine from 127.0.0.1 (docs/secrets.md, \"A local agent\").");
        let desc = SERVICE_DESCRIPTIONW { lpDescription: text.as_mut_ptr() };
        // Restart after 5 s, every time, as Restart=always does; the count of failures resets
        // after a day.
        let mut actions = [SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5_000 }; 3];
        let failures = SERVICE_FAILURE_ACTIONSW { dwResetPeriod: 86_400, lpRebootMsg: std::ptr::null_mut(), lpCommand: std::ptr::null_mut(), cActions: 3, lpsaActions: actions.as_mut_ptr() };
        // SAFETY: both structs and what they point at live for the calls.
        unsafe {
            ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_DESCRIPTION, (&raw const desc).cast());
            ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_FAILURE_ACTIONS, (&raw const failures).cast());
        }
        // The account exists now that its service does.
        let me = acl::sid(&account).with_context(|| format!("looking up {account}"))?;
        let mut readers = vec![];
        for r in i.readers {
            readers.push(acl::sid(r).with_context(|| format!("--token-reader {r}"))?);
        }
        // The directory: the Administrators, and the service reading and writing (its log)
        // without changing who may open what.
        acl::set_path(&dir, &format!("D:P(A;OICI;FA;;;BA)(A;OICI;GRGW;;;{me})")).with_context(|| format!("setting who may open {}", dir.display()))?;
        let token_sddl = format!("D:P(A;;FA;;;BA)(A;;FR;;;{me}){}", readers.iter().map(|r| format!("(A;;FR;;;{r})")).collect::<String>());
        if token_file.exists() {
            // Kept (apps may hold it already); who may read it is set again.
            acl::set_path(&token_file, &token_sddl).with_context(|| format!("setting who may open {}", token_file.display()))?;
        } else {
            let mut b = [0u8; 32];
            getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("system randomness: {e}"))?;
            write_new(&token_file, &token_sddl, &format!("{}\n", b.iter().map(|x| format!("{x:02x}")).collect::<String>()))?;
        }
        let login_path = login_file(i.name);
        let _ = std::fs::remove_file(&login_path);
        let text: String = login.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        write_new(&login_path, &format!("D:P(A;;FA;;;BA)(A;;FR;;;{me})"), &text)?;
        Ok(())
    })();
    if let Err(e) = made {
        // SAFETY: the handle is the service just made.
        unsafe { DeleteService(svc.0) };
        return Err(e);
    }
    // SAFETY: as above; no arguments.
    if unsafe { StartServiceW(svc.0, 0, std::ptr::null()) } == 0 {
        let e = std::io::Error::last_os_error();
        bail!("installed {}, but it didn't start ({e}): see {}", i.name, dir.join(format!("{}.log", i.name)).display());
    }
    eprintln!("roll secrets agent: installed and started the service {} (as {account}), listening on 127.0.0.1:{}", i.name, i.port);
    eprintln!("  request token: {} (the Administrators and the service{}; --token-reader adds an app's account)", token_file.display(), if i.readers.is_empty() { String::new() } else { format!(", and {}", i.readers.join(", ")) });
    eprintln!("  log: {}", dir.join(format!("{}.log", i.name)).display());
    eprintln!("  stop: sc.exe stop {0} · remove: roll secrets agent --uninstall-service --service-name {0}", i.name);
    Ok(())
}

/// `--uninstall-service`: stops and removes the service, and deletes its login. The token file and
/// the log stay (apps may still hold the token).
pub fn uninstall(name: &str) -> anyhow::Result<()> {
    check_name(name)?;
    let scm = manager()?;
    let name_w = acl::wide(name);
    // SAFETY: the name is NUL-terminated; the handle is closed by `Sc`.
    let svc = unsafe { OpenServiceW(scm.0, name_w.as_ptr(), SERVICE_ALL_ACCESS) };
    if svc.is_null() {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("opening the service {name}"));
    }
    let svc = Sc(svc);
    // SAFETY: SERVICE_STATUS is plain integers.
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: `status` is ours to fill; a service that isn't running answers ERROR_SERVICE_NOT_ACTIVE.
    if unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_SERVICE_NOT_ACTIVE as i32) {
            eprintln!("roll secrets agent: stopping {name}: {e}");
        }
    }
    // SAFETY: as above.
    if unsafe { DeleteService(svc.0) } == 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("removing the service {name}"));
    }
    let login = login_file(name);
    match std::fs::remove_file(&login) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", login.display())),
    }
    eprintln!("roll secrets agent: removed the service {name} and its login; {} keeps the token file and the log", dir().display());
    Ok(())
}

/// The status handle `service_main` registered.
static STATUS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
/// The service manager asked the agent to stop.
static STOP: tokio::sync::Notify = tokio::sync::Notify::const_new();
static NAME: OnceLock<Vec<u16>> = OnceLock::new();
/// `service_main` ran (`Ok`) or the dispatcher couldn't start (`Err`).
static STARTED: Mutex<Option<tokio::sync::oneshot::Sender<Result<(), String>>>> = Mutex::new(None);

fn report(state: SERVICE_STATUS_CURRENT_STATE, exit: u32) {
    let accepts = if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepts,
        dwWin32ExitCode: if exit == 0 { NO_ERROR } else { ERROR_SERVICE_SPECIFIC_ERROR },
        dwServiceSpecificExitCode: exit,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING { 60_000 } else { 0 },
    };
    let h = STATUS.load(Ordering::Acquire);
    if !h.is_null() {
        // SAFETY: the handle is the one RegisterServiceCtrlHandlerExW returned; it needs no closing.
        unsafe { SetServiceStatus(h, &status) };
    }
}

unsafe extern "system" fn handler(control: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING, 0);
            STOP.notify_one();
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(_: u32, _: *mut windows_sys::core::PWSTR) {
    let name = NAME.get().map(|n| n.as_ptr()).unwrap_or(std::ptr::null());
    // SAFETY: `name` is NUL-terminated and static; the handler is a plain function.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name, Some(handler), std::ptr::null()) };
    let started = if h.is_null() { Err(std::io::Error::last_os_error().to_string()) } else { Ok(()) };
    STATUS.store(h, Ordering::Release);
    report(SERVICE_START_PENDING, 0);
    if let Some(tx) = STARTED.lock().ok().and_then(|mut s| s.take()) {
        let _ = tx.send(started);
    }
}

/// The agent under the service control manager: stderr to the service's log, and the manager
/// told it's starting.
pub struct Scm;

impl Scm {
    pub async fn start(name: &str) -> anyhow::Result<Scm> {
        check_name(name)?;
        let log = dir().join(format!("{name}.log"));
        let f = std::fs::OpenOptions::new().create(true).append(true).open(&log).with_context(|| format!("opening {}", log.display()))?;
        {
            use std::os::windows::io::IntoRawHandle;
            // SAFETY: the handle stays open for the process's life (it's never closed).
            unsafe { SetStdHandle(STD_ERROR_HANDLE, f.into_raw_handle()) };
        }
        let _ = NAME.set(acl::wide(name));
        let (tx, rx) = tokio::sync::oneshot::channel();
        *STARTED.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        std::thread::spawn(|| {
            let mut name = NAME.get().cloned().unwrap_or_default();
            let table = [SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) }, SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None }];
            // SAFETY: the table ends with the null entry; this returns once the service stopped.
            if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
                let e = std::io::Error::last_os_error().to_string();
                if let Some(tx) = STARTED.lock().ok().and_then(|mut s| s.take()) {
                    let _ = tx.send(Err(e));
                }
            }
        });
        match rx.await {
            Ok(Ok(())) => Ok(Scm),
            Ok(Err(e)) => bail!("{FLAG} is for the service control manager ({e}): run `roll secrets agent` without it"),
            Err(_) => bail!("the service control manager didn't start the service"),
        }
    }

    /// Listening.
    pub fn running(&self) {
        report(SERVICE_RUNNING, 0);
    }

    /// Resolves when the manager asks the agent to stop.
    pub async fn stopping() {
        STOP.notified().await;
    }

    /// The agent ended: stopped, or failed (which the service's recovery restarts).
    pub fn stopped(self, failed: bool) {
        report(SERVICE_STOPPED, failed as u32);
    }
}
