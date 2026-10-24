//! uuid-ossp (`contrib/uuid-ossp/uuid-ossp.c`, as built with `--with-uuid=e2fs`): the
//! namespace constants and version 1, 3, 4 and 5 UUIDs.

use super::native::Native;
use crate::Result;
use crate::sql::func::FnCtx;
use crate::types::Value;
use md5::Digest as _;

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(Native::scalar(match sym {
        "uuid_nil" => |_, _| Ok(Value::Uuid(0)),
        "uuid_ns_dns" => |_, _| Ok(Value::Uuid(0x6ba7b810_9dad_11d1_80b4_00c04fd430c8)),
        "uuid_ns_url" => |_, _| Ok(Value::Uuid(0x6ba7b811_9dad_11d1_80b4_00c04fd430c8)),
        "uuid_ns_oid" => |_, _| Ok(Value::Uuid(0x6ba7b812_9dad_11d1_80b4_00c04fd430c8)),
        "uuid_ns_x500" => |_, _| Ok(Value::Uuid(0x6ba7b814_9dad_11d1_80b4_00c04fd430c8)),
        "uuid_generate_v1" => v1,
        "uuid_generate_v1mc" => v1mc,
        "uuid_generate_v3" => v3,
        "uuid_generate_v4" => v4,
        "uuid_generate_v5" => v5,
        _ => return None,
    }))
}

/// libuuid's `uuid_generate_time` state: the last timestamp handed out (100 ns intervals
/// since 1582-10-15), the clock sequence and the node.
struct Clock {
    last: u64,
    seq: u16,
    node: [u8; 6],
}

fn clock(c: &FnCtx) -> std::sync::MutexGuard<'static, Option<Clock>> {
    static CLOCK: std::sync::Mutex<Option<Clock>> = std::sync::Mutex::new(None);
    let mut g = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let mut r = [0u8; 8];
        c.exec.random_bytes(&mut r);
        // Without a network interface's address libuuid makes up a node with the multicast bit
        // set.
        let mut node = [r[2], r[3], r[4], r[5], r[6], r[7]];
        node[0] |= 0x01;
        *g = Some(Clock {
            last: 0,
            seq: u16::from_be_bytes([r[0], r[1]]) & 0x3fff,
            node,
        });
    }
    g
}

/// A version 1 UUID of the next (strictly increasing) timestamp, with `node`.
fn time_uuid(c: &FnCtx, node: Option<[u8; 6]>) -> u128 {
    // 100 ns intervals between 1582-10-15 and 1970-01-01.
    const GREGORIAN: u64 = 0x01b2_1dd2_1381_4000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 / 100)
        .unwrap_or(0)
        + GREGORIAN;
    let mut g = clock(c);
    let st = g.as_mut().expect("clock state");
    let ts = if now > st.last { now } else { st.last + 1 };
    st.last = ts;
    let node = node.unwrap_or(st.node);
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&((ts & 0xffff_ffff) as u32).to_be_bytes());
    b[4..6].copy_from_slice(&(((ts >> 32) & 0xffff) as u16).to_be_bytes());
    b[6..8].copy_from_slice(&((((ts >> 48) & 0x0fff) as u16) | 0x1000).to_be_bytes());
    b[8..10].copy_from_slice(&(st.seq | 0x8000).to_be_bytes());
    b[10..16].copy_from_slice(&node);
    u128::from_be_bytes(b)
}

fn v1(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Uuid(time_uuid(c, None)))
}

/// A random node with the IEEE 802 multicast and local-admin bits set.
fn v1mc(_: &[Value], c: &FnCtx) -> Result<Value> {
    let mut node = [0u8; 6];
    c.exec.random_bytes(&mut node);
    node[0] |= 0x03;
    Ok(Value::Uuid(time_uuid(c, Some(node))))
}

/// A name-based UUID: the hash of the namespace's bytes and the name, with `version`.
fn named(hash: &[u8], version: u8) -> Value {
    let mut b = [0u8; 16];
    b.copy_from_slice(&hash[..16]);
    b[6] = (b[6] & 0x0f) | (version << 4);
    b[8] = (b[8] & 0x3f) | 0x80;
    Value::Uuid(u128::from_be_bytes(b))
}

fn ns_and_name(a: &[Value]) -> (u128, &str) {
    let ns = match &a[0] {
        Value::Uuid(u) => *u,
        _ => 0,
    };
    (ns, crate::sql::func::text_arg(&a[1]))
}

fn v3(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (ns, name) = ns_and_name(a);
    let mut h = md5::Md5::new();
    h.update(ns.to_be_bytes());
    h.update(name.as_bytes());
    Ok(named(&h.finalize(), 3))
}

fn v4(_: &[Value], c: &FnCtx) -> Result<Value> {
    let mut b = [0u8; 16];
    c.exec.random_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(Value::Uuid(u128::from_be_bytes(b)))
}

fn v5(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (ns, name) = ns_and_name(a);
    let mut h = sha1::Sha1::new();
    sha1::Digest::update(&mut h, ns.to_be_bytes());
    sha1::Digest::update(&mut h, name.as_bytes());
    Ok(named(&sha1::Digest::finalize(h), 5))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_based_vectors() {
        // RFC 4122 / PostgreSQL's expected output for www.widgets.com in the DNS namespace.
        let dns = 0x6ba7b810_9dad_11d1_80b4_00c04fd430c8u128;
        let mut h = md5::Md5::new();
        h.update(dns.to_be_bytes());
        h.update(b"www.widgets.com");
        let Value::Uuid(u) = named(&h.finalize(), 3) else { panic!() };
        assert_eq!(format!("{u:032x}"), "3d813cbb47fb32ba91df831e1593ac29");
        let mut h = sha1::Sha1::new();
        sha1::Digest::update(&mut h, dns.to_be_bytes());
        sha1::Digest::update(&mut h, b"www.widgets.com");
        let Value::Uuid(u) = named(&sha1::Digest::finalize(h), 5) else { panic!() };
        assert_eq!(format!("{u:032x}"), "21f7f8de80515b8986800195ef798b6a");
    }
}
