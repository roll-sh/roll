//! JSON documents: the RedisJSON module's data type (`ReJSON-RL`), held, addressed and
//! serialized as RedisJSON 8.10 does it.

pub mod bson;
pub mod cbor;
pub mod de;
pub mod path;
pub mod ser;
pub mod value;

pub use value::{J, N, Node};

/// The module type's name and encoding version.
pub const TYPE_NAME: &str = "ReJSON-RL";
pub const ENCVER: u64 = 4;

/// The 64-bit id the RDB stores for the type: its 9-character name in 6-bit symbols, then the
/// encoding version in 10 bits (`moduleTypeEncodeId`).
pub const TYPE_ID: u64 = {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let name = TYPE_NAME.as_bytes();
    let mut id = 0u64;
    let mut i = 0;
    while i < name.len() {
        let mut p = 0;
        while CHARSET[p] != name[i] {
            p += 1;
        }
        id = (id << 6) | p as u64;
        i += 1;
    }
    (id << 10) | ENCVER
};

/// A document in the keyspace: its root value, and what the value allocates (measured again
/// only after the document was handed out for changing, so memory accounting stays cheap).
#[derive(Debug, Default)]
pub struct Json {
    root: J,
    mem: usize,
    stale: bool,
}

/// A copy (COPY, the module's copy callback) allocates its containers to fit: measured anew.
impl Clone for Json {
    fn clone(&self) -> Json {
        Json::new(self.root.clone())
    }
}

impl Json {
    pub fn new(root: J) -> Json {
        let mem = root.mem();
        Json { root, mem, stale: false }
    }

    pub fn root(&self) -> &J {
        &self.root
    }

    /// The root to change; the size is measured again on [`Json::refresh`] (done when the
    /// key's `KeyMut` is dropped).
    pub fn root_mut(&mut self) -> &mut J {
        self.stale = true;
        &mut self.root
    }

    pub fn refresh(&mut self) {
        if self.stale {
            self.mem = self.root.mem();
            self.stale = false;
        }
    }

    /// What the tree allocates (`IValue::mem_allocated`).
    pub fn mem(&self) -> usize {
        self.mem
    }

    /// The module's `mem_usage`: the tree and the root slot (`JSON.DEBUG MEMORY`, the value's
    /// part of `MEMORY USAGE`).
    pub fn usage(&self) -> usize {
        self.mem + 8
    }
}

