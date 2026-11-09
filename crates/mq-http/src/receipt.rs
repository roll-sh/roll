//! Receipts: opaque, HMAC-signed handles for messages handed out over HTTP. A receipt names
//! the vhost, queue, message position and delivery count, plus who holds the message: a
//! `receive` lease (with its deadline) or a streaming session (SSE).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use hmac::{Hmac, Mac};
use mq_core::meta::MetaStore;
use serde_json::json;
use sha2::Sha256;

const VERSION: u8 = 1;
const TAG_LEN: usize = 16;

/// Who holds the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    /// A `receive` lease; `deadline_ms` also identifies the lease.
    Lease { deadline_ms: u64 },
    /// A streaming consumer session (its channel id).
    Session { channel: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub vhost: String,
    pub queue: String,
    pub seq: u64,
    pub delivery_count: u32,
    pub holder: Holder,
}

fn mac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

impl Receipt {
    pub fn encode(&self, key: &[u8]) -> String {
        let mut b = Vec::with_capacity(32 + self.vhost.len() + self.queue.len() + TAG_LEN);
        b.push(VERSION);
        let (kind, x) = match self.holder {
            Holder::Lease { deadline_ms } => (1u8, deadline_ms),
            Holder::Session { channel } => (2u8, channel),
        };
        b.push(kind);
        b.extend_from_slice(&self.seq.to_be_bytes());
        b.extend_from_slice(&self.delivery_count.to_be_bytes());
        b.extend_from_slice(&x.to_be_bytes());
        put_str(&mut b, &self.vhost);
        put_str(&mut b, &self.queue);
        let tag = mac(key, &b);
        b.extend_from_slice(&tag[..TAG_LEN]);
        B64URL.encode(b)
    }

    /// Verify and decode; `None` for anything forged, truncated or from another key.
    pub fn decode(key: &[u8], s: &str) -> Option<Receipt> {
        let b = B64URL.decode(s.trim()).ok()?;
        if b.len() < 2 + 8 + 4 + 8 + 4 + TAG_LEN {
            return None;
        }
        let (data, tag) = b.split_at(b.len() - TAG_LEN);
        let want = mac(key, data);
        if want[..TAG_LEN].iter().zip(tag).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
            return None;
        }
        let mut r = Reader(data);
        if r.u8()? != VERSION {
            return None;
        }
        let kind = r.u8()?;
        let seq = r.u64()?;
        let delivery_count = r.u32()?;
        let x = r.u64()?;
        let vhost = r.str()?;
        let queue = r.str()?;
        let holder = match kind {
            1 => Holder::Lease { deadline_ms: x },
            2 => Holder::Session { channel: x },
            _ => return None,
        };
        Some(Receipt { vhost, queue, seq, delivery_count, holder })
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_be_bytes(b.try_into().unwrap_or_default()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_be_bytes(b.try_into().unwrap_or_default()))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

/// The shared receipt key from the metadata store, created on first use. `cas` makes
/// concurrent first starts agree on one key.
pub async fn load_or_create_key(meta: &dyn MetaStore) -> anyhow::Result<Vec<u8>> {
    const KEY: &str = "receipt_secret";
    loop {
        if let Some(v) = meta.get(crate::kind::HTTP, KEY).await?
            && let Some(k) = v["key"].as_str().and_then(|s| hex::decode(s).ok())
            && !k.is_empty()
        {
            return Ok(k);
        }
        let key: Vec<u8> = (0..4).flat_map(|_| mq_core::rand_u64().to_le_bytes()).collect();
        if meta.cas(crate::kind::HTTP, KEY, None, &json!({"key": hex::encode(&key)})).await?.is_some() {
            return Ok(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Receipt {
        Receipt { vhost: "/".into(), queue: "orders".into(), seq: 42, delivery_count: 3, holder: Holder::Lease { deadline_ms: 1_700_000_000_000 } }
    }

    #[test]
    fn roundtrip_and_tamper() {
        let key = b"k1";
        let s = sample().encode(key);
        assert_eq!(Receipt::decode(key, &s), Some(sample()));
        assert_eq!(Receipt::decode(b"k2", &s), None, "other key");
        let mut raw = B64URL.decode(&s).unwrap();
        raw[3] ^= 1;
        assert_eq!(Receipt::decode(key, &B64URL.encode(&raw)), None, "tampered");
        assert_eq!(Receipt::decode(key, "garbage!"), None);
        assert_eq!(Receipt::decode(key, ""), None);
        let r = Receipt { holder: Holder::Session { channel: 7 }, ..sample() };
        assert_eq!(Receipt::decode(key, &r.encode(key)), Some(r));
    }

    #[tokio::test]
    async fn key_is_persisted() {
        let meta = mq_core::meta::MemMeta::new();
        let a = load_or_create_key(meta.as_ref()).await.unwrap();
        let b = load_or_create_key(meta.as_ref()).await.unwrap();
        assert_eq!(a.len(), 32);
        assert_eq!(a, b);
    }
}
