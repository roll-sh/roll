//! Write-ahead log objects (the fleet's acknowledgement path, `fleet.rs`): the batches a
//! stream's partitions appended since the previous object, as written (offsets assigned), framed
//! by partition. Records are never decoded; a frame is the partition and the batch's length.
//!
//! ```text
//! "RLWAL001" { topic id (16) | partition (i32) | length (u32) | record batch }*
//! ```

use bytes::{BufMut, Bytes, BytesMut};
use uuid::Uuid;

const MAGIC: &[u8; 8] = b"RLWAL001";
const FRAME: usize = 16 + 4 + 4;

/// `streams/<stream>/wal/<generation>-<seq>.wal`: the owner's tenure is in the name, so an
/// owner that lost its lease can never overwrite an object the next one wrote.
pub fn key(stream: &str, generation: i64, seq: i64) -> String {
    format!("streams/{stream}/wal/{generation:020}-{seq:020}.wal")
}

pub fn prefix(stream: &str) -> String {
    format!("streams/{stream}/wal/")
}

/// One object from the batches of each partition.
pub fn encode<'a>(parts: impl IntoIterator<Item = (Uuid, i32, &'a [Bytes])>) -> Bytes {
    let parts: Vec<_> = parts.into_iter().collect();
    let size = MAGIC.len() + parts.iter().flat_map(|(_, _, bs)| bs.iter()).map(|b| FRAME + b.len()).sum::<usize>();
    let mut out = BytesMut::with_capacity(size);
    out.put_slice(MAGIC);
    for (topic, partition, batches) in parts {
        for b in batches {
            out.put_slice(topic.as_bytes());
            out.put_i32(partition);
            out.put_u32(b.len() as u32);
            out.put_slice(b);
        }
    }
    out.freeze()
}

/// The frames of an object, in order, as slices of it. Lengths are checked against the bytes
/// present, never trusted.
pub fn decode(data: &Bytes) -> anyhow::Result<Vec<(Uuid, i32, Bytes)>> {
    anyhow::ensure!(data.len() >= MAGIC.len() && &data[..MAGIC.len()] == MAGIC, "not a write-ahead log object");
    let mut out = Vec::new();
    let mut pos = MAGIC.len();
    while pos < data.len() {
        anyhow::ensure!(data.len() - pos >= FRAME, "a torn write-ahead log frame at {pos}");
        let topic = Uuid::from_slice(&data[pos..pos + 16])?;
        let partition = i32::from_be_bytes(data[pos + 16..pos + 20].try_into().expect("4 bytes"));
        let len = u32::from_be_bytes(data[pos + 20..pos + 24].try_into().expect("4 bytes")) as usize;
        pos += FRAME;
        anyhow::ensure!(len <= data.len() - pos, "a write-ahead log frame of {len} bytes runs past the object");
        out.push((topic, partition, data.slice(pos..pos + len)));
        pos += len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip_and_lengths_are_bounded() {
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let x = [Bytes::from_static(b"one"), Bytes::from_static(b"two")];
        let y = [Bytes::from_static(b"three")];
        let data = encode([(a, 0, &x[..]), (b, 3, &y[..])]);
        let frames = decode(&data).unwrap();
        assert_eq!(frames, vec![(a, 0, x[0].clone()), (a, 0, x[1].clone()), (b, 3, y[0].clone())]);
        // A length that claims more than the object holds is refused, not allocated.
        let mut bad = BytesMut::from(&data[..]);
        let at = MAGIC.len() + 20;
        bad[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode(&bad.freeze()).is_err());
        assert!(decode(&data.slice(..data.len() - 1)).is_err());
        assert!(decode(&Bytes::from_static(b"nope")).is_err());
    }
}
