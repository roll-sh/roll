//! Double Metaphone (`contrib/fuzzystrmatch/dmetaphone.c`, Lawrence Philips' algorithm as
//! the Perl module has it): the primary and alternate codes, at most four characters each.

struct Word {
    /// The upper-cased word, padded with five spaces.
    s: Vec<u8>,
}

impl Word {
    fn at(&self, pos: isize) -> u8 {
        if pos < 0 || pos as usize >= self.s.len() { 0 } else { self.s[pos as usize] }
    }

    fn vowel(&self, pos: isize) -> bool {
        matches!(self.at(pos), b'A' | b'E' | b'I' | b'O' | b'U' | b'Y')
    }

    /// `StringAt`: one of `tests` (each `len` long, compared as strncmp does) at `start`.
    fn string_at(&self, start: isize, len: usize, tests: &[&str]) -> bool {
        if start < 0 || start as usize >= self.s.len() {
            return false;
        }
        tests.iter().any(|t| {
            let t = t.as_bytes();
            for k in 0..len {
                let a = self.at(start + k as isize);
                let b = t.get(k).copied().unwrap_or(0);
                if a != b {
                    return false;
                }
                if a == 0 {
                    return true;
                }
            }
            true
        })
    }

    fn contains(&self, p: &[u8]) -> bool {
        self.s.windows(p.len()).any(|w| w == p)
    }

    fn slavo_germanic(&self) -> bool {
        self.contains(b"W") || self.contains(b"K") || self.contains(b"CZ") || self.contains(b"WITZ")
    }
}

/// The primary and the alternate code.
pub(super) fn double_metaphone(input: &[u8]) -> (String, String) {
    let end = input.iter().position(|&c| c == 0).unwrap_or(input.len());
    let length = end as isize;
    let last = length - 1;
    let mut s: Vec<u8> = input[..end].iter().map(u8::to_ascii_uppercase).collect();
    s.extend_from_slice(b"     ");
    let o = Word { s };
    let mut p: Vec<u8> = Vec::new();
    let mut q: Vec<u8> = Vec::new();
    let mut current: isize = 0;
    macro_rules! add {
        ($a:expr, $b:expr) => {{
            p.extend_from_slice($a.as_bytes());
            q.extend_from_slice($b.as_bytes());
        }};
    }
    if o.string_at(0, 2, &["GN", "KN", "PN", "WR", "PS"]) {
        current += 1;
    }
    if o.at(0) == b'X' {
        add!("S", "S");
        current += 1;
    }
    while p.len() < 4 || q.len() < 4 {
        if current >= length {
            break;
        }
        let c = current;
        match o.at(c) {
            b'A' | b'E' | b'I' | b'O' | b'U' | b'Y' => {
                if c == 0 {
                    add!("A", "A");
                }
                current += 1;
            }
            b'B' => {
                add!("P", "P");
                current += if o.at(c + 1) == b'B' { 2 } else { 1 };
            }
            0xC7 => {
                add!("S", "S");
                current += 1;
            }
            b'C' => current = code_c(&o, c, &mut p, &mut q),
            b'D' => {
                if o.string_at(c, 2, &["DG"]) {
                    if o.string_at(c + 2, 1, &["I", "E", "Y"]) {
                        add!("J", "J");
                        current += 3;
                    } else {
                        add!("TK", "TK");
                        current += 2;
                    }
                } else if o.string_at(c, 2, &["DT", "DD"]) {
                    add!("T", "T");
                    current += 2;
                } else {
                    add!("T", "T");
                    current += 1;
                }
            }
            b'F' => {
                current += if o.at(c + 1) == b'F' { 2 } else { 1 };
                add!("F", "F");
            }
            b'G' => current = code_g(&o, c, &mut p, &mut q),
            b'H' => {
                if (c == 0 || o.vowel(c - 1)) && o.vowel(c + 1) {
                    add!("H", "H");
                    current += 2;
                } else {
                    current += 1;
                }
            }
            b'J' => current = code_j(&o, c, last, &mut p, &mut q),
            b'K' => {
                current += if o.at(c + 1) == b'K' { 2 } else { 1 };
                add!("K", "K");
            }
            b'L' => {
                if o.at(c + 1) == b'L' {
                    if (c == length - 3 && o.string_at(c - 1, 4, &["ILLO", "ILLA", "ALLE"]))
                        || ((o.string_at(last - 1, 2, &["AS", "OS"]) || o.string_at(last, 1, &["A", "O"]))
                            && o.string_at(c - 1, 4, &["ALLE"]))
                    {
                        add!("L", "");
                        current += 2;
                        continue;
                    }
                    current += 2;
                } else {
                    current += 1;
                }
                add!("L", "L");
            }
            b'M' => {
                if (o.string_at(c - 1, 3, &["UMB"]) && (c + 1 == last || o.string_at(c + 2, 2, &["ER"])))
                    || o.at(c + 1) == b'M'
                {
                    current += 2;
                } else {
                    current += 1;
                }
                add!("M", "M");
            }
            b'N' => {
                current += if o.at(c + 1) == b'N' { 2 } else { 1 };
                add!("N", "N");
            }
            0xD1 => {
                current += 1;
                add!("N", "N");
            }
            b'P' => {
                if o.at(c + 1) == b'H' {
                    add!("F", "F");
                    current += 2;
                } else {
                    current += if o.string_at(c + 1, 1, &["P", "B"]) { 2 } else { 1 };
                    add!("P", "P");
                }
            }
            b'Q' => {
                current += if o.at(c + 1) == b'Q' { 2 } else { 1 };
                add!("K", "K");
            }
            b'R' => {
                if c == last
                    && !o.slavo_germanic()
                    && o.string_at(c - 2, 2, &["IE"])
                    && !o.string_at(c - 4, 2, &["ME", "MA"])
                {
                    add!("", "R");
                } else {
                    add!("R", "R");
                }
                current += if o.at(c + 1) == b'R' { 2 } else { 1 };
            }
            b'S' => current = code_s(&o, c, last, &mut p, &mut q),
            b'T' => current = code_t(&o, c, &mut p, &mut q),
            b'V' => {
                current += if o.at(c + 1) == b'V' { 2 } else { 1 };
                add!("F", "F");
            }
            b'W' => current = code_w(&o, c, last, &mut p, &mut q),
            b'X' => {
                if !(c == last && (o.string_at(c - 3, 3, &["IAU", "EAU"]) || o.string_at(c - 2, 2, &["AU", "OU"]))) {
                    add!("KS", "KS");
                }
                current += if o.string_at(c + 1, 1, &["C", "X"]) { 2 } else { 1 };
            }
            b'Z' => {
                if o.at(c + 1) == b'H' {
                    add!("J", "J");
                    current += 2;
                } else {
                    if o.string_at(c + 1, 2, &["ZO", "ZI", "ZA"]) || (o.slavo_germanic() && c > 0 && o.at(c - 1) != b'T') {
                        add!("S", "TS");
                    } else {
                        add!("S", "S");
                    }
                    current += if o.at(c + 1) == b'Z' { 2 } else { 1 };
                }
            }
            _ => current += 1,
        }
    }
    p.truncate(4);
    q.truncate(4);
    (String::from_utf8_lossy(&p).into_owned(), String::from_utf8_lossy(&q).into_owned())
}

fn push(p: &mut Vec<u8>, q: &mut Vec<u8>, a: &str, b: &str) {
    p.extend_from_slice(a.as_bytes());
    q.extend_from_slice(b.as_bytes());
}

fn code_c(o: &Word, c: isize, p: &mut Vec<u8>, q: &mut Vec<u8>) -> isize {
    let mut add = |a: &str, b: &str| push(p, q, a, b);
    // Various germanic.
    if c > 1
        && !o.vowel(c - 2)
        && o.string_at(c - 1, 3, &["ACH"])
        && o.at(c + 2) != b'I'
        && (o.at(c + 2) != b'E' || o.string_at(c - 2, 6, &["BACHER", "MACHER"]))
    {
        add("K", "K");
        return c + 2;
    }
    if c == 0 && o.string_at(c, 6, &["CAESAR"]) {
        add("S", "S");
        return c + 2;
    }
    if o.string_at(c, 4, &["CHIA"]) {
        add("K", "K");
        return c + 2;
    }
    if o.string_at(c, 2, &["CH"]) {
        if c > 0 && o.string_at(c, 4, &["CHAE"]) {
            add("K", "X");
            return c + 2;
        }
        if c == 0
            && (o.string_at(c + 1, 5, &["HARAC", "HARIS"]) || o.string_at(c + 1, 3, &["HOR", "HYM", "HIA", "HEM"]))
            && !o.string_at(0, 5, &["CHORE"])
        {
            add("K", "K");
            return c + 2;
        }
        if (o.string_at(0, 4, &["VAN ", "VON "]) || o.string_at(0, 3, &["SCH"]))
            || o.string_at(c - 2, 6, &["ORCHES", "ARCHIT", "ORCHID"])
            || o.string_at(c + 2, 1, &["T", "S"])
            || ((o.string_at(c - 1, 1, &["A", "O", "U", "E"]) || c == 0)
                && o.string_at(c + 2, 1, &["L", "R", "N", "M", "B", "H", "F", "V", "W", " "]))
        {
            add("K", "K");
        } else if c > 0 {
            if o.string_at(0, 2, &["MC"]) {
                add("K", "K");
            } else {
                add("X", "K");
            }
        } else {
            add("X", "X");
        }
        return c + 2;
    }
    if o.string_at(c, 2, &["CZ"]) && !o.string_at(c - 2, 4, &["WICZ"]) {
        add("S", "X");
        return c + 2;
    }
    if o.string_at(c + 1, 3, &["CIA"]) {
        add("X", "X");
        return c + 3;
    }
    if o.string_at(c, 2, &["CC"]) && !(c == 1 && o.at(0) == b'M') {
        if o.string_at(c + 2, 1, &["I", "E", "H"]) && !o.string_at(c + 2, 2, &["HU"]) {
            if (c == 1 && o.at(c - 1) == b'A') || o.string_at(c - 1, 5, &["UCCEE", "UCCES"]) {
                add("KS", "KS");
            } else {
                add("X", "X");
            }
            return c + 3;
        }
        add("K", "K");
        return c + 2;
    }
    if o.string_at(c, 2, &["CK", "CG", "CQ"]) {
        add("K", "K");
        return c + 2;
    }
    if o.string_at(c, 2, &["CI", "CE", "CY"]) {
        if o.string_at(c, 3, &["CIO", "CIE", "CIA"]) {
            add("S", "X");
        } else {
            add("S", "S");
        }
        return c + 2;
    }
    add("K", "K");
    if o.string_at(c + 1, 2, &[" C", " Q", " G"]) {
        c + 3
    } else if o.string_at(c + 1, 1, &["C", "K", "Q"]) && !o.string_at(c + 1, 2, &["CE", "CI"]) {
        c + 2
    } else {
        c + 1
    }
}

fn code_g(o: &Word, c: isize, p: &mut Vec<u8>, q: &mut Vec<u8>) -> isize {
    let mut add = |a: &str, b: &str| push(p, q, a, b);
    if o.at(c + 1) == b'H' {
        if c > 0 && !o.vowel(c - 1) {
            add("K", "K");
            return c + 2;
        }
        if c == 0 {
            if o.at(c + 2) == b'I' {
                add("J", "J");
            } else {
                add("K", "K");
            }
            return c + 2;
        }
        // Parker's rule.
        if (c > 1 && o.string_at(c - 2, 1, &["B", "H", "D"]))
            || (c > 2 && o.string_at(c - 3, 1, &["B", "H", "D"]))
            || (c > 3 && o.string_at(c - 4, 1, &["B", "H"]))
        {
            return c + 2;
        }
        if c > 2 && o.at(c - 1) == b'U' && o.string_at(c - 3, 1, &["C", "G", "L", "R", "T"]) {
            add("F", "F");
        } else if c > 0 && o.at(c - 1) != b'I' {
            add("K", "K");
        }
        return c + 2;
    }
    if o.at(c + 1) == b'N' {
        if c == 1 && o.vowel(0) && !o.slavo_germanic() {
            add("KN", "N");
        } else if !o.string_at(c + 2, 2, &["EY"]) && o.at(c + 1) != b'Y' && !o.slavo_germanic() {
            add("N", "KN");
        } else {
            add("KN", "KN");
        }
        return c + 2;
    }
    if o.string_at(c + 1, 2, &["LI"]) && !o.slavo_germanic() {
        add("KL", "L");
        return c + 2;
    }
    if c == 0
        && (o.at(c + 1) == b'Y'
            || o.string_at(c + 1, 2, &["ES", "EP", "EB", "EL", "EY", "IB", "IL", "IN", "IE", "EI", "ER"]))
    {
        add("K", "J");
        return c + 2;
    }
    if (o.string_at(c + 1, 2, &["ER"]) || o.at(c + 1) == b'Y')
        && !o.string_at(0, 6, &["DANGER", "RANGER", "MANGER"])
        && !o.string_at(c - 1, 1, &["E", "I"])
        && !o.string_at(c - 1, 3, &["RGY", "OGY"])
    {
        add("K", "J");
        return c + 2;
    }
    if o.string_at(c + 1, 1, &["E", "I", "Y"]) || o.string_at(c - 1, 4, &["AGGI", "OGGI"]) {
        if (o.string_at(0, 4, &["VAN ", "VON "]) || o.string_at(0, 3, &["SCH"])) || o.string_at(c + 1, 2, &["ET"]) {
            add("K", "K");
        } else if o.string_at(c + 1, 4, &["IER "]) {
            add("J", "J");
        } else {
            add("J", "K");
        }
        return c + 2;
    }
    let next = if o.at(c + 1) == b'G' { c + 2 } else { c + 1 };
    add("K", "K");
    next
}

fn code_j(o: &Word, c: isize, last: isize, p: &mut Vec<u8>, q: &mut Vec<u8>) -> isize {
    let mut add = |a: &str, b: &str| push(p, q, a, b);
    if o.string_at(c, 4, &["JOSE"]) || o.string_at(0, 4, &["SAN "]) {
        if (c == 0 && o.at(c + 4) == b' ') || o.string_at(0, 4, &["SAN "]) {
            add("H", "H");
        } else {
            add("J", "H");
        }
        return c + 1;
    }
    if c == 0 && !o.string_at(c, 4, &["JOSE"]) {
        add("J", "A");
    } else if o.vowel(c - 1) && !o.slavo_germanic() && (o.at(c + 1) == b'A' || o.at(c + 1) == b'O') {
        add("J", "H");
    } else if c == last {
        add("J", "");
    } else if !o.string_at(c + 1, 1, &["L", "T", "K", "S", "N", "M", "B", "Z"]) && !o.string_at(c - 1, 1, &["S", "K", "L"]) {
        add("J", "J");
    }
    if o.at(c + 1) == b'J' { c + 2 } else { c + 1 }
}

fn code_s(o: &Word, c: isize, last: isize, p: &mut Vec<u8>, q: &mut Vec<u8>) -> isize {
    let mut add = |a: &str, b: &str| push(p, q, a, b);
    if o.string_at(c - 1, 3, &["ISL", "YSL"]) {
        return c + 1;
    }
    if c == 0 && o.string_at(c, 5, &["SUGAR"]) {
        add("X", "S");
        return c + 1;
    }
    if o.string_at(c, 2, &["SH"]) {
        if o.string_at(c + 1, 4, &["HEIM", "HOEK", "HOLM", "HOLZ"]) {
            add("S", "S");
        } else {
            add("X", "X");
        }
        return c + 2;
    }
    if o.string_at(c, 3, &["SIO", "SIA"]) || o.string_at(c, 4, &["SIAN"]) {
        if !o.slavo_germanic() {
            add("S", "X");
        } else {
            add("S", "S");
        }
        return c + 3;
    }
    if (c == 0 && o.string_at(c + 1, 1, &["M", "N", "L", "W"])) || o.string_at(c + 1, 1, &["Z"]) {
        add("S", "X");
        return if o.string_at(c + 1, 1, &["Z"]) { c + 2 } else { c + 1 };
    }
    if o.string_at(c, 2, &["SC"]) {
        if o.at(c + 2) == b'H' {
            if o.string_at(c + 3, 2, &["OO", "ER", "EN", "UY", "ED", "EM"]) {
                if o.string_at(c + 3, 2, &["ER", "EN"]) {
                    add("X", "SK");
                } else {
                    add("SK", "SK");
                }
            } else if c == 0 && !o.vowel(3) && o.at(3) != b'W' {
                add("X", "S");
            } else {
                add("X", "X");
            }
            return c + 3;
        }
        if o.string_at(c + 2, 1, &["I", "E", "Y"]) {
            add("S", "S");
        } else {
            add("SK", "SK");
        }
        return c + 3;
    }
    if c == last && o.string_at(c - 2, 2, &["AI", "OI"]) {
        add("", "S");
    } else {
        add("S", "S");
    }
    if o.string_at(c + 1, 1, &["S", "Z"]) { c + 2 } else { c + 1 }
}

fn code_t(o: &Word, c: isize, p: &mut Vec<u8>, q: &mut Vec<u8>) -> isize {
    let mut add = |a: &str, b: &str| push(p, q, a, b);
    if o.string_at(c, 4, &["TION"]) || o.string_at(c, 3, &["TIA", "TCH"]) {
        add("X", "X");
        return c + 3;
    }
    if o.string_at(c, 2, &["TH"]) || o.string_at(c, 3, &["TTH"]) {
        if o.string_at(c + 2, 2, &["OM", "AM"]) || o.string_at(0, 4, &["VAN ", "VON "]) || o.string_at(0, 3, &["SCH"]) {
            add("T", "T");
        } else {
            add("0", "T");
        }
        return c + 2;
    }
    let next = if o.string_at(c + 1, 1, &["T", "D"]) { c + 2 } else { c + 1 };
    add("T", "T");
    next
}

fn code_w(o: &Word, c: isize, last: isize, p: &mut Vec<u8>, q: &mut Vec<u8>) -> isize {
    let mut add = |a: &str, b: &str| push(p, q, a, b);
    if o.string_at(c, 2, &["WR"]) {
        add("R", "R");
        return c + 2;
    }
    if c == 0 && (o.vowel(c + 1) || o.string_at(c, 2, &["WH"])) {
        if o.vowel(c + 1) {
            add("A", "F");
        } else {
            add("A", "A");
        }
    }
    if (c == last && o.vowel(c - 1))
        || o.string_at(c - 1, 5, &["EWSKI", "EWSKY", "OWSKI", "OWSKY"])
        || o.string_at(0, 3, &["SCH"])
    {
        add("", "F");
        return c + 1;
    }
    if o.string_at(c, 4, &["WICZ", "WITZ"]) {
        add("TS", "FX");
        return c + 4;
    }
    c + 1
}

#[cfg(test)]
mod tests {
    use super::double_metaphone;

    #[test]
    fn dmetaphone_vectors() {
        let d = |s: &str| double_metaphone(s.as_bytes());
        assert_eq!(d("gumbo"), ("KMP".into(), "KMP".into()));
        assert_eq!(d("Smith"), ("SM0".into(), "XMT".into()));
        assert_eq!(d("Schmidt"), ("XMT".into(), "SMT".into()));
        assert_eq!(d("Thomas"), ("TMS".into(), "TMS".into()));
    }
}
