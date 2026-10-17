//! Keyspace notification classes (`notify-keyspace-events`).

pub const KEYSPACE: u32 = 1 << 0; // K
pub const KEYEVENT: u32 = 1 << 1; // E
pub const GENERIC: u32 = 1 << 2; // g
pub const STRING: u32 = 1 << 3; // $
pub const LIST: u32 = 1 << 4; // l
pub const SET: u32 = 1 << 5; // s
pub const HASH: u32 = 1 << 6; // h
pub const ZSET: u32 = 1 << 7; // z
pub const EXPIRED: u32 = 1 << 8; // x
pub const EVICTED: u32 = 1 << 9; // e
pub const STREAM: u32 = 1 << 10; // t
pub const KEY_MISS: u32 = 1 << 11; // m
pub const MODULE: u32 = 1 << 12; // d
pub const NEW: u32 = 1 << 13; // n
pub const ARRAY: u32 = 1 << 14; // a
pub const OVERWRITTEN: u32 = 1 << 15; // o
pub const TYPE_CHANGED: u32 = 1 << 16; // c
pub const SUBKEYSPACE: u32 = 1 << 17; // S
pub const SUBKEYEVENT: u32 = 1 << 18; // T
pub const SUBKEYSPACEITEM: u32 = 1 << 19; // I
pub const SUBKEYSPACEEVENT: u32 = 1 << 20; // V
/// The subkey channels (`S`, `T`, `I`, `V`).
pub const SUBKEY_ALL: u32 = SUBKEYSPACE | SUBKEYEVENT | SUBKEYSPACEITEM | SUBKEYSPACEEVENT;
/// `A`: every class except `m`, `n`, `o`, `c` and the subkey ones.
pub const ALL: u32 = GENERIC | STRING | LIST | SET | HASH | ZSET | EXPIRED | EVICTED | STREAM | MODULE | ARRAY;

pub fn parse_flags(s: &str) -> Option<u32> {
    let mut f = 0;
    for c in s.chars() {
        f |= match c {
            'A' => ALL,
            'g' => GENERIC,
            '$' => STRING,
            'l' => LIST,
            's' => SET,
            'h' => HASH,
            'z' => ZSET,
            'x' => EXPIRED,
            'e' => EVICTED,
            'K' => KEYSPACE,
            'E' => KEYEVENT,
            't' => STREAM,
            'm' => KEY_MISS,
            'd' => MODULE,
            'a' => ARRAY,
            'n' => NEW,
            'o' => OVERWRITTEN,
            'c' => TYPE_CHANGED,
            'S' => SUBKEYSPACE,
            'T' => SUBKEYEVENT,
            'I' => SUBKEYSPACEITEM,
            'V' => SUBKEYSPACEEVENT,
            _ => return None,
        };
    }
    Some(f)
}

pub fn flags_to_string(f: u32) -> String {
    let mut s = String::new();
    if f & ALL == ALL {
        s.push('A');
    } else {
        for (bit, c) in [
            (GENERIC, 'g'),
            (STRING, '$'),
            (LIST, 'l'),
            (SET, 's'),
            (HASH, 'h'),
            (ZSET, 'z'),
            (EXPIRED, 'x'),
            (EVICTED, 'e'),
            (STREAM, 't'),
            (MODULE, 'd'),
            (ARRAY, 'a'),
            (NEW, 'n'),
            (OVERWRITTEN, 'o'),
            (TYPE_CHANGED, 'c'),
        ] {
            if f & bit != 0 {
                s.push(c);
            }
        }
    }
    for (bit, c) in [
        (KEYSPACE, 'K'),
        (KEYEVENT, 'E'),
        (KEY_MISS, 'm'),
        (SUBKEYSPACE, 'S'),
        (SUBKEYEVENT, 'T'),
        (SUBKEYSPACEITEM, 'I'),
        (SUBKEYSPACEEVENT, 'V'),
    ] {
        if f & bit != 0 {
            s.push(c);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        assert_eq!(flags_to_string(parse_flags("KEA").unwrap()), "AKE");
        assert_eq!(flags_to_string(parse_flags("Ex$").unwrap()), "$xE");
        assert!(parse_flags("Q").is_none());
        assert_eq!(parse_flags("").unwrap(), 0);
    }
}
