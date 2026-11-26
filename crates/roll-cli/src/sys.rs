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

/// One argument for a Windows command line (CommandLineToArgvW's rules).
#[cfg(any(windows, test))]
pub fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    // Backslashes are literal unless they come before a quote: then they're doubled, and the
    // quote escaped.
    let mut out = String::from('"');
    let mut slashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
        } else {
            out.push_str(&"\\".repeat(slashes));
        }
        out.push(c);
        slashes = 0;
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
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
    use anyhow::{Context, bail};
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GENERIC_ALL, GENERIC_WRITE, GetLastError, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorDacl, GetTokenInformation, LookupAccountNameW, LookupAccountSidW, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    };
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

    /// Runs `f` with the security descriptor `sddl` describes; `f` answers a Windows error code.
    fn with_sd(sddl: &str, f: impl FnOnce(PSECURITY_DESCRIPTOR) -> u32) -> io::Result<()> {
        let text = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: Windows allocates `sd` (freed below), valid while `f` runs.
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
                return Err(io::Error::last_os_error());
            }
            let res = f(sd);
            LocalFree(sd);
            if res != 0 { Err(io::Error::from_raw_os_error(res as i32)) } else { Ok(()) }
        }
    }

    /// Runs `f` with the DACL `sddl` describes (`D:P(A;;FA;;;BA)…`; `P`: nothing inherited).
    fn with_dacl(sddl: &str, f: impl FnOnce(*const ACL) -> u32) -> io::Result<()> {
        with_sd(sddl, |sd| {
            let (mut present, mut defaulted, mut dacl) = (0, 0, std::ptr::null_mut());
            // SAFETY: `sd` is valid; the DACL points into it.
            let got = unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } != 0 && present != 0;
            if got { f(dacl) } else { 87 }
        })
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

    /// Whose a directory that holds credentials may be ([`trusted_dir`]).
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Owners {
        /// The machine's (the service's files under `%ProgramData%`, its token's and its
        /// credential's directories): owned by the Administrators, SYSTEM or the administrator
        /// running this, and nobody else may add to it, take from it or change who may. A
        /// reparse point (a link or junction) is refused.
        Machine,
        /// This user's (`%APPDATA%\roll`): owned by this user, the Administrators or SYSTEM.
        User,
    }

    const ADMINS: &str = "S-1-5-32-544";
    const SYSTEM: &str = "S-1-5-18";

    /// A directory [`trusted_dir`] makes for the machine: owned by the Administrators, opened by
    /// them and SYSTEM, and with `OWNER RIGHTS` (`OW`) given nothing, so its owner keeps no
    /// implicit `WRITE_DAC`.
    pub const MACHINE_DIR: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x0;;;OW)";

    /// What lets someone add to a directory, take from it, or change who may.
    const CHANGES: u32 = FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL;

    /// Who may hold [`CHANGES`] on a machine's directory besides its owners: services (only an
    /// administrator makes one, `NT SERVICE\…`), `CREATOR OWNER` (whoever makes an entry, who
    /// needed the right to make it) and `OWNER RIGHTS` (the owner, checked itself).
    fn may_change(sid: &str, owners: &[&str]) -> bool {
        owners.contains(&sid) || sid.starts_with("S-1-5-80-") || sid == "S-1-3-0" || sid == "S-1-3-4"
    }

    /// `path` opened to read who owns it, never through a reparse point when `nofollow`.
    fn open_meta(path: &Path, nofollow: bool) -> io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if nofollow {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        std::fs::OpenOptions::new().access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).custom_flags(flags).open(path)
    }

    /// An account's name (`BUILTIN\Users`), else its SID.
    unsafe fn account(sid: PSID) -> String {
        let (mut name, mut dom) = ([0u16; 256], [0u16; 256]);
        let (mut name_len, mut dom_len, mut kind) = (256u32, 256u32, SID_NAME_USE::default());
        // SAFETY: the caller's SID is valid; both buffers are as long as said.
        unsafe {
            if LookupAccountSidW(std::ptr::null(), sid, name.as_mut_ptr(), &mut name_len, dom.as_mut_ptr(), &mut dom_len, &mut kind) != 0 {
                let (n, d) = (String::from_utf16_lossy(&name[..name_len as usize]), String::from_utf16_lossy(&dom[..dom_len as usize]));
                return if d.is_empty() { n } else { format!(r"{d}\{n}") };
            }
            sid_string(sid).unwrap_or_else(|_| "an account".into())
        }
    }

    /// The owner of what `f` opened, and (with `dacl`) the accounts other than `owners` that may
    /// change it.
    fn inspect(f: &std::fs::File, owners: &[&str], dacl: bool) -> io::Result<(String, String, Vec<String>)> {
        let (mut owner, mut list, mut sd): (PSID, *mut ACL, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        let what = OWNER_SECURITY_INFORMATION | if dacl { DACL_SECURITY_INFORMATION } else { 0 };
        // SAFETY: Windows allocates `sd` (freed below); `owner` and `list` point into it.
        unsafe {
            let e = GetSecurityInfo(f.as_raw_handle(), SE_FILE_OBJECT, what, &mut owner, std::ptr::null_mut(), &mut list, std::ptr::null_mut(), &mut sd);
            if e != 0 {
                return Err(io::Error::from_raw_os_error(e as i32));
            }
            let out = (|| {
                let (sid, name) = (sid_string(owner)?, account(owner));
                let mut others = vec![];
                if dacl && list.is_null() {
                    // No DACL at all: everyone may do anything.
                    others.push("Everyone".to_string());
                } else if dacl {
                    for i in 0..(*list).AceCount as u32 {
                        let mut ace = std::ptr::null_mut();
                        if GetAce(list, i, &mut ace) == 0 {
                            return Err(io::Error::last_os_error());
                        }
                        match (*ace.cast::<ACE_HEADER>()).AceType {
                            // Allowed, and allowed with a condition: same layout.
                            0 | 9 => {
                                let a = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                                let who: PSID = (&raw const a.SidStart).cast_mut().cast();
                                if a.Mask & CHANGES != 0 && !may_change(&sid_string(who)?, owners) {
                                    others.push(account(who));
                                }
                            }
                            // Denied: takes rights away.
                            1 | 6 | 10 | 12 => {}
                            t => others.push(format!("an access rule of type {t}")),
                        }
                    }
                }
                Ok((sid, name, others))
            })();
            LocalFree(sd);
            out
        }
    }

    /// `dir` made safe to keep credentials in, or refused: made when it doesn't exist (the
    /// machine's as [`MACHINE_DIR`]; its parent must exist), and refused when someone else owns it
    /// or (the machine's) may change it, or it's a reparse point. `true` when this made it.
    pub fn trusted_dir(dir: &Path, owners: Owners) -> anyhow::Result<bool> {
        let me = me().context("looking up who runs this")?;
        trusted_dir_owned_by(dir, owners, &[ADMINS, SYSTEM, &me])
    }

    pub(super) fn trusted_dir_owned_by(dir: &Path, owners: Owners, by: &[&str]) -> anyhow::Result<bool> {
        let machine = owners == Owners::Machine;
        let name = wide(dir);
        let res = if machine {
            with_sd(MACHINE_DIR, |sd| {
                let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
                // SAFETY: `name` is NUL-terminated; `sa` and its descriptor live for the call.
                if unsafe { CreateDirectoryW(name.as_ptr(), &sa) } == 0 { unsafe { GetLastError() } } else { 0 }
            })
        } else {
            // SAFETY: as above, with the parent's inherited access list.
            if unsafe { CreateDirectoryW(name.as_ptr(), std::ptr::null()) } == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
        };
        let made = match res {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) => false,
            Err(e) => return Err(e).with_context(|| format!("creating {}{}", dir.display(), if machine { " (run this as an administrator)" } else { "" })),
        };
        const AGAIN: &str = "remove it as an administrator and run this again";
        let f = open_meta(dir, machine).with_context(|| format!("reading who owns {}", dir.display()))?;
        let attrs = f.metadata().with_context(|| format!("reading {}", dir.display()))?.file_attributes();
        if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 || (machine && attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0) {
            bail!("{} isn't a directory (or it's a link): {}", dir.display(), AGAIN);
        }
        let (owner, owner_name, others) = inspect(&f, by, machine).with_context(|| format!("reading who owns {}", dir.display()))?;
        if !by.contains(&owner.as_str()) {
            bail!("someone else made {} ({owner_name}): {}", dir.display(), AGAIN);
        }
        if !others.is_empty() {
            bail!("someone else may change {} ({}): {}", dir.display(), others.join(", "), AGAIN);
        }
        Ok(made)
    }

    /// Whether `path` is a file (not a link) that the Administrators, SYSTEM or the administrator
    /// running this made; `false` too when it doesn't exist or can't be read.
    pub fn machine_file(path: &Path) -> io::Result<bool> {
        let f = match open_meta(path, true) {
            Ok(f) => f,
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied) => return Ok(false),
            Err(e) => return Err(e),
        };
        let attrs = f.metadata()?.file_attributes();
        if attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Ok(false);
        }
        let me = me()?;
        let (owner, _, _) = inspect(&f, &[], false)?;
        Ok([ADMINS, SYSTEM, me.as_str()].contains(&owner.as_str()))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_windows_service_command_line_keeps_each_argument_whole() {
        // CommandLineToArgvW's rules: a quoted argument's backslashes are literal unless they
        // come before a quote.
        assert_eq!(super::quote("production:/"), "production:/");
        assert_eq!(super::quote(r"C:\Program Files\roll\roll.exe"), r#""C:\Program Files\roll\roll.exe""#);
        assert_eq!(super::quote(r"C:\agent dir\"), r#""C:\agent dir\\""#);
        assert_eq!(super::quote(r#"a "b""#), r#""a \"b\"""#);
        assert_eq!(super::quote(""), r#""""#);
    }

    /// Runs only on Windows (`release-cli.yml`'s `windows-check`); Linux has no access lists.
    #[cfg(windows)]
    #[test]
    fn a_directory_someone_else_owns_or_may_change_isnt_trusted_for_the_machine() {
        use super::acl::{self, Owners};
        let dir = std::env::temp_dir().join(format!("roll-trusted-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let me = acl::me().unwrap();
        // The service directory's access list (with `OWNER RIGHTS` given nothing) applies, and
        // a directory this account made with it is trusted.
        acl::set_path(&dir, &format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{me})(A;;0x0;;;OW)")).unwrap();
        assert!(!acl::trusted_dir(&dir, Owners::Machine).unwrap());
        assert!(!acl::trusted_dir(&dir, Owners::User).unwrap());
        // Owned by someone other than the accounts trusted: refused, naming the directory.
        let err = format!("{:#}", acl::trusted_dir_owned_by(&dir, Owners::Machine, &["S-1-5-18"]).unwrap_err());
        assert!(err.starts_with(&format!("someone else made {}", dir.display())), "{err}");
        // Every user may add to it: refused for the machine, still this user's own.
        acl::set_path(&dir, &format!("D:P(A;OICI;FA;;;{me})(A;CI;0x6;;;BU)")).unwrap();
        let err = format!("{:#}", acl::trusted_dir(&dir, Owners::Machine).unwrap_err());
        assert!(err.starts_with(&format!("someone else may change {}", dir.display())), "{err}");
        assert!(acl::trusted_dir(&dir, Owners::User).is_ok());
        // A file this account made is the machine's to keep; a missing one isn't.
        std::fs::write(dir.join("agent-token"), "t").unwrap();
        assert!(acl::machine_file(&dir.join("agent-token")).unwrap());
        assert!(!acl::machine_file(&dir.join("missing")).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
