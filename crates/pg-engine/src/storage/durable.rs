//! Changes to the files a crash must neither lose nor tear (WAL, SSTs, manifests, the
//! registry, database directories) go through here. They are plain `std::fs` calls; with the
//! `powerloss` feature and `ROLL_POWERLOSS` set, each is also recorded with what its fsyncs
//! made durable, so a test can cut the power (`powerloss::power_cut`) and keep only that.
//! Release builds have no such feature: these compile to the calls themselves.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

#[cfg(feature = "powerloss")]
use super::powerloss as pl;

/// A file written through here.
pub struct DFile {
    file: File,
    #[cfg(feature = "powerloss")]
    id: Option<u64>,
}

impl DFile {
    /// Create (or empty) a file to write.
    pub fn create(path: &Path) -> io::Result<DFile> {
        DFile::open(path, true)
    }

    /// Open a file to write, creating it if it is missing; what it holds stays.
    pub fn open_write(path: &Path) -> io::Result<DFile> {
        DFile::open(path, false)
    }

    fn open(path: &Path, truncate: bool) -> io::Result<DFile> {
        let open = || OpenOptions::new().write(true).create(true).truncate(truncate).open(path);
        #[cfg(feature = "powerloss")]
        {
            let (file, id) = pl::open(path, truncate, open)?;
            Ok(DFile { file, id })
        }
        #[cfg(not(feature = "powerloss"))]
        Ok(DFile { file: open()? })
    }

    pub fn write_all_at(&self, buf: &[u8], off: u64) -> io::Result<()> {
        self.file.write_all_at(buf, off)?;
        #[cfg(feature = "powerloss")]
        pl::wrote(self.id, off, buf);
        Ok(())
    }

    pub fn sync_all(&self) -> io::Result<()> {
        self.sync(|| self.file.sync_all())
    }

    /// fdatasync: the data, and the metadata needed to read it back (the length).
    pub fn sync_data(&self) -> io::Result<()> {
        self.sync(|| self.file.sync_data())
    }

    fn sync(&self, f: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        #[cfg(feature = "powerloss")]
        return pl::synced(self.id, f);
        #[cfg(not(feature = "powerloss"))]
        f()
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }
}

impl Write for DFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(feature = "powerloss")]
        let off = io::Seek::stream_position(&mut self.file)?;
        let n = self.file.write(buf)?;
        #[cfg(feature = "powerloss")]
        pl::wrote(self.id, off, &buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Wrap a namespace change for the record (`powerloss`), or just make it.
fn change<T>(_what: Change<'_>, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    #[cfg(feature = "powerloss")]
    return pl::change(_what, f);
    #[cfg(not(feature = "powerloss"))]
    f()
}

/// A change to a directory's entries.
#[allow(dead_code)]
pub(super) enum Change<'a> {
    Rename(&'a Path, &'a Path),
    Remove(&'a Path),
    MakeDirs(&'a Path),
}

/// fsync a file by its path.
pub fn sync_path(path: &Path) -> io::Result<()> {
    let f = || File::open(path)?.sync_all();
    #[cfg(feature = "powerloss")]
    return pl::synced(pl::id_of(path), f);
    #[cfg(not(feature = "powerloss"))]
    f()
}

/// fsync a directory: its entries (files created, renamed or removed in it) are durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    let f = || File::open(dir)?.sync_all();
    #[cfg(feature = "powerloss")]
    return pl::dir_synced(dir, f);
    #[cfg(not(feature = "powerloss"))]
    f()
}

pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    change(Change::Rename(from, to), || std::fs::rename(from, to))
}

pub fn remove_file(path: &Path) -> io::Result<()> {
    change(Change::Remove(path), || std::fs::remove_file(path))
}

pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    change(Change::Remove(path), || std::fs::remove_dir_all(path))
}

pub fn create_dir_all(path: &Path) -> io::Result<()> {
    change(Change::MakeDirs(path), || std::fs::create_dir_all(path))
}

/// `create_dir_all`, durably: each directory it makes is fsynced into its parent.
pub fn create_dir_all_synced(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_dir_all_synced(parent)?;
    }
    create_dir_all(path)?;
    path.parent().map_or(Ok(()), sync_dir)
}

/// Write a whole file (not synced).
pub fn write(path: &Path, data: &[u8]) -> io::Result<()> {
    DFile::create(path)?.write_all(data)
}

/// Replace `path` with `data` durably: written to `<path>.tmp`, fsynced, renamed over it,
/// and the directory fsynced. (Without `sync`, the same steps less the fsyncs.)
pub fn replace(path: &Path, data: &[u8], sync: bool) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);
    {
        let mut f = DFile::create(tmp)?;
        f.write_all(data)?;
        if sync {
            f.sync_all()?;
        }
    }
    rename(tmp, path)?;
    match path.parent() {
        Some(dir) if sync => sync_dir(dir),
        _ => Ok(()),
    }
}
