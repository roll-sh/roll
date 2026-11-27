//! The protobuf wire format, as far as OTLP's messages need it: varints, fixed 32 and 64 bit
//! fields and length-delimited fields, read as borrowed slices of the request body. Nothing is
//! sized from a length or count a peer sent: a length-delimited field is a slice of the bytes
//! present, checked against what's left.

/// A field's value as the wire carries it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// Why a message didn't decode.
#[derive(Clone, Debug, PartialEq)]
pub struct Malformed(pub String);

/// Fields of one message in order: `(field number, value)`.
pub struct Fields<'a> {
    rest: &'a [u8],
}

impl<'a> Fields<'a> {
    pub fn new(b: &'a [u8]) -> Fields<'a> {
        Fields { rest: b }
    }
}

/// A varint, taken off the front of `b`.
pub(super) fn varint(b: &mut &[u8]) -> Result<u64, Malformed> {
    let mut v = 0u64;
    for i in 0..10 {
        let Some((&byte, rest)) = b.split_first() else { return Err(Malformed("a varint ends early".into())) };
        *b = rest;
        if i == 9 && byte > 1 {
            return Err(Malformed("a varint is longer than 64 bits".into()));
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Malformed("a varint is longer than 64 bits".into()))
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8], Malformed> {
    if b.len() < n {
        return Err(Malformed(format!("a field says {n} bytes and {} are left", b.len())));
    }
    let (head, rest) = b.split_at(n);
    *b = rest;
    Ok(head)
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Wire<'a>), Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let r = (|| {
            let key = varint(&mut self.rest)?;
            let field = u32::try_from(key >> 3).map_err(|_| Malformed("a field number past 2^29".into()))?;
            if field == 0 {
                return Err(Malformed("field number 0".into()));
            }
            let value = match key & 7 {
                0 => Wire::Varint(varint(&mut self.rest)?),
                1 => Wire::Fixed64(u64::from_le_bytes(take(&mut self.rest, 8)?.try_into().unwrap_or_default())),
                2 => {
                    let n = varint(&mut self.rest)?;
                    let n = usize::try_from(n).map_err(|_| Malformed("a length past the address space".into()))?;
                    Wire::Bytes(take(&mut self.rest, n)?)
                }
                5 => Wire::Fixed32(u32::from_le_bytes(take(&mut self.rest, 4)?.try_into().unwrap_or_default())),
                t => return Err(Malformed(format!("wire type {t} (groups aren't used by OTLP)"))),
            };
            Ok((field, value))
        })();
        if r.is_err() {
            // One error ends the message.
            self.rest = &[];
        }
        Some(r)
    }
}

impl<'a> Wire<'a> {
    pub fn bytes(self) -> Result<&'a [u8], Malformed> {
        match self {
            Wire::Bytes(b) => Ok(b),
            _ => Err(Malformed("a message field isn't length-delimited".into())),
        }
    }

    pub fn string(self) -> Result<&'a str, Malformed> {
        std::str::from_utf8(self.bytes()?).map_err(|_| Malformed("a string isn't UTF-8".into()))
    }

    pub fn uint(self) -> Result<u64, Malformed> {
        match self {
            Wire::Varint(v) | Wire::Fixed64(v) => Ok(v),
            Wire::Fixed32(v) => Ok(u64::from(v)),
            Wire::Bytes(_) => Err(Malformed("a number field is length-delimited".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_read_and_lies_are_refused() {
        // 1: varint 150; 2: "hi"; 3: fixed64 7; 4: fixed32 9.
        let b = [0x08, 0x96, 0x01, 0x12, 0x02, b'h', b'i', 0x19, 7, 0, 0, 0, 0, 0, 0, 0, 0x25, 9, 0, 0, 0];
        let got: Vec<_> = Fields::new(&b).collect::<Result<_, _>>().unwrap();
        assert_eq!(got, vec![(1, Wire::Varint(150)), (2, Wire::Bytes(b"hi")), (3, Wire::Fixed64(7)), (4, Wire::Fixed32(9))]);
        // A length past the bytes present, a varint that never ends, field 0, a group.
        for bad in [&[0x12, 0xff, 0xff, 0xff, 0x0f, b'x'][..], &[0x08, 0xff, 0xff], &[0x00, 0x01], &[0x0b]] {
            let r: Result<Vec<_>, _> = Fields::new(bad).collect();
            assert!(r.is_err(), "{bad:?}");
        }
    }
}
