//! Replication slots (slot.c): the cluster's registry, with PostgreSQL's names, limits and
//! errors. Persistent slots are stored in `<data>/replslots.json` (written to a temporary
//! file and renamed, fsynced when a slot is created, dropped or altered), so they survive a
//! restart or crash; temporary slots live until their session ends.

use crate::error::{Error, Result, code};
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// ReplicationSlotPersistentData, as stored.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SlotData {
    pub name: String,
    /// The output plugin (None: a physical slot).
    pub plugin: Option<String>,
    /// The database a logical slot decodes (0: physical).
    pub database: u32,
    pub temporary: bool,
    pub two_phase: bool,
    /// Where decoding of prepared transactions at PREPARE started (0: not yet).
    pub two_phase_at: u64,
    pub failover: bool,
    /// What the slot still needs (0: no WAL reserved).
    pub restart_lsn: u64,
    /// Up to where the client confirmed it has the changes.
    pub confirmed_flush: u64,
    pub catalog_xmin: u64,
    pub xmin: u64,
    /// Why it was invalidated (ReplicationSlotInvalidationCause's name), if it was.
    #[serde(default)]
    pub invalidated: Option<String>,
    #[serde(default)]
    pub stats: SlotStats,
}

/// PgStat_StatReplSlotEntry: pg_stat_replication_slots' counters.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SlotStats {
    pub spill_txns: i64,
    pub spill_count: i64,
    pub spill_bytes: i64,
    pub stream_txns: i64,
    pub stream_count: i64,
    pub stream_bytes: i64,
    pub total_txns: i64,
    pub total_bytes: i64,
    /// When they were last reset (timestamptz microseconds; None: never).
    pub reset: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct Slot {
    pub data: SlotData,
    /// The backend using it (0: none).
    pub active_pid: i32,
    /// When it was last released (timestamptz microseconds).
    pub inactive_since: Option<i64>,
    /// Still being created (RS_EPHEMERAL): dropped if that fails.
    pub creating: bool,
    /// Its place in PostgreSQL's slot array (a new slot takes the first free one), the
    /// order the views list slots in.
    pub index: usize,
}

impl Slot {
    pub fn logical(&self) -> bool {
        self.data.plugin.is_some()
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    slots: Vec<SlotData>,
}

pub struct Slots {
    slots: Mutex<Vec<Slot>>,
    /// Signalled when a slot is released or dropped.
    released: Condvar,
    path: Option<PathBuf>,
    pub max: usize,
}

fn now() -> i64 {
    crate::types::datetime::Clock::system().now
}

/// ReplicationSlotValidateName.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::new(
            code::INVALID_NAME,
            format!("replication slot name \"{name}\" is too short"),
        ));
    }
    if name.len() >= 64 {
        return Err(Error::new(
            "42622",
            format!("replication slot name \"{name}\" is too long"),
        ));
    }
    if !name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_') {
        return Err(Error::new(
            code::INVALID_NAME,
            format!("replication slot name \"{name}\" contains invalid character"),
        )
        .hint("Replication slot names may only contain lower case letters, numbers, and the underscore character."));
    }
    Ok(())
}

pub fn no_slot(name: &str) -> Error {
    Error::new(
        code::UNDEFINED_OBJECT,
        format!("replication slot \"{name}\" does not exist"),
    )
}

fn active_error(name: &str, pid: i32) -> Error {
    Error::new(
        code::OBJECT_IN_USE,
        format!("replication slot \"{name}\" is active for PID {pid}"),
    )
}

impl Slots {
    /// The slots stored under `dir` (None: nothing is stored), at most `max`.
    pub fn open(dir: Option<PathBuf>, max: usize) -> Result<Slots> {
        let path = dir.map(|d| d.join("replslots.json"));
        let mut slots = Vec::new();
        if let Some(p) = &path
            && let Ok(b) = std::fs::read(p)
        {
            let s: Stored = serde_json::from_slice(&b)
                .map_err(|e| crate::error::internal(format!("replication slots file: {e}")))?;
            for (index, data) in s.slots.into_iter().enumerate() {
                slots.push(Slot { data, active_pid: 0, inactive_since: Some(now()), creating: false, index });
            }
        }
        Ok(Slots { slots: Mutex::new(slots), released: Condvar::new(), path, max })
    }

    fn save(&self, slots: &[Slot], sync: bool) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let stored = Stored {
            slots: slots
                .iter()
                .filter(|s| !s.data.temporary && !s.creating)
                .map(|s| s.data.clone())
                .collect(),
        };
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec(&stored).map_err(|e| crate::error::internal(e.to_string()))?;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            if sync {
                f.sync_all()?;
            }
        }
        std::fs::rename(&tmp, path)?;
        if sync && let Some(d) = path.parent() && let Ok(f) = std::fs::File::open(d) {
            let _ = f.sync_all();
        }
        Ok(())
    }

    /// Every slot, in creation order.
    pub fn list(&self) -> Vec<Slot> {
        let mut v: Vec<Slot> = self.slots.lock().iter().filter(|s| !s.creating).cloned().collect();
        v.sort_by_key(|s| s.index);
        v
    }

    pub fn get(&self, name: &str) -> Option<Slot> {
        self.slots.lock().iter().find(|s| s.data.name == name).cloned()
    }

    /// The logical slots of database `db`.
    pub fn logical_in(&self, db: u32) -> Vec<Slot> {
        self.slots
            .lock()
            .iter()
            .filter(|s| s.logical() && s.data.database == db)
            .cloned()
            .collect()
    }

    /// ReplicationSlotCreate: a new slot, acquired by `pid` and still being created
    /// (`finish_create` makes it permanent).
    pub fn create(&self, data: SlotData, pid: i32) -> Result<()> {
        validate_name(&data.name)?;
        if data.failover && data.temporary {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot enable failover for a temporary replication slot",
            ));
        }
        let mut slots = self.slots.lock();
        if slots.iter().any(|s| s.data.name == data.name) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("replication slot \"{}\" already exists", data.name),
            ));
        }
        if slots.len() >= self.max {
            return Err(Error::new(code::CONFIGURATION_LIMIT_EXCEEDED, "all replication slots are in use")
                .hint("Free one or increase \"max_replication_slots\"."));
        }
        let index = (0..).find(|i| !slots.iter().any(|s| s.index == *i)).unwrap_or(0);
        slots.push(Slot { data, active_pid: pid, inactive_since: None, creating: true, index });
        Ok(())
    }

    /// The slot being created is ready: its positions, persisted (ReplicationSlotPersist).
    pub fn finish_create(&self, name: &str, f: impl FnOnce(&mut SlotData)) -> Result<()> {
        let mut slots = self.slots.lock();
        let Some(s) = slots.iter_mut().find(|s| s.data.name == name) else {
            return Err(no_slot(name));
        };
        f(&mut s.data);
        s.creating = false;
        self.save(&slots, true)
    }

    /// Drop a slot whose creation failed.
    pub fn abort_create(&self, name: &str) {
        let mut slots = self.slots.lock();
        slots.retain(|s| !(s.data.name == name && s.creating));
        self.released.notify_all();
    }

    /// ReplicationSlotAcquire for backend `pid` (`wait`: until another backend releases it).
    pub fn acquire(&self, name: &str, pid: i32, wait: bool, cancel: &dyn Fn() -> Result<()>) -> Result<Slot> {
        let mut slots = self.slots.lock();
        loop {
            let Some(s) = slots.iter_mut().find(|s| s.data.name == name && !s.creating) else {
                return Err(no_slot(name));
            };
            if s.active_pid == 0 || s.active_pid == pid {
                s.active_pid = pid;
                s.inactive_since = None;
                return Ok(s.clone());
            }
            if !wait {
                return Err(active_error(name, s.active_pid));
            }
            self.released.wait_for(&mut slots, std::time::Duration::from_millis(100));
            drop(slots);
            cancel()?;
            slots = self.slots.lock();
        }
    }

    /// ReplicationSlotRelease: a temporary slot stays its session's.
    pub fn release(&self, name: &str, pid: i32) {
        let mut slots = self.slots.lock();
        if let Some(s) = slots.iter_mut().find(|s| s.data.name == name && s.active_pid == pid) {
            if !s.data.temporary {
                s.active_pid = 0;
                s.inactive_since = Some(now());
            }
            let _ = self.save(&slots, false);
        }
        self.released.notify_all();
    }

    /// Change a slot's data (`sync`: fsync the file now; else it is written, as Postgres
    /// marks it dirty for the next checkpoint).
    pub fn update<T>(&self, name: &str, sync: bool, f: impl FnOnce(&mut SlotData) -> T) -> Result<T> {
        let mut slots = self.slots.lock();
        let Some(s) = slots.iter_mut().find(|s| s.data.name == name) else {
            return Err(no_slot(name));
        };
        let r = f(&mut s.data);
        self.save(&slots, sync)?;
        Ok(r)
    }

    /// ReplicationSlotDrop (`wait`: for a backend using it to release it).
    pub fn drop_slot(&self, name: &str, pid: i32, wait: bool, cancel: &dyn Fn() -> Result<()>) -> Result<SlotData> {
        let mut slots = self.slots.lock();
        loop {
            let Some(i) = slots.iter().position(|s| s.data.name == name && !s.creating) else {
                return Err(no_slot(name));
            };
            let s = &slots[i];
            if s.active_pid == 0 || s.active_pid == pid {
                let s = slots.remove(i);
                self.save(&slots, true)?;
                self.released.notify_all();
                return Ok(s.data);
            }
            if !wait {
                return Err(active_error(name, s.active_pid));
            }
            self.released.wait_for(&mut slots, std::time::Duration::from_millis(100));
            drop(slots);
            cancel()?;
            slots = self.slots.lock();
        }
    }

    /// ReplicationSlotCleanup when backend `pid` ends: its temporary slots go, the others it
    /// has acquired are released. The temporary slots' databases are returned.
    pub fn backend_exit(&self, pid: i32) -> Vec<u32> {
        let mut slots = self.slots.lock();
        if !slots.iter().any(|s| s.active_pid == pid) {
            return Vec::new();
        }
        let mut dbs = Vec::new();
        slots.retain(|s| {
            let gone = s.active_pid == pid && (s.data.temporary || s.creating);
            if gone {
                dbs.push(s.data.database);
            }
            !gone
        });
        for s in slots.iter_mut().filter(|s| s.active_pid == pid) {
            s.active_pid = 0;
            s.inactive_since = Some(now());
        }
        let _ = self.save(&slots, false);
        self.released.notify_all();
        dbs
    }

    /// Persist every slot (shutdown).
    pub fn flush(&self) {
        let slots = self.slots.lock();
        let _ = self.save(&slots, true);
    }
}
