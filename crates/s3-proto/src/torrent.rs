//! BitTorrent metainfo (`.torrent`) files, as GetObjectTorrent returns them: bencoded, with the
//! object's SHA-1 piece hashes.
//!
//! ```text
//! d [8:announce <url>] 4:info d 6:length i<size>e 4:name <name> 12:piece length i<n>e
//!                              6:pieces <20-byte SHA-1 per piece> 12:x-amz-bucket <b> 9:x-amz-key <k> e e
//! ```
//!
//! Keys are in sorted (raw byte) order, as bencoding requires.

use sha1::{Digest, Sha1};

/// The piece length S3 uses (256 KiB).
pub const PIECE_LENGTH: u64 = 256 * 1024;

/// SHA-1 of each `piece_length` piece of a stream of bytes, fed in any chunks.
pub struct PieceHasher {
    piece_length: u64,
    current: Sha1,
    filled: u64,
    pieces: Vec<u8>,
    total: u64,
}

impl PieceHasher {
    pub fn new(piece_length: u64) -> Self {
        assert!(piece_length > 0);
        Self { piece_length, current: Sha1::new(), filled: 0, pieces: Vec::new(), total: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        while !data.is_empty() {
            let room = (self.piece_length - self.filled) as usize;
            let n = room.min(data.len());
            self.current.update(&data[..n]);
            self.filled += n as u64;
            data = &data[n..];
            if self.filled == self.piece_length {
                let h = std::mem::replace(&mut self.current, Sha1::new()).finalize();
                self.pieces.extend_from_slice(&h);
                self.filled = 0;
            }
        }
    }

    /// The concatenated hashes (the last, shorter piece included) and the bytes seen.
    pub fn finish(mut self) -> (Vec<u8>, u64) {
        if self.filled > 0 {
            let h = self.current.finalize();
            self.pieces.extend_from_slice(&h);
        }
        (self.pieces, self.total)
    }
}

fn bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(b.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(b);
}

fn int(out: &mut Vec<u8>, n: u64) {
    out.push(b'i');
    out.extend_from_slice(n.to_string().as_bytes());
    out.push(b'e');
}

/// What goes into a metainfo file.
pub struct Metainfo<'a> {
    /// The tracker; `None` or empty: none (trackerless, DHT).
    pub announce: Option<&'a str>,
    /// The file name (the object key's last path segment).
    pub name: &'a str,
    pub length: u64,
    pub piece_length: u64,
    /// 20 bytes per piece.
    pub pieces: &'a [u8],
    pub bucket: &'a str,
    pub key: &'a str,
}

impl Metainfo<'_> {
    /// The bencoded file.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pieces.len() + 256 + self.key.len() * 2);
        out.push(b'd');
        if let Some(a) = self.announce.filter(|a| !a.is_empty()) {
            bytes(&mut out, b"announce");
            bytes(&mut out, a.as_bytes());
        }
        bytes(&mut out, b"info");
        out.extend_from_slice(&self.info());
        out.push(b'e');
        out
    }

    /// The bencoded `info` dictionary (its SHA-1 is the torrent's info hash).
    pub fn info(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pieces.len() + 128 + self.key.len() * 2);
        out.push(b'd');
        bytes(&mut out, b"length");
        int(&mut out, self.length);
        bytes(&mut out, b"name");
        bytes(&mut out, self.name.as_bytes());
        bytes(&mut out, b"piece length");
        int(&mut out, self.piece_length);
        bytes(&mut out, b"pieces");
        bytes(&mut out, self.pieces);
        bytes(&mut out, b"x-amz-bucket");
        bytes(&mut out, self.bucket.as_bytes());
        bytes(&mut out, b"x-amz-key");
        bytes(&mut out, self.key.as_bytes());
        out.push(b'e');
        out
    }
}

/// The file name a torrent of `key` gets: its last path segment (the whole key if that's empty).
pub fn file_name(key: &str) -> &str {
    key.rsplit('/').find(|s| !s.is_empty()).unwrap_or(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(data: &[u8], n: usize) -> Vec<u8> {
        data.chunks(n).flat_map(|c| Sha1::digest(c).to_vec()).collect()
    }

    #[test]
    fn pieces_whatever_the_chunks() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        for chunk in [1, 3, 64, 100, 999, 1000] {
            let mut h = PieceHasher::new(64);
            for c in data.chunks(chunk) {
                h.update(c);
            }
            assert_eq!(h.finish(), (reference(&data, 64), 1000), "chunks of {chunk}");
        }
        let (p, n) = PieceHasher::new(64).finish();
        assert!(p.is_empty() && n == 0, "an empty object has no pieces");
        let mut h = PieceHasher::new(10);
        h.update(&[0; 20]);
        assert_eq!(h.finish().0.len(), 40, "exact multiples have no empty last piece");
    }

    #[test]
    fn bencoding() {
        let m = Metainfo { announce: None, name: "a.txt", length: 3, piece_length: PIECE_LENGTH, pieces: &[0xAB; 20], bucket: "b", key: "dir/a.txt" };
        let mut want = b"d4:infod6:lengthi3e4:name5:a.txt12:piece lengthi262144e6:pieces20:".to_vec();
        want.extend_from_slice(&[0xAB; 20]);
        want.extend_from_slice(b"12:x-amz-bucket1:b9:x-amz-key9:dir/a.txtee");
        assert_eq!(m.encode(), want);
        let with = Metainfo { announce: Some("udp://t:1"), ..m };
        assert!(with.encode().starts_with(b"d8:announce9:udp://t:14:infod"));
        assert_eq!(file_name("dir/sub/a.mpg"), "a.mpg");
        assert_eq!(file_name("dir/"), "dir");
        assert_eq!(file_name("plain"), "plain");
    }
}
