//! UUID helpers: name-based UUIDs (v3 as Cassandra's `UUID.nameUUIDFromBytes`, v5 for host ids),
//! time-based ids for tables, and text forms.

use md5::{Digest, Md5};
use sha1::Sha1;

pub use nosql_types::timeuuid::{format_uuid as format, parse_uuid as parse, version};

/// The namespace of roll's name-based UUIDs.
pub const NAMESPACE_ROLL: [u8; 16] = [
    0x6c, 0x0b, 0x4c, 0x52, 0x1f, 0x7e, 0x4d, 0x0e, 0x9a, 0x3b, 0x52, 0x6f, 0x6c, 0x6c, 0x6e, 0x71,
];

fn stamp(mut b: [u8; 16], version: u8) -> [u8; 16] {
    b[6] = (b[6] & 0x0f) | (version << 4);
    b[8] = (b[8] & 0x3f) | 0x80;
    b
}

/// Java's `UUID.nameUUIDFromBytes`: an MD5 (version 3) UUID of `name`, without a namespace.
pub fn v3(name: &[u8]) -> [u8; 16] {
    let d = Md5::digest(name);
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    stamp(b, 3)
}

/// An RFC 4122 version 5 (SHA-1) UUID of `name` in `namespace`.
pub fn v5(namespace: &[u8; 16], name: &[u8]) -> [u8; 16] {
    let mut h = <Sha1 as sha1::Digest>::new();
    sha1::Digest::update(&mut h, namespace);
    sha1::Digest::update(&mut h, name);
    let d = sha1::Digest::finalize(h);
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    stamp(b, 5)
}

/// The id of a built-in table (`TableId.forSystemTable`: `nameUUIDFromBytes(keyspace + table)`).
pub fn system_table_id(keyspace: &str, table: &str) -> [u8; 16] {
    let mut name = Vec::with_capacity(keyspace.len() + table.len());
    name.extend_from_slice(keyspace.as_bytes());
    name.extend_from_slice(table.as_bytes());
    v3(&name)
}

/// A fresh time-based UUID (new table ids and versions, as both references use).
pub fn timeuuid() -> [u8; 16] {
    nosql_types::timeuuid::now()
}

pub fn to_u128(u: &[u8; 16]) -> u128 {
    u128::from_be_bytes(*u)
}

pub fn from_u128(v: u128) -> [u8; 16] {
    v.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_table_ids_match_the_references() {
        // Both references report these ids in system_schema.tables.
        assert_eq!(
            format(&system_table_id("system", "local")),
            "7ad54392-bcdd-35a6-8417-4e047860b377"
        );
        assert_eq!(
            format(&system_table_id("system", "peers")),
            "37f71aca-7dc2-383b-a706-72528af04d4f"
        );
        assert_eq!(
            format(&system_table_id("system_schema", "tables")),
            "afddfb9d-bc1e-3068-8056-eed6c302ba09"
        );
        assert_eq!(
            format(&system_table_id("system", "size_estimates")),
            "618f817b-005f-3678-b8a4-53f3930b8e86"
        );
    }

    #[test]
    fn v5_and_round_trips() {
        let a = v5(&NAMESPACE_ROLL, b"db1");
        assert_eq!(version(&a), 5);
        assert_eq!(a, v5(&NAMESPACE_ROLL, b"db1"));
        assert_eq!(parse(&format(&a)), Some(a));
        assert_eq!(from_u128(to_u128(&a)), a);
        assert_eq!(version(&timeuuid()), 1);
    }
}
