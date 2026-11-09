//! Legacy encodings found in old RDB files: ziplists (RDB < 10), zipmaps (Redis 2.4) and
//! intsets. Read-only, validated in full before use.

use super::listpack::Item;

const ZL_HDR: usize = 10;

/// A validated ziplist (`ziplistValidateIntegrity`, deep).
pub struct Ziplist<'a> {
    zl: &'a [u8],
    len: usize,
}

/// Decode the entry at `p`: (item, prevlen, total entry length).
fn entry(zl: &[u8], p: usize) -> Option<(Item<'_>, usize, usize)> {
    let (prev, ph) = match *zl.get(p)? {
        0xfe => (u32::from_le_bytes(zl.get(p + 1..p + 5)?.try_into().ok()?) as usize, 5),
        0xff => return None,
        b => (b as usize, 1),
    };
    let e = p + ph;
    let enc = *zl.get(e)?;
    let int = |n: usize| zl.get(e + 1..e + 1 + n);
    let (item, len) = match enc >> 6 {
        0 => {
            let n = (enc & 0x3f) as usize;
            (Item::Str(zl.get(e + 1..e + 1 + n)?), 1 + n)
        }
        1 => {
            let n = (((enc & 0x3f) as usize) << 8) | *zl.get(e + 1)? as usize;
            (Item::Str(zl.get(e + 2..e + 2 + n)?), 2 + n)
        }
        2 => {
            let n = u32::from_be_bytes(zl.get(e + 1..e + 5)?.try_into().ok()?) as usize;
            (Item::Str(zl.get(e + 5..(e + 5).checked_add(n)?)?), 5 + n)
        }
        _ => match enc {
            0xc0 => (Item::Int(i16::from_le_bytes(int(2)?.try_into().ok()?) as i64), 3),
            0xd0 => (Item::Int(i32::from_le_bytes(int(4)?.try_into().ok()?) as i64), 5),
            0xe0 => (Item::Int(i64::from_le_bytes(int(8)?.try_into().ok()?)), 9),
            0xf0 => {
                let b = int(3)?;
                (Item::Int((i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as i64), 4)
            }
            0xfe => (Item::Int(*int(1)?.first()? as i8 as i64), 2),
            0xf1..=0xfd => (Item::Int((enc & 0x0f) as i64 - 1), 1),
            _ => return None,
        },
    };
    Some((item, prev, ph + len))
}

impl<'a> Ziplist<'a> {
    pub fn parse(zl: &'a [u8]) -> Option<Self> {
        let size = zl.len();
        if size < ZL_HDR + 1 || zl[size - 1] != 0xff {
            return None;
        }
        let bytes = u32::from_le_bytes(zl[0..4].try_into().unwrap()) as usize;
        let tail = u32::from_le_bytes(zl[4..8].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes([zl[8], zl[9]]) as usize;
        if bytes != size {
            return None;
        }
        let body = &zl[..size - 1];
        let (mut p, mut prev_len, mut last, mut n) = (ZL_HDR, 0, ZL_HDR, 0);
        while p < body.len() {
            let (_, prev, l) = entry(body, p)?;
            if prev != prev_len {
                return None;
            }
            last = p;
            prev_len = l;
            p += l;
            n += 1;
        }
        if p != body.len() || tail != last || (count != u16::MAX as usize && count != n) {
            return None;
        }
        Some(Ziplist { zl: body, len: n })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn iter(&self) -> impl Iterator<Item = Item<'a>> + 'a {
        let zl = self.zl;
        let mut p = ZL_HDR;
        std::iter::from_fn(move || {
            let (it, _, l) = entry(zl, p)?;
            p += l;
            Some(it)
        })
    }
}

/// A validated zipmap (`zipmapValidateIntegrity`, deep): field/value pairs.
pub struct Zipmap<'a> {
    zm: &'a [u8],
    len: usize,
}

/// A zipmap length at `p`: (value, encoded size).
fn zm_len(zm: &[u8], p: usize) -> Option<(usize, usize)> {
    match *zm.get(p)? {
        b if b < 254 => Some((b as usize, 1)),
        _ => Some((u32::from_le_bytes(zm.get(p + 1..p + 5)?.try_into().ok()?) as usize, 5)),
    }
}

/// The pair at `p` (which must not be the end marker): (field, value, next position).
fn zm_pair(zm: &[u8], p: usize) -> Option<(&[u8], &[u8], usize)> {
    let (fl, s) = zm_len(zm, p)?;
    let f = p + s;
    let fe = f.checked_add(fl)?;
    let (vl, s) = zm_len(zm, fe)?;
    let free = *zm.get(fe + s)? as usize;
    let v = fe + s + 1;
    let ve = v.checked_add(vl)?;
    let next = ve.checked_add(free)?;
    // Everything up to `next` must lie before the end marker.
    if next >= zm.len() {
        return None;
    }
    Some((&zm[f..fe], &zm[v..ve], next))
}

impl<'a> Zipmap<'a> {
    pub fn parse(zm: &'a [u8]) -> Option<Self> {
        if zm.len() < 2 || zm[zm.len() - 1] != 0xff {
            return None;
        }
        let (mut p, mut n) = (1, 0usize);
        while zm[p] != 0xff {
            let (_, _, next) = zm_pair(zm, p)?;
            p = next;
            n += 1;
        }
        if p != zm.len() - 1 || n == 0 || (zm[0] != 254 && zm[0] as usize != n) {
            return None;
        }
        Some(Zipmap { zm, len: n })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + 'a {
        let zm = self.zm;
        let mut p = 1;
        std::iter::from_fn(move || {
            if zm[p] == 0xff {
                return None;
            }
            let (f, v, next) = zm_pair(zm, p)?;
            p = next;
            Some((f, v))
        })
    }
}

/// A validated intset (`intsetValidateIntegrity`, deep): (element width, values ascending).
pub fn intset(b: &[u8]) -> Option<(usize, impl Iterator<Item = i64> + '_)> {
    if b.len() < 8 {
        return None;
    }
    let w = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
    let n = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    if !matches!(w, 2 | 4 | 8) || n == 0 || (n as u64) * (w as u64) + 8 != b.len() as u64 {
        return None;
    }
    let get = move |i: usize| -> i64 {
        let s = &b[8 + i * w..8 + (i + 1) * w];
        match w {
            2 => i16::from_le_bytes([s[0], s[1]]) as i64,
            4 => i32::from_le_bytes(s.try_into().unwrap()) as i64,
            _ => i64::from_le_bytes(s.try_into().unwrap()),
        }
    };
    if (1..n).any(|i| get(i - 1) >= get(i)) {
        return None;
    }
    Some((w, (0..n).map(get)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a ziplist the way Redis did (`ziplistPush`).
    pub(crate) fn build(items: &[Item]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut prev = 0usize;
        let mut last = ZL_HDR;
        for it in items {
            let mut e = Vec::new();
            if prev < 254 {
                e.push(prev as u8);
            } else {
                e.push(0xfe);
                e.extend_from_slice(&(prev as u32).to_le_bytes());
            }
            match *it {
                Item::Str(s) if s.len() < 64 => {
                    e.push(s.len() as u8);
                    e.extend_from_slice(s);
                }
                Item::Str(s) if s.len() < 16384 => {
                    e.extend_from_slice(&[0x40 | (s.len() >> 8) as u8, s.len() as u8]);
                    e.extend_from_slice(s);
                }
                Item::Str(s) => {
                    e.push(0x80);
                    e.extend_from_slice(&(s.len() as u32).to_be_bytes());
                    e.extend_from_slice(s);
                }
                Item::Int(v) if (0..=12).contains(&v) => e.push(0xf1 + v as u8),
                Item::Int(v) if i8::try_from(v).is_ok() => e.extend_from_slice(&[0xfe, v as u8]),
                Item::Int(v) if i16::try_from(v).is_ok() => {
                    e.push(0xc0);
                    e.extend_from_slice(&(v as i16).to_le_bytes());
                }
                Item::Int(v) if (-(1 << 23)..1 << 23).contains(&v) => {
                    e.push(0xf0);
                    e.extend_from_slice(&(v as i32).to_le_bytes()[..3]);
                }
                Item::Int(v) if i32::try_from(v).is_ok() => {
                    e.push(0xd0);
                    e.extend_from_slice(&(v as i32).to_le_bytes());
                }
                Item::Int(v) => {
                    e.push(0xe0);
                    e.extend_from_slice(&v.to_le_bytes());
                }
            }
            last = ZL_HDR + body.len();
            prev = e.len();
            body.extend_from_slice(&e);
        }
        let mut zl = Vec::new();
        zl.extend_from_slice(&((ZL_HDR + body.len() + 1) as u32).to_le_bytes());
        zl.extend_from_slice(&(last as u32).to_le_bytes());
        zl.extend_from_slice(&(items.len() as u16).to_le_bytes());
        zl.extend_from_slice(&body);
        zl.push(0xff);
        zl
    }

    #[test]
    fn ziplist_roundtrip_and_corruption() {
        let big = vec![b'z'; 300];
        let huge = vec![b'h'; 20000];
        let items = [
            Item::Str(b"hello"),
            Item::Int(7),
            Item::Int(-100),
            Item::Int(30000),
            Item::Int(-5_000_000),
            Item::Int(2_000_000_000),
            Item::Int(i64::MIN),
            Item::Str(&big),
            Item::Str(&huge),
            Item::Str(b""),
        ];
        let zl = build(&items);
        let z = Ziplist::parse(&zl).unwrap();
        assert_eq!(z.len(), items.len());
        assert_eq!(z.iter().collect::<Vec<_>>(), items);
        for i in 0..zl.len().min(400) {
            let mut bad = zl.clone();
            bad[i] = bad[i].wrapping_add(1);
            if let Some(z) = Ziplist::parse(&bad) {
                assert_eq!(z.iter().count(), z.len());
            }
        }
        assert!(Ziplist::parse(&zl[..zl.len() - 1]).is_none());
        let empty = build(&[]);
        assert_eq!(Ziplist::parse(&empty).unwrap().len(), 0);
    }

    #[test]
    fn zipmap_and_intset() {
        // zmlen 2, "a"->"1" (free 0), "bb"->"xyz" with 2 free bytes.
        let zm = [2, 1, b'a', 1, 0, b'1', 2, b'b', b'b', 3, 2, b'x', b'y', b'z', 0, 0, 0xff];
        let z = Zipmap::parse(&zm).unwrap();
        assert_eq!(z.iter().collect::<Vec<_>>(), [(&b"a"[..], &b"1"[..]), (b"bb", b"xyz")]);
        let mut bad = zm;
        bad[0] = 3;
        assert!(Zipmap::parse(&bad).is_none());
        assert!(Zipmap::parse(&zm[..10]).is_none());

        let mut is = vec![2, 0, 0, 0, 3, 0, 0, 0];
        for v in [-2i16, 5, 300] {
            is.extend_from_slice(&v.to_le_bytes());
        }
        let (w, it) = intset(&is).unwrap();
        assert_eq!((w, it.collect::<Vec<_>>()), (2, vec![-2, 5, 300]));
        is[8] = 0x2c; // 300 then 300: not ascending
        is[9] = 1;
        assert!(intset(&is).is_none());
        assert!(intset(&[2, 0, 0, 0, 0, 0, 0, 0]).is_none());
    }
}
