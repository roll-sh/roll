//! Object ids. Git objects (blobs, trees, commits) are addressed by their git SHA-1 so git
//! serving needs no translation; changesets by the SHA-256 of their canonical encoding.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha1::Digest;
use std::fmt;
use std::str::FromStr;

/// A git object id (SHA-1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub const ZERO: Oid = Oid([0; 20]);

    /// Id of a git object of `kind` with `data` (hashes the `<kind> <len>\0` header too).
    pub fn hash_object(kind: &str, data: &[u8]) -> Oid {
        let mut h = sha1::Sha1::new();
        h.update(kind.as_bytes());
        h.update(b" ");
        h.update(data.len().to_string().as_bytes());
        h.update([0]);
        h.update(data);
        Oid(h.finalize().into())
    }

    pub fn from_bytes(b: &[u8]) -> Option<Oid> {
        b.try_into().ok().map(Oid)
    }

    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 20]
    }
}

impl FromStr for Oid {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Oid> {
        let mut b = [0u8; 20];
        hex::decode_to_slice(s, &mut b).map_err(|_| anyhow::anyhow!("invalid git object id {s:?}"))?;
        Ok(Oid(b))
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", &self.hex()[..12])
    }
}

/// A changeset id: SHA-256 of the changeset's canonical encoding. Displayed as 64 hex chars;
/// `short()` gives the `cs_` + 12 hex form used in UIs, and any unique prefix resolves.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChangesetId(pub [u8; 32]);

impl ChangesetId {
    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn short(&self) -> String {
        format!("cs_{}", &self.hex()[..12])
    }

    pub fn from_bytes(b: &[u8]) -> Option<ChangesetId> {
        b.try_into().ok().map(ChangesetId)
    }
}

impl FromStr for ChangesetId {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<ChangesetId> {
        let s = s.strip_prefix("cs_").unwrap_or(s);
        let mut b = [0u8; 32];
        hex::decode_to_slice(s, &mut b).map_err(|_| anyhow::anyhow!("invalid changeset id {s:?}"))?;
        Ok(ChangesetId(b))
    }
}

impl fmt::Display for ChangesetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for ChangesetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.short())
    }
}

macro_rules! serde_hex {
    ($t:ty) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.hex())
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}
serde_hex!(Oid);
serde_hex!(ChangesetId);
