//! The paging state: opaque to clients, self-contained so a query resumes on any node.
//!
//! Layout (version 1): `[1][kind][key][clustering][remaining][in_partition][key_index][groups]`
//! where byte strings are a u32 length and bytes, `clustering` may be absent (length
//! `u32::MAX`), counters are u64. A state that doesn't decode is rejected with the protocol error
//! drivers expect.

use bytes::{Bytes, BytesMut};
use nosql_storage::Clustering;

use crate::error::{Error, Result};

/// Where the next page starts.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PagingState {
    /// The partition the last row came from.
    pub key: Bytes,
    /// The last returned clustering in that partition (`None`: the whole partition was returned).
    pub clustering: Option<Clustering>,
    /// Rows still allowed by `LIMIT`.
    pub remaining: u64,
    /// Rows already returned from `key` (for `PER PARTITION LIMIT`).
    pub in_partition: u64,
    /// Position in the list of keys of an `IN` / `=` query.
    pub key_index: u64,
    /// Groups (or rows of a computed result) already returned.
    pub returned: u64,
    /// Where an index read resumes (an index read paged by entries), opaque to the core.
    pub index: Option<Bytes>,
}

const VERSION: u8 = 1;

impl PagingState {
    pub fn encode(&self) -> Bytes {
        let mut b = BytesMut::new();
        b.extend_from_slice(&[VERSION, 0]);
        b.extend_from_slice(&(self.key.len() as u32).to_be_bytes());
        b.extend_from_slice(&self.key);
        match &self.clustering {
            Some(c) => {
                b.extend_from_slice(&(c.encoded().len() as u32).to_be_bytes());
                b.extend_from_slice(c.encoded());
            }
            None => b.extend_from_slice(&u32::MAX.to_be_bytes()),
        }
        for v in [
            self.remaining,
            self.in_partition,
            self.key_index,
            self.returned,
        ] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        if let Some(ix) = &self.index {
            b.extend_from_slice(&(ix.len() as u32).to_be_bytes());
            b.extend_from_slice(ix);
        }
        b.freeze()
    }

    pub fn decode(raw: &[u8]) -> Result<PagingState> {
        let bad = || Error::protocol("Invalid value for the paging state");
        let mut r = raw;
        fn take<'a>(r: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            if r.len() < n {
                return None;
            }
            let (h, t) = r.split_at(n);
            *r = t;
            Some(h)
        }
        let u32_at = |r: &mut &[u8]| -> Result<u32> {
            take(r, 4)
                .and_then(|b| b.try_into().ok())
                .map(u32::from_be_bytes)
                .ok_or_else(bad)
        };
        let head = take(&mut r, 2).ok_or_else(bad)?;
        if head[0] != VERSION {
            return Err(bad());
        }
        let klen = u32_at(&mut r)? as usize;
        let key = Bytes::copy_from_slice(take(&mut r, klen).ok_or_else(bad)?);
        let clen = u32_at(&mut r)?;
        let clustering = if clen == u32::MAX {
            None
        } else {
            let raw = Bytes::copy_from_slice(take(&mut r, clen as usize).ok_or_else(bad)?);
            Some(Clustering::from_encoded(raw).map_err(|_| bad())?)
        };
        let mut nums = [0u64; 4];
        for n in &mut nums {
            let b = take(&mut r, 8).ok_or_else(bad)?;
            *n = u64::from_be_bytes(b.try_into().map_err(|_| bad())?);
        }
        let index = if r.is_empty() {
            None
        } else {
            let n = u32_at(&mut r)? as usize;
            Some(Bytes::copy_from_slice(take(&mut r, n).ok_or_else(bad)?))
        };
        if !r.is_empty() {
            return Err(bad());
        }
        Ok(PagingState {
            key,
            clustering,
            remaining: nums[0],
            in_partition: nums[1],
            key_index: nums[2],
            returned: nums[3],
            index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_garbage() {
        let s = PagingState {
            key: Bytes::from_static(b"k"),
            clustering: Some(Clustering::new([&b"a"[..], &b"bc"[..]])),
            remaining: 7,
            in_partition: 2,
            key_index: 3,
            returned: 9,
            index: None,
        };
        assert_eq!(PagingState::decode(&s.encode()).unwrap(), s);
        let t = PagingState {
            clustering: None,
            ..s.clone()
        };
        assert_eq!(PagingState::decode(&t.encode()).unwrap(), t);
        let u = PagingState {
            index: Some(Bytes::from_static(b"\x01\x02")),
            ..s.clone()
        };
        assert_eq!(PagingState::decode(&u.encode()).unwrap(), u);
        assert_eq!(
            PagingState::decode(b"\x07").unwrap_err().code,
            crate::ErrorCode::Protocol
        );
        let mut enc = s.encode().to_vec();
        enc.push(0);
        assert!(PagingState::decode(&enc).is_err());
        for n in 0..enc.len() - 1 {
            let _ = PagingState::decode(&enc[..n]);
        }
    }
}
