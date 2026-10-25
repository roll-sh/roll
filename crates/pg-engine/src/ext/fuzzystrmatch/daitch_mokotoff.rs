//! Daitch-Mokotoff soundex (`contrib/fuzzystrmatch/daitch_mokotoff.c`): every six-digit code
//! a name can have. The coding chart is daitch_mokotoff_header.pl's table, built into the
//! same letter tree at first use.

use crate::Result;
use crate::sql::func::{FnCtx, text_arg};
use crate::types::Value;
use crate::types::array::ArrayValue;
use crate::types::DataType;
use std::sync::{Arc, OnceLock};

/// daitch_mokotoff_header.pl's `__DATA__`: letters, then the codes (start of name, before a
/// vowel, any other; alternatives separated by `|`; X: not coded). `a`, `e`, `t` stand for Ą,
/// Ę, Ţ (looked up as `[`, `\`, `]`).
const CHART: &str = "\
AI,AJ,AY 0,1,X
AU 0,7,X
a X,X,6|X,X,X
A 0,X,X
B 7,7,7
CHS 5,54,54
CH 5,5,5|4,4,4
CK 5,5,5|45,45,45
CZ,CS,CSZ,CZS 4,4,4
C 5,5,5|4,4,4
DRZ,DRS 4,4,4
DS,DSH,DSZ 4,4,4
DZ,DZH,DZS 4,4,4
D,DT 3,3,3
EI,EJ,EY 0,1,X
EU 1,1,X
e X,X,6|X,X,X
E 0,X,X
FB 7,7,7
F 7,7,7
G 5,5,5
H 5,5,X
IA,IE,IO,IU 1,X,X
I 0,X,X
J 1,X,X|4,4,4
KS 5,54,54
KH 5,5,5
K 5,5,5
L 8,8,8
MN 66,66,66
M 6,6,6
NM 66,66,66
N 6,6,6
OI,OJ,OY 0,1,X
O 0,X,X
P,PF,PH 7,7,7
Q 5,5,5
RZ,RS 94,94,94|4,4,4
R 9,9,9
SCHTSCH,SCHTSH,SCHTCH 2,4,4
SCH 4,4,4
SHTCH,SHCH,SHTSH 2,4,4
SHT,SCHT,SCHD 2,43,43
SH 4,4,4
STCH,STSCH,SC 2,4,4
STRZ,STRS,STSH 2,4,4
ST 2,43,43
SZCZ,SZCS 2,4,4
SZT,SHD,SZD,SD 2,43,43
SZ 4,4,4
S 4,4,4
TCH,TTCH,TTSCH 4,4,4
TH 3,3,3
TRZ,TRS 4,4,4
TSCH,TSH 4,4,4
TS,TTS,TTSZ,TC 4,4,4
TZ,TTZ,TZS,TSZ 4,4,4
t 3,3,3|4,4,4
T 3,3,3
UI,UJ,UY,UE 0,1,X
U 0,X,X
V 7,7,7
W 7,7,7
X 5,54,54
Y 1,X,X
ZDZ,ZDZH,ZHDZH 2,4,4
ZD,ZHD 2,43,43
ZH,ZS,ZSCH,ZSH 4,4,4
Z 4,4,4";

/// One or two alternatives of (start of name, before a vowel, any other).
type Codes = Vec<[String; 3]>;

#[derive(Default)]
struct Letter {
    /// Following letters, by letter (sorted as the generator sorts its keys).
    next: std::collections::BTreeMap<u8, Letter>,
    codes: Option<Arc<Codes>>,
}

fn chart() -> &'static Letter {
    static ROOT: OnceLock<Letter> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut root = Letter::default();
        for line in CHART.lines() {
            let (letters, codes) = line.split_once(' ').expect("chart line");
            let codes: Codes = codes
                .split('|')
                .map(|alt| {
                    let mut it = alt.split(',').map(str::to_string);
                    [it.next().unwrap(), it.next().unwrap(), it.next().unwrap()]
                })
                .collect();
            let codes = Arc::new(codes);
            for seq in letters.split(',') {
                let mut node = &mut root;
                for c in seq.bytes() {
                    // (The substitute letters are looked up as `[`, `\`, `]`.)
                    let c = match c {
                        b'a' => b'[',
                        b'e' => b'\\',
                        b't' => b']',
                        c => c,
                    };
                    node = node.next.entry(c).or_default();
                }
                node.codes = Some(codes.clone());
            }
        }
        root
    })
}

/// ISO-8859-1 0x60..=0xFF to upper-case ASCII.
const ISO8859_1_UPPER: &[u8] = b"`ABCDEFGHIJKLMNOPQRSTUVWXYZ{|}~                                  !                             ?AAAAAAECEEEEIIIIDNOOOOO*OUUUUYDSAAAAAAECEEEEIIIIDNOOOOO/OUUUUYDY";

/// `read_char`: the next character as upper-case ASCII (0 at the end).
fn read_char(s: &str, ix: &mut usize) -> u8 {
    const NA: u8 = 0x1a;
    let Some(ch) = s[*ix..].chars().next() else {
        return 0;
    };
    *ix += ch.len_utf8();
    let c = ch as u32;
    if (b'[' as u32..=b']' as u32).contains(&c) {
        NA
    } else if c < 0x60 {
        c as u8
    } else if c < 0x100 {
        ISO8859_1_UPPER[(c - 0x60) as usize]
    } else {
        match c {
            0x0104 | 0x0105 => b'[',
            0x0118 | 0x0119 => b'\\',
            0x0162 | 0x0163 | 0x021A | 0x021B => b']',
            _ => NA,
        }
    }
}

/// `read_valid_char`: the next character in `A..=]`, skipping others (0 at the end).
fn read_valid_char(s: &str, ix: &mut usize) -> u8 {
    loop {
        let c = read_char(s, ix);
        if c == 0 || (b'A'..=b']').contains(&c) {
            return c;
        }
    }
}

/// `read_letter`: the coding of the longest letter sequence at `ix` (which moves past it).
fn read_letter(s: &str, ix: &mut usize) -> Option<Arc<Codes>> {
    let c = read_valid_char(s, ix);
    if c == 0 {
        return None;
    }
    let mut node = chart().next.get(&c)?;
    let mut codes = node.codes.clone();
    let mut i = *ix;
    while !node.next.is_empty() {
        let c = read_valid_char(s, &mut i);
        if c == 0 {
            break;
        }
        let Some(n) = node.next.get(&c) else {
            break;
        };
        node = n;
        if node.codes.is_some() {
            codes = node.codes.clone();
            *ix = i;
        }
    }
    codes
}

const DIGITS: usize = 6;

#[derive(Clone)]
struct Node {
    len: usize,
    soundex: [u8; DIGITS],
    is_leaf: bool,
    last_update: i32,
    prev_digits: [u8; 2],
    next_digits: [u8; 2],
    prev_index: i32,
    next_index: i32,
    children: [Option<usize>; 10],
    next: [Option<usize>; 2],
}

const START: Node = Node {
    len: 0,
    soundex: *b"000000",
    is_leaf: false,
    last_update: 0,
    prev_digits: [0; 2],
    next_digits: [0; 2],
    prev_index: 0,
    next_index: 0,
    children: [None; 10],
    next: [None; 2],
};

struct Tree {
    nodes: Vec<Node>,
    first: [Option<usize>; 2],
    last: [Option<usize>; 2],
    out: Vec<String>,
}

impl Tree {
    fn initialize(&mut self, n: usize, letter_no: i32) {
        let x = &mut self.nodes[n];
        if x.last_update < letter_no {
            x.prev_digits = x.next_digits;
            x.next_digits = [0; 2];
            x.prev_index = x.next_index;
            x.next_index = 0;
            x.is_leaf = false;
            x.last_update = letter_no;
        }
    }

    fn add_next_digit(&mut self, n: usize, index: i32, digit: u8) {
        let x = &mut self.nodes[n];
        x.next_index |= index;
        if x.next_digits[0] == 0 {
            x.next_digits[0] = digit;
        } else if x.next_digits[0] != digit {
            x.next_digits[1] = digit;
        }
    }

    fn set_leaf(&mut self, n: usize, ix: usize) {
        if !self.nodes[n].is_leaf {
            self.nodes[n].is_leaf = true;
            match (self.first[ix], self.last[ix]) {
                (Some(_), Some(l)) => self.nodes[l].next[ix] = Some(n),
                _ => self.first[ix] = Some(n),
            }
            self.last[ix] = Some(n);
            self.nodes[n].next[ix] = None;
        }
    }

    fn child(&mut self, parent: usize, digit: u8) -> Option<usize> {
        let i = (digit - b'0') as usize;
        if let Some(c) = self.nodes[parent].children[i] {
            return (self.nodes[c].len < DIGITS).then_some(c);
        }
        let mut node = START.clone();
        node.soundex = self.nodes[parent].soundex;
        node.len = self.nodes[parent].len;
        node.soundex[node.len] = digit;
        node.len += 1;
        node.next_index = node.prev_index;
        let complete = node.len >= DIGITS;
        if complete {
            self.out.push(String::from_utf8_lossy(&node.soundex).into_owned());
        }
        self.nodes.push(node);
        let c = self.nodes.len() - 1;
        self.nodes[parent].children[i] = Some(c);
        (!complete).then_some(c)
    }

    #[allow(clippy::too_many_arguments)]
    fn update(&mut self, n: usize, ix: usize, letter_no: i32, prev_index: i32, next_index: i32, digits: &[u8], digit_no: usize) {
        let digit = digits[digit_no];
        self.initialize(n, letter_no);
        let x = &self.nodes[n];
        if x.prev_index != 0 && x.prev_index & prev_index == 0 {
            return;
        }
        let mut dirty = Vec::with_capacity(2);
        if digit == b'X' || (digit_no == 0 && (x.prev_digits[0] == digit || x.prev_digits[1] == digit)) {
            dirty.push(n);
        }
        let x = &self.nodes[n];
        if digit != b'X' && (digit_no > 0 || x.prev_digits[0] != digit || x.prev_digits[1] != 0)
            && let Some(c) = self.child(n, digit)
        {
            self.initialize(c, letter_no);
            dirty.push(c);
        }
        // (The digit index moves on for each node, as update_node's `++digit_no` does.)
        let mut digit_no = digit_no;
        for d in dirty {
            self.add_next_digit(d, next_index, digit);
            digit_no += 1;
            if digit_no < digits.len() {
                self.update(d, ix, letter_no, prev_index, next_index, digits, digit_no);
            } else {
                self.set_leaf(d, ix);
            }
        }
    }

    fn update_leaves(&mut self, ix: &mut usize, letter_no: i32, codes: &Codes, next_codes: &Codes) {
        let nx = (*ix + 1) & 1;
        self.first[nx] = None;
        self.last[nx] = None;
        let mut node = self.first[*ix];
        while let Some(n) = node {
            for code in codes {
                let prev_index = (code[0].as_bytes()[0] > b'1') as i32 + 1;
                for next in next_codes {
                    let code_index = if letter_no == 0 {
                        0
                    } else if next[0].as_bytes()[0] <= b'1' {
                        1
                    } else {
                        2
                    };
                    let digits = code[code_index].clone();
                    self.update(n, nx, letter_no, prev_index, code_index as i32, digits.as_bytes(), 0);
                }
            }
            node = self.nodes[n].next[*ix];
        }
        *ix = nx;
    }
}

/// `daitch_mokotoff_coding`: the codes, or None without an encodable character.
fn coding(word: &str) -> Option<Vec<String>> {
    let mut i = 0;
    let mut codes = read_letter(word, &mut i)?;
    let end: Codes = vec![["X".into(), "X".into(), "X".into()]];
    let mut t = Tree { nodes: vec![START.clone()], first: [Some(0), None], last: [None; 2], out: Vec::new() };
    let mut ix = 0;
    let mut letter_no = 0;
    while t.first[ix].is_some() {
        let next = read_letter(word, &mut i);
        t.update_leaves(&mut ix, letter_no, &codes, next.as_deref().unwrap_or(&end));
        let Some(n) = next else { break };
        codes = n;
        letter_no += 1;
    }
    let mut node = t.first[ix];
    while let Some(n) = node {
        t.out.push(String::from_utf8_lossy(&t.nodes[n].soundex).into_owned());
        node = t.nodes[n].next[ix];
    }
    Some(t.out)
}

pub(super) fn daitch_mokotoff(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(match coding(text_arg(&a[0])) {
        None => Value::Null,
        Some(v) => Value::Array(Arc::new(ArrayValue::from_vec(
            DataType::Text,
            v.into_iter().map(Value::text).collect(),
        ))),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn dm_vectors() {
        assert_eq!(super::coding("Augsburg").unwrap(), vec!["054795"]);
        assert_eq!(super::coding("Breuer").unwrap(), vec!["791900"]);
    }
}
