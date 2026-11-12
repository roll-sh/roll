//! The connections of a database, for ScyllaDB's `system.clients` and Cassandra's
//! `system_views.clients`.
//!
//! The server registers each native-protocol connection once it has sent STARTUP
//! ([`Database::register_client`]) and keeps the returned [`ClientHandle`] for the life of the
//! connection: it reports authentication, `USE`, requests and TLS on it, and dropping it removes
//! the row.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use crate::database::Database;
use crate::response::ClientInfo;

/// TLS details of a connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tls {
    /// `TLSv1.3`.
    pub protocol: String,
    /// `TLS_AES_256_GCM_SHA384`.
    pub cipher_suite: String,
}

/// One connection.
#[derive(Debug, Default)]
pub struct Conn {
    pub info: ClientInfo,
    /// The authenticated role (`None`: anonymous).
    pub user: Mutex<Option<String>>,
    pub keyspace: Mutex<Option<String>>,
    /// Past STARTUP and authentication.
    pub ready: AtomicBool,
    pub requests: AtomicU64,
    pub tls: Mutex<Option<Tls>>,
    /// ScyllaDB's shard of the connection.
    pub shard: AtomicU64,
}

/// The connections of a database.
#[derive(Default)]
pub struct Clients {
    next: AtomicU64,
    live: Mutex<BTreeMap<u64, Arc<Conn>>>,
}

impl Clients {
    /// The current connections, oldest first.
    pub fn snapshot(&self) -> Vec<Arc<Conn>> {
        self.live.lock().values().cloned().collect()
    }
}

/// A registered connection; dropping it unregisters the connection.
pub struct ClientHandle {
    db: Weak<Database>,
    id: u64,
    conn: Arc<Conn>,
}

impl Database {
    /// Registers a connection (after STARTUP). Its row shows in `system.clients` /
    /// `system_views.clients` until the handle is dropped.
    pub fn register_client(self: &Arc<Self>, info: ClientInfo) -> ClientHandle {
        let id = self.clients.next.fetch_add(1, Ordering::Relaxed);
        let conn = Arc::new(Conn {
            info,
            ..Default::default()
        });
        self.clients.live.lock().insert(id, conn.clone());
        ClientHandle {
            db: Arc::downgrade(self),
            id,
            conn,
        }
    }

    /// The connected clients.
    pub fn clients(&self) -> Vec<Arc<Conn>> {
        self.clients.snapshot()
    }
}

impl ClientHandle {
    /// Authentication finished (`None`: anonymous, `AllowAllAuthenticator`); the connection is
    /// ready.
    pub fn authenticated(&self, user: Option<&str>) {
        *self.conn.user.lock() = user.map(str::to_string);
        self.conn.ready.store(true, Ordering::Relaxed);
    }

    /// The connection is ready without authentication.
    pub fn ready(&self) {
        self.conn.ready.store(true, Ordering::Relaxed);
    }

    /// `USE ks` on the connection.
    pub fn set_keyspace(&self, keyspace: Option<&str>) {
        *self.conn.keyspace.lock() = keyspace.map(str::to_string);
    }

    /// A request was received (`request_count`).
    pub fn request(&self) {
        self.conn.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_tls(&self, tls: Option<Tls>) {
        *self.conn.tls.lock() = tls;
    }

    /// ScyllaDB's shard the connection is on (shard-aware drivers).
    pub fn set_shard(&self, shard: u32) {
        self.conn.shard.store(shard as u64, Ordering::Relaxed);
    }

    pub fn conn(&self) -> &Arc<Conn> {
        &self.conn
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        if let Some(db) = self.db.upgrade() {
            db.clients.live.lock().remove(&self.id);
        }
    }
}
