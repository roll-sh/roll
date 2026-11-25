//! Where this machine keeps roll's files, and what it's called: the same on Linux and macOS as
//! before Windows builds, and Windows' own places there (`%APPDATA%`, `%USERPROFILE%`,
//! `%COMPUTERNAME%`).

use anyhow::Context;
use std::path::PathBuf;

fn var(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The user's home: `$HOME`.
#[cfg(not(windows))]
pub fn home() -> Option<PathBuf> {
    var("HOME")
}

/// The user's home: `%HOME%` (Git for Windows sets it), else `%USERPROFILE%`.
#[cfg(windows)]
pub fn home() -> Option<PathBuf> {
    var("HOME").or_else(|| var("USERPROFILE"))
}

/// The directory roll's config goes under (`<it>/roll/…`): `$XDG_CONFIG_HOME`, else (Windows)
/// `%APPDATA%`, else `~/.config`.
pub fn config_base() -> anyhow::Result<PathBuf> {
    if let Some(x) = var("XDG_CONFIG_HOME") {
        return Ok(x);
    }
    #[cfg(windows)]
    if let Some(a) = var("APPDATA") {
        return Ok(a);
    }
    Ok(home().context("HOME isn't set")?.join(".config"))
}

/// What to start for a command's name: as given, except that on Windows a bare name (`npm`) is
/// looked up with `%PATHEXT%` (`npm.cmd`), as a shell does; std's `Command` finds only `.exe`.
pub fn program(name: &str) -> PathBuf {
    #[cfg(windows)]
    if let Some(found) = on_path(name) {
        return found;
    }
    PathBuf::from(name)
}

#[cfg(windows)]
fn on_path(name: &str) -> Option<PathBuf> {
    let p = std::path::Path::new(name);
    if p.extension().is_some() || p.components().count() != 1 {
        return None;
    }
    let exts = std::env::var("PATHEXT").ok().filter(|e| !e.is_empty()).unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| exts.split(';').filter(|e| !e.is_empty()).map(|e| dir.join(format!("{name}{e}"))).find(|c| c.is_file()))
}

/// A command line for the shell: `sh -c` on Linux and macOS, `cmd /C` on Windows (the line as
/// written, unquoted).
#[cfg(windows)]
pub fn shell(line: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("cmd");
    c.arg("/C").raw_arg(line);
    c
}

/// This machine's name, if it has one.
pub fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length; the name is NUL-terminated within it.
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        std::str::from_utf8(&buf[..end]).ok().filter(|h| ok && !h.is_empty()).map(str::to_string)
    }
    #[cfg(not(unix))]
    std::env::var("COMPUTERNAME").ok().filter(|h| !h.is_empty())
}

/// The Windows console: what termios does on Linux and macOS.
#[cfg(windows)]
pub mod console {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetConsoleScreenBufferInfo, GetStdHandle, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    /// A console handle's mode, put back when dropped.
    struct Mode(HANDLE, CONSOLE_MODE);

    impl Mode {
        /// `which`'s mode with `on` set and `off` cleared; `None` when it isn't a console.
        fn change(which: STD_HANDLE, on: CONSOLE_MODE, off: CONSOLE_MODE) -> Option<Mode> {
            // SAFETY: GetStdHandle returns this process's handle (or none); GetConsoleMode writes
            // one u32; SetConsoleMode only reads its arguments.
            unsafe {
                let h = GetStdHandle(which);
                let mut was = 0;
                if h.is_null() || GetConsoleMode(h, &mut was) == 0 {
                    return None;
                }
                (SetConsoleMode(h, (was | on) & !off) != 0).then_some(Mode(h, was))
            }
        }
    }

    impl Drop for Mode {
        fn drop(&mut self) {
            // SAFETY: as in `change`, on the handle it read.
            unsafe { SetConsoleMode(self.0, self.1) };
        }
    }

    /// Typed input isn't echoed until dropped (then a newline is, as ECHONL would).
    pub struct EchoOff(#[allow(dead_code)] Mode);

    impl EchoOff {
        pub fn new() -> Option<EchoOff> {
            Mode::change(STD_INPUT_HANDLE, 0, ENABLE_ECHO_INPUT).map(EchoOff)
        }
    }

    impl Drop for EchoOff {
        fn drop(&mut self) {
            eprintln!();
        }
    }

    /// Columns and rows of the console window on stdout.
    pub fn size() -> Option<(u16, u16)> {
        // SAFETY: GetConsoleScreenBufferInfo fills the struct we pass.
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) == 0 {
                return None;
            }
            let w = info.srWindow;
            let (cols, rows) = (w.Right - w.Left + 1, w.Bottom - w.Top + 1);
            (cols > 0 && rows > 0).then_some((cols as u16, rows as u16))
        }
    }

    /// Keys go through as typed (Ctrl-C included, as escape sequences), and output's escape
    /// sequences are drawn, until dropped; nothing when stdin isn't a console.
    pub struct Raw(#[allow(dead_code)] Option<(Mode, Option<Mode>)>);

    impl Raw {
        pub fn enter() -> Raw {
            let Some(input) = Mode::change(STD_INPUT_HANDLE, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT) else { return Raw(None) };
            Raw(Some((input, Mode::change(STD_OUTPUT_HANDLE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0))))
        }
    }
}

/// Who may open a file on Windows: what 0600 and 0700 are on Linux and macOS.
#[cfg(windows)]
pub mod acl {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation, LookupAccountNameW, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE, READ_CONTROL, WRITE_DAC};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub(crate) fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain([0]).collect()
    }

    /// A SID as its string (`S-1-5-…`), freeing what Windows allocated.
    unsafe fn sid_string(sid: PSID) -> io::Result<String> {
        let mut s = std::ptr::null_mut();
        // SAFETY: the caller's SID is valid; ConvertSidToStringSidW allocates `s`, freed below.
        unsafe {
            if ConvertSidToStringSidW(sid, &mut s) == 0 {
                return Err(io::Error::last_os_error());
            }
            let len = (0..).take_while(|i| *s.add(*i) != 0).count();
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
            LocalFree(s.cast());
            Ok(out)
        }
    }

    /// `account`'s SID (a user, a group, or `NT SERVICE\<name>`) as a string.
    pub fn sid(account: &str) -> io::Result<String> {
        let name = wide(account);
        let (mut sid_len, mut dom_len, mut kind) = (0u32, 0u32, SID_NAME_USE::default());
        // SAFETY: the first call only reports the sizes; the second writes within them.
        unsafe {
            LookupAccountNameW(std::ptr::null(), name.as_ptr(), std::ptr::null_mut(), &mut sid_len, std::ptr::null_mut(), &mut dom_len, &mut kind);
            if sid_len == 0 {
                return Err(io::Error::other(format!("no account named {account} on this machine")));
            }
            let mut sid = vec![0u64; (sid_len as usize).div_ceil(8)];
            let mut dom = vec![0u16; dom_len as usize + 1];
            if LookupAccountNameW(std::ptr::null(), name.as_ptr(), sid.as_mut_ptr().cast(), &mut sid_len, dom.as_mut_ptr(), &mut dom_len, &mut kind) == 0 {
                return Err(io::Error::last_os_error());
            }
            sid_string(sid.as_mut_ptr().cast())
        }
    }

    /// The SID of the user this process runs as.
    pub fn me() -> io::Result<String> {
        // SAFETY: the token is ours and closed below; GetTokenInformation writes within `len`.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) != 0;
            let err = io::Error::last_os_error();
            CloseHandle(token);
            if !ok {
                return Err(err);
            }
            sid_string((*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid)
        }
    }

    /// Runs `f` with the DACL `sddl` describes (`D:P(A;;FA;;;BA)…`; `P`: nothing inherited).
    fn with_dacl(sddl: &str, f: impl FnOnce(*const ACL) -> u32) -> io::Result<()> {
        let text = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: Windows allocates `sd` (freed below); the DACL points into it while `f` runs.
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
                return Err(io::Error::last_os_error());
            }
            let (mut present, mut defaulted, mut dacl) = (0, 0, std::ptr::null_mut());
            let got = GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) != 0 && present != 0;
            let res = if got { f(dacl) } else { 87 };
            LocalFree(sd);
            if res != 0 { Err(io::Error::from_raw_os_error(res as i32)) } else { Ok(()) }
        }
    }

    const ONLY_THIS_DACL: u32 = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;

    /// Replaces an open file's DACL with `sddl`'s (the handle needs `WRITE_DAC`: [`exclusive`]).
    pub fn set(file: &std::fs::File, sddl: &str) -> io::Result<()> {
        // SAFETY: the handle is open for the call.
        with_dacl(sddl, |dacl| unsafe { SetSecurityInfo(file.as_raw_handle(), SE_FILE_OBJECT, ONLY_THIS_DACL, std::ptr::null_mut(), std::ptr::null_mut(), dacl, std::ptr::null()) })
    }

    /// Replaces a file's or directory's DACL with `sddl`'s.
    pub fn set_path(path: &Path, sddl: &str) -> io::Result<()> {
        let name = wide(path);
        // SAFETY: `name` is NUL-terminated and lives for the call.
        with_dacl(sddl, |dacl| unsafe { SetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT, ONLY_THIS_DACL, std::ptr::null_mut(), std::ptr::null_mut(), dacl, std::ptr::null()) })
    }

    /// For a file about to be made private: nobody else can open it while this handle is open
    /// (so not before [`set`] has run), and the handle may set its DACL.
    pub fn exclusive(o: &mut std::fs::OpenOptions) {
        use std::os::windows::fs::OpenOptionsExt;
        o.share_mode(0).access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL | WRITE_DAC);
    }

    /// Only this user and the machine's Administrators may open `file` (made with [`exclusive`]).
    pub fn private(file: &std::fs::File) -> io::Result<()> {
        set(file, &format!("D:P(A;;FA;;;{})(A;;FA;;;BA)", me()?))
    }
}
