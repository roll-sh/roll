//! What the test binaries share: where their databases live, and the fresh database each starts
//! from.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// A scratch directory for a test's databases: in memory where there's one (`/dev/shm`). Every
/// SQLite write fsyncs its WAL, which on a busy disk cost these tests most of their time, and
/// nothing here is meant to outlive a crash.
pub fn scratch() -> tempfile::TempDir {
    let shm = Path::new("/dev/shm");
    shm.is_dir().then(|| tempfile::tempdir_in(shm).ok()).flatten().unwrap_or_else(|| tempfile::tempdir().unwrap())
}

/// Copy a fresh secrets database to `to`, as `Secrets::new` leaves one on an empty file (the
/// schema and the audit log's key under the tests' keyring; not the issuer's key, which each test
/// still makes), made once per run: running every migration was most of each test's time. On
/// Postgres (`ROLL_CODE_TEST_DATABASE`) each test makes its own, as before.
pub fn fresh_secrets_db(to: &Path) {
    static TEMPLATE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    if std::env::var(code_sql::TEST_DATABASE_ENV).is_ok_and(|url| !url.is_empty()) {
        return;
    }
    let (_, file) = TEMPLATE.get_or_init(|| {
        let dir = scratch();
        let ws = Arc::new(roll_workspace::Workspace::new(code_sql::Db::open(&code_sql::Target::Sqlite(dir.path().join("workspace.db"))).unwrap()).unwrap());
        let live = dir.path().join("live.db");
        let opts = roll_secrets::Options { keyring: Ok(keyring()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: None, sts: None, outside: None, directory: None, github: None, gateways: None, clouds: None, workloads: None };
        let secrets = roll_secrets::Secrets::new(code_sql::Db::open(&code_sql::Target::Sqlite(live.clone())).unwrap(), ws, opts).unwrap();
        let file = dir.path().join("template.db");
        rusqlite::Connection::open(&live).unwrap().execute("VACUUM INTO ?1", [file.to_str().unwrap()]).unwrap();
        drop(secrets);
        (dir, file)
    });
    std::fs::copy(file, to).unwrap();
}

/// The tests' deployment key.
pub fn keyring() -> roll_secrets::Keyring {
    roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()
}
