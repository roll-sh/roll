//! Where this machine keeps roll's files, and what it's called: the same on Linux and macOS as
//! before Windows builds, and Windows' own places there (`%APPDATA%`, `%USERPROFILE%`,
//! `%COMPUTERNAME%`).

use anyhow::Context;
use std::path::PathBuf;

fn var(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The user's home: `$HOME`, else (Windows) `%USERPROFILE%`.
pub fn home() -> Option<PathBuf> {
    let home = var("HOME");
    #[cfg(windows)]
    let home = home.or_else(|| var("USERPROFILE"));
    home
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
