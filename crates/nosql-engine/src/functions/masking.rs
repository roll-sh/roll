//! Cassandra 5's data masking functions (`cql3.functions.masking`, Apache-2.0): `mask_null`
//! (`NullMaskingFunction`), `mask_default` (`DefaultMaskingFunction`, each type's
//! `AbstractType.getMaskedValue`), `mask_replace` (`ReplaceMaskingFunction`), `mask_inner` /
//! `mask_outer` (`PartialMaskingFunction`) and `mask_hash` (`HashMaskingFunction`, any digest the
//! JDK's default providers offer). They are plain functions callable in any query, and the
//! functions column masks apply ([`crate::masking`]). ScyllaDB has none of them.
//!
//! Like every Cassandra native function they run on null arguments: `mask_default(null)` is the
//! type's masked value, `mask_replace(null, x)` is `x`.

use std::sync::Arc;

use bytes::Bytes;
use nosql_types::CqlType;

use super::params::{INT, Param, ParamFactory, TEXT};
use super::util::arg;
use super::{FnCtx, FnName, Function, Registry};
use crate::error::{Error, Result};

/// The name prefix of every masking function (`MaskingFunction.NAME_PREFIX`).
pub const PREFIX: &str = "mask_";

pub fn register(r: &mut Registry) {
    if r.dialect().is_scylla() {
        return;
    }
    let any = || Param::Any {
        from_receiver: false,
    };
    r.add_factory(
        "mask_null",
        ParamFactory::new("mask_null", vec![any()], |n, t, _| {
            Ok(mask(n, t.to_vec(), t[0].clone(), Kind::Null))
        }),
    );
    r.add_factory(
        "mask_default",
        ParamFactory::new("mask_default", vec![any()], |n, t, _| {
            Ok(mask(n, t.to_vec(), t[0].clone(), Kind::Default))
        }),
    );
    r.add_factory(
        "mask_replace",
        ParamFactory::new(
            "mask_replace",
            vec![
                Param::Any {
                    from_receiver: true,
                },
                Param::SameAs {
                    index: 0,
                    prefer_other: true,
                    inner: Box::new(Param::Any {
                        from_receiver: true,
                    }),
                },
            ],
            |n, t, _| Ok(mask(n, t.to_vec(), t[1].clone(), Kind::Replace)),
        ),
    );
    r.add_factory(
        "mask_hash",
        ParamFactory::new(
            "mask_hash",
            vec![any(), Param::Fixed(TEXT).optional()],
            |n, t, _| {
                let mut args = vec![t[0].clone()];
                if t.len() == 2 {
                    args.push(CqlType::Text);
                }
                Ok(mask(n, args, CqlType::Blob, Kind::Hash))
            },
        ),
    );
    for (name, kind) in [("mask_inner", Kind::Inner), ("mask_outer", Kind::Outer)] {
        let build: super::params::Build = match kind {
            Kind::Inner => |n, t, _| Ok(partial(n, t, Kind::Inner)),
            _ => |n, t, _| Ok(partial(n, t, Kind::Outer)),
        };
        r.add_factory(
            name,
            ParamFactory::new(
                name,
                vec![
                    Param::Str,
                    Param::Fixed(INT),
                    Param::Fixed(INT),
                    Param::Fixed(TEXT).optional(),
                ],
                build,
            ),
        );
    }
}

fn partial(name: &str, t: &[CqlType], kind: Kind) -> Arc<dyn Function> {
    let mut args = vec![t[0].clone(), CqlType::Int, CqlType::Int];
    if t.len() == 4 {
        args.push(CqlType::Text);
    }
    mask(name, args, t[0].clone(), kind)
}

fn mask(name: &str, args: Vec<CqlType>, ret: CqlType, kind: Kind) -> Arc<dyn Function> {
    Arc::new(MaskFn {
        name: FnName::system(name),
        args,
        ret,
        kind,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Null,
    Default,
    Replace,
    Inner,
    Outer,
    Hash,
}

#[derive(Debug)]
pub struct MaskFn {
    name: FnName,
    args: Vec<CqlType>,
    ret: CqlType,
    kind: Kind,
}

impl Function for MaskFn {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.ret
    }
    fn execute(&self, _: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        match self.kind {
            Kind::Null => Ok(None),
            Kind::Default => masked_value(&self.args[0]).map(Some),
            Kind::Replace => Ok(arg(args, 1).cloned()),
            Kind::Hash => {
                let algorithm = match arg(args, 1) {
                    None => "SHA-256".to_string(),
                    Some(b) => String::from_utf8_lossy(b).into_owned(),
                };
                let Some(digest) = Digest::named(&algorithm) else {
                    return Err(Error::invalid(format!(
                        "Hash algorithm not found: {algorithm}"
                    )));
                };
                Ok(arg(args, 0).map(|v| Bytes::from(digest.hash(v))))
            }
            Kind::Inner | Kind::Outer => {
                let padding = match arg(args, 3) {
                    None => '*' as u16,
                    Some(b) => {
                        let s = String::from_utf8_lossy(b);
                        let units: Vec<u16> = s.encode_utf16().collect();
                        if units.len() != 1 {
                            return Err(Error::invalid(format!(
                                "The padding argument for function {} should be single-character, but '{s}' has {} characters.",
                                self.name,
                                units.len()
                            )));
                        }
                        units[0]
                    }
                };
                let Some(v) = arg(args, 0) else {
                    return Ok(None);
                };
                let int = |i: usize| -> i32 {
                    arg(args, i)
                        .and_then(|b| <[u8; 4]>::try_from(b.as_ref()).ok())
                        .map(i32::from_be_bytes)
                        .unwrap_or(0)
                };
                let text = String::from_utf8_lossy(v);
                Ok(Some(Bytes::from(partial_mask(
                    &text,
                    int(1),
                    int(2),
                    padding,
                    self.kind == Kind::Inner,
                ))))
            }
        }
    }
}

/// `PartialMaskingFunction.Kind.mask` on UTF-16 code units, as Java strings are.
pub fn partial_mask(value: &str, begin: i32, end: i32, padding: u16, inner: bool) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.is_empty() {
        return String::new();
    }
    let size = units.len() as i64;
    let (begin, end_index) = (begin as i64, size - 1 - end as i64);
    let out: Vec<u16> = units
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let i = i as i64;
            let masked = if inner {
                i >= begin && i <= end_index
            } else {
                i < begin || i > end_index
            };
            if masked { padding } else { *u }
        })
        .collect();
    // Java encodes a lone surrogate as '?'.
    char::decode_utf16(out).map(|c| c.unwrap_or('?')).collect()
}

/// `AbstractType.getMaskedValue`: `****` for text, zero / false / the epoch for the others,
/// empty collections, and tuples, UDTs and vectors of their elements' masked values.
pub fn masked_value(ty: &CqlType) -> Result<Bytes> {
    let zero = |n: usize| Bytes::from(vec![0u8; n]);
    Ok(match ty {
        CqlType::Ascii | CqlType::Text => Bytes::from_static(b"****"),
        CqlType::Bigint | CqlType::Counter | CqlType::Time | CqlType::Timestamp => zero(8),
        CqlType::Blob | CqlType::Empty => Bytes::new(),
        CqlType::Boolean | CqlType::Tinyint => zero(1),
        CqlType::Date => Bytes::copy_from_slice(&(1u32 << 31).to_be_bytes()),
        CqlType::Decimal => zero(5),
        CqlType::Double => zero(8),
        CqlType::Duration => zero(3),
        CqlType::Float | CqlType::Int | CqlType::Inet => zero(4),
        CqlType::Smallint => zero(2),
        CqlType::Varint => zero(1),
        CqlType::Uuid => zero(16),
        CqlType::Timeuuid => Bytes::copy_from_slice(&nosql_types::timeuuid::min_timeuuid(0)),
        CqlType::List { .. } | CqlType::Set { .. } | CqlType::Map { .. } => zero(4),
        CqlType::Tuple(fields) => {
            let vals = fields
                .iter()
                .map(masked_value)
                .collect::<Result<Vec<_>>>()?;
            nosql_types::pack_fields(vals.iter().map(|v| Some(v.as_ref())))
        }
        CqlType::Udt(def, _) => {
            let vals = def
                .fields
                .iter()
                .map(|(_, t)| masked_value(t))
                .collect::<Result<Vec<_>>>()?;
            nosql_types::pack_fields(vals.iter().map(|v| Some(v.as_ref())))
        }
        CqlType::Vector { elem, dim } => {
            let v = masked_value(elem)?;
            let decoded = nosql_types::decode(elem, &v).map_err(|e| Error::invalid(e.message))?;
            let all = nosql_types::Value::Vector(vec![decoded; *dim as usize]);
            nosql_types::encode(&all, ty)
        }
        CqlType::Custom(c) => {
            return Err(Error::server(format!(
                "java.lang.UnsupportedOperationException: There isn't a defined masked value for type '{c}'"
            )));
        }
    })
}

/// The JDK's `MessageDigest` algorithms (names case-insensitive, with the SUN provider's
/// aliases and OIDs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Digest {
    Md2,
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha512_224,
    Sha512_256,
    Sha3(usize),
}

impl Digest {
    fn named(name: &str) -> Option<Digest> {
        let n = name.to_ascii_uppercase();
        let n = n.strip_prefix("OID.").unwrap_or(&n);
        Some(match n {
            "MD2" => Digest::Md2,
            "MD5" => Digest::Md5,
            "SHA-1" | "SHA" | "SHA1" | "1.3.14.3.2.26" => Digest::Sha1,
            "SHA-224" | "SHA224" | "2.16.840.1.101.3.4.2.4" => Digest::Sha224,
            "SHA-256" | "SHA256" | "2.16.840.1.101.3.4.2.1" => Digest::Sha256,
            "SHA-384" | "SHA384" | "2.16.840.1.101.3.4.2.2" => Digest::Sha384,
            "SHA-512" | "SHA512" | "2.16.840.1.101.3.4.2.3" => Digest::Sha512,
            "SHA-512/224" | "SHA512/224" | "2.16.840.1.101.3.4.2.5" => Digest::Sha512_224,
            "SHA-512/256" | "SHA512/256" | "2.16.840.1.101.3.4.2.6" => Digest::Sha512_256,
            "SHA3-224" | "2.16.840.1.101.3.4.2.7" => Digest::Sha3(224),
            "SHA3-256" | "2.16.840.1.101.3.4.2.8" => Digest::Sha3(256),
            "SHA3-384" | "2.16.840.1.101.3.4.2.9" => Digest::Sha3(384),
            "SHA3-512" | "2.16.840.1.101.3.4.2.10" => Digest::Sha3(512),
            _ => return None,
        })
    }

    fn hash(self, v: &[u8]) -> Vec<u8> {
        use sha2::Digest as _;
        match self {
            Digest::Md2 => md2(v).to_vec(),
            Digest::Md5 => <md5::Md5 as md5::Digest>::digest(v).to_vec(),
            Digest::Sha1 => sha1::Sha1::digest(v).to_vec(),
            Digest::Sha224 => sha2::Sha224::digest(v).to_vec(),
            Digest::Sha256 => sha2::Sha256::digest(v).to_vec(),
            Digest::Sha384 => sha2::Sha384::digest(v).to_vec(),
            Digest::Sha512 => sha2::Sha512::digest(v).to_vec(),
            Digest::Sha512_224 => sha2::Sha512_224::digest(v).to_vec(),
            Digest::Sha512_256 => sha2::Sha512_256::digest(v).to_vec(),
            Digest::Sha3(bits) => sha3(v, bits),
        }
    }
}

/// MD2 (RFC 1319).
fn md2(input: &[u8]) -> [u8; 16] {
    const S: [u8; 256] = [
        41, 46, 67, 201, 162, 216, 124, 1, 61, 54, 84, 161, 236, 240, 6, 19, 98, 167, 5, 243, 192,
        199, 115, 140, 152, 147, 43, 217, 188, 76, 130, 202, 30, 155, 87, 60, 253, 212, 224, 22,
        103, 66, 111, 24, 138, 23, 229, 18, 190, 78, 196, 214, 218, 158, 222, 73, 160, 251, 245,
        142, 187, 47, 238, 122, 169, 104, 121, 145, 21, 178, 7, 63, 148, 194, 16, 137, 11, 34, 95,
        33, 128, 127, 93, 154, 90, 144, 50, 39, 53, 62, 204, 231, 191, 247, 151, 3, 255, 25, 48,
        179, 72, 165, 181, 209, 215, 94, 146, 42, 172, 86, 170, 198, 79, 184, 56, 210, 150, 164,
        125, 182, 118, 252, 107, 226, 156, 116, 4, 241, 69, 157, 112, 89, 100, 113, 135, 32, 134,
        91, 207, 101, 230, 45, 168, 2, 27, 96, 37, 173, 174, 176, 185, 246, 28, 70, 97, 105, 52,
        64, 126, 15, 85, 71, 163, 35, 221, 81, 175, 58, 195, 92, 249, 206, 186, 197, 234, 38, 44,
        83, 13, 110, 133, 40, 132, 9, 211, 223, 205, 244, 65, 129, 77, 82, 106, 220, 55, 200, 108,
        193, 171, 250, 36, 225, 123, 8, 12, 189, 177, 74, 120, 136, 149, 139, 227, 99, 232, 109,
        233, 203, 213, 254, 59, 0, 29, 57, 242, 239, 183, 14, 102, 88, 208, 228, 166, 119, 114,
        248, 235, 117, 75, 10, 49, 68, 80, 180, 143, 237, 31, 26, 219, 153, 141, 51, 159, 17, 131,
        20,
    ];
    let pad = 16 - input.len() % 16;
    let mut msg = input.to_vec();
    msg.extend(std::iter::repeat_n(pad as u8, pad));
    let mut checksum = [0u8; 16];
    let mut l = 0u8;
    for block in msg.chunks(16) {
        for j in 0..16 {
            checksum[j] ^= S[(block[j] ^ l) as usize];
            l = checksum[j];
        }
    }
    msg.extend_from_slice(&checksum);
    let mut x = [0u8; 48];
    for block in msg.chunks(16) {
        for j in 0..16 {
            x[16 + j] = block[j];
            x[32 + j] = x[16 + j] ^ x[j];
        }
        let mut t = 0u8;
        for round in 0..18u8 {
            for b in x.iter_mut() {
                *b ^= S[t as usize];
                t = *b;
            }
            t = t.wrapping_add(round);
        }
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&x[..16]);
    out
}

/// SHA-3 (FIPS 202) with a `bits`-bit output.
fn sha3(input: &[u8], bits: usize) -> Vec<u8> {
    const RC: [u64; 24] = [
        0x0000000000000001,
        0x0000000000008082,
        0x800000000000808a,
        0x8000000080008000,
        0x000000000000808b,
        0x0000000080000001,
        0x8000000080008081,
        0x8000000000008009,
        0x000000000000008a,
        0x0000000000000088,
        0x0000000080008009,
        0x000000008000000a,
        0x000000008000808b,
        0x800000000000008b,
        0x8000000000008089,
        0x8000000000008003,
        0x8000000000008002,
        0x8000000000000080,
        0x000000000000800a,
        0x800000008000000a,
        0x8000000080008081,
        0x8000000000008080,
        0x0000000080000001,
        0x8000000080008008,
    ];
    const ROT: [u32; 25] = [
        0, 1, 62, 28, 27, 36, 44, 6, 55, 20, 3, 10, 43, 25, 39, 41, 45, 15, 21, 8, 18, 2, 61, 56,
        14,
    ];
    fn keccak(a: &mut [u64; 25]) {
        for rc in RC {
            let mut c = [0u64; 5];
            for x in 0..5 {
                c[x] = a[x] ^ a[x + 5] ^ a[x + 10] ^ a[x + 15] ^ a[x + 20];
            }
            for x in 0..5 {
                let d = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
                for y in 0..5 {
                    a[x + 5 * y] ^= d;
                }
            }
            let mut b = [0u64; 25];
            for x in 0..5 {
                for y in 0..5 {
                    b[y + 5 * ((2 * x + 3 * y) % 5)] = a[x + 5 * y].rotate_left(ROT[x + 5 * y]);
                }
            }
            for x in 0..5 {
                for y in 0..5 {
                    a[x + 5 * y] =
                        b[x + 5 * y] ^ (!b[(x + 1) % 5 + 5 * y] & b[(x + 2) % 5 + 5 * y]);
                }
            }
            a[0] ^= rc;
        }
    }
    let rate = 200 - 2 * bits / 8;
    let mut msg = input.to_vec();
    msg.push(0x06);
    while !msg.len().is_multiple_of(rate) {
        msg.push(0);
    }
    let last = msg.len() - 1;
    msg[last] |= 0x80;
    let mut state = [0u64; 25];
    for block in msg.chunks(rate) {
        for (i, lane) in block.chunks(8).enumerate() {
            let mut w = [0u8; 8];
            w.copy_from_slice(lane);
            state[i] ^= u64::from_le_bytes(w);
        }
        keccak(&mut state);
    }
    let mut out = Vec::with_capacity(bits / 8);
    for lane in state.iter() {
        out.extend_from_slice(&lane.to_le_bytes());
    }
    out.truncate(bits / 8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Digests of `hello` from `cassandra:5.0.9`'s `mask_hash(txt, …)`.
    #[test]
    fn digests_match_the_jdk() {
        let h = |a: &str| hex(&Digest::named(a).unwrap().hash(b"hello"));
        assert_eq!(h("MD2"), "a9046c73e00331af68917d3804f70655");
        assert_eq!(h("md5"), "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(h("SHA"), "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
        assert_eq!(
            h("SHA224"),
            "ea09ae9cc6768c50fcee903ed054556e5bfc8347907f12598aa24193"
        );
        assert_eq!(
            h("OID.2.16.840.1.101.3.4.2.1"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            h("SHA512/224"),
            "fe8509ed1fb7dcefc27e6ac1a80eddbec4cb3d2c6fe565244374061c"
        );
        assert_eq!(
            h("SHA3-224"),
            "b87f88c72702fff1748e58b87e9141a42c0dbedc29a78cb0d4a5cd81"
        );
        assert_eq!(
            h("sha3-512"),
            "75d527c368f2efe848ecf6b073a36767800805e9eef2b1857d5f984f036eb6df891d75f72d9b154518c1cd58835286d1da9a38deba3de98b5a53e5ed78a84976"
        );
        assert!(Digest::named("nope").is_none());
    }

    #[test]
    fn partial_masks() {
        let star = '*' as u16;
        assert_eq!(partial_mask("hello", 1, 1, '#' as u16, true), "h###o");
        assert_eq!(partial_mask("hello", 2, 0, '#' as u16, false), "##llo");
        assert_eq!(partial_mask("hello", 10, 10, star, true), "hello");
        assert_eq!(partial_mask("hello", 10, 10, star, false), "*****");
        assert_eq!(partial_mask("hello", -1, 2, star, true), "***lo");
        assert_eq!(partial_mask("hello", 0, 1, star, false), "hell*");
        assert_eq!(partial_mask("", 1, 1, star, true), "");
    }
}
