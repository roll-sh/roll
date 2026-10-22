//! Lucene's light and minimal stemmers for European languages (`light_french`,
//! `minimal_german`, `spanish_plural` …) and the German `german_stem` / normalization filters.
//!
//! Lucene writes these as edits on a `char[]` with a length, and their rules index characters
//! relative to the end of the word; they are ported the same way, over a reused `Vec<char>`
//! (the inputs are BMP text, so a `char` is one Java `char`). Each function takes the buffer and
//! length and returns the new length, exactly like `int stem(char[] s, int len)`.
#![allow(clippy::if_same_then_else)] // ported suffix chains keep Lucene's rule order

/// `StemmerUtil.endsWith`.
#[inline]
pub(crate) fn ends_with(s: &[char], len: usize, suffix: &str) -> bool {
    let mut i = len;
    for c in suffix.chars().rev() {
        if i == 0 {
            return false;
        }
        i -= 1;
        if s[i] != c {
            return false;
        }
    }
    true
}

/// `StemmerUtil.startsWith`.
#[inline]
pub(crate) fn starts_with(s: &[char], len: usize, prefix: &str) -> bool {
    for (i, c) in prefix.chars().enumerate() {
        if i >= len || s[i] != c {
            return false;
        }
    }
    true
}

/// `StemmerUtil.delete`: removes the char at `pos`.
#[inline]
pub(crate) fn delete(s: &mut [char], pos: usize, len: usize) -> usize {
    if pos < len {
        s.copy_within(pos + 1..len, pos);
    }
    len - 1
}

/// `StemmerUtil.deleteN`.
#[inline]
pub(crate) fn delete_n(s: &mut [char], pos: usize, len: usize, n: usize) -> usize {
    let mut len = len;
    for _ in 0..n {
        len = delete(s, pos, len);
    }
    len
}

fn is_letter(c: char) -> bool {
    crate::unicode::is_letter(c)
}

// ---------------------------------------------------------------------------------------------
// English

/// `EnglishMinimalStemmer` (`minimal_english`).
pub(crate) fn english_minimal(s: &mut [char], len: usize) -> usize {
    if len < 3 || s[len - 1] != 's' {
        return len;
    }
    match s[len - 2] {
        'u' | 's' => len,
        'e' => {
            if len > 3 && s[len - 3] == 'i' && s[len - 4] != 'a' && s[len - 4] != 'e' {
                s[len - 3] = 'y';
                return len - 2;
            }
            if matches!(s[len - 3], 'i' | 'a' | 'o' | 'e') {
                return len;
            }
            len - 1
        }
        _ => len - 1,
    }
}

/// `EnglishPossessiveFilter`: strips a trailing `'s` (with ASCII, right single or fullwidth
/// apostrophe).
pub(crate) fn english_possessive(term: &mut String) -> bool {
    let mut it = term.chars().rev();
    let (Some(last), Some(prev)) = (it.next(), it.next()) else {
        return false;
    };
    if (last == 's' || last == 'S') && matches!(prev, '\'' | '\u{2019}' | '\u{FF07}') {
        let cut = term.len() - last.len_utf8() - prev.len_utf8();
        term.truncate(cut);
        return true;
    }
    false
}

// ---------------------------------------------------------------------------------------------
// French

/// `FrenchMinimalStemmer` (`minimal_french`).
pub(crate) fn french_minimal(s: &mut [char], mut len: usize) -> usize {
    if len < 6 {
        return len;
    }
    if s[len - 1] == 'x' {
        if s[len - 3] == 'a' && s[len - 2] == 'u' {
            s[len - 2] = 'l';
        }
        return len - 1;
    }
    if s[len - 1] == 's' {
        len -= 1;
    }
    if s[len - 1] == 'r' {
        len -= 1;
    }
    if s[len - 1] == 'e' {
        len -= 1;
    }
    if s[len - 1] == 'é' {
        len -= 1;
    }
    if s[len - 1] == s[len - 2] && is_letter(s[len - 1]) {
        len -= 1;
    }
    len
}

/// `FrenchLightStemmer` (`light_french`, the French analyzer).
pub(crate) fn french_light(s: &mut [char], mut len: usize) -> usize {
    if len > 5 && s[len - 1] == 'x' {
        if s[len - 3] == 'a' && s[len - 2] == 'u' && s[len - 4] != 'e' {
            s[len - 2] = 'l';
        }
        len -= 1;
    }
    if len > 3 && s[len - 1] == 'x' {
        len -= 1;
    }
    if len > 3 && s[len - 1] == 's' {
        len -= 1;
    }
    if len > 9 && ends_with(s, len, "issement") {
        len -= 6;
        s[len - 1] = 'r';
        return french_norm(s, len);
    }
    if len > 8 && ends_with(s, len, "issant") {
        len -= 4;
        s[len - 1] = 'r';
        return french_norm(s, len);
    }
    if len > 6 && ends_with(s, len, "ement") {
        len -= 4;
        if len > 3 && ends_with(s, len, "ive") {
            len -= 1;
            s[len - 1] = 'f';
        }
        return french_norm(s, len);
    }
    if len > 11 && ends_with(s, len, "ficatrice") {
        len -= 5;
        s[len - 2] = 'e';
        s[len - 1] = 'r';
        return french_norm(s, len);
    }
    if len > 10 && ends_with(s, len, "ficateur") {
        len -= 4;
        s[len - 2] = 'e';
        s[len - 1] = 'r';
        return french_norm(s, len);
    }
    if len > 9 && ends_with(s, len, "catrice") {
        len -= 3;
        s[len - 4] = 'q';
        s[len - 3] = 'u';
        s[len - 2] = 'e';
        return french_norm(s, len);
    }
    if len > 8 && ends_with(s, len, "cateur") {
        len -= 2;
        s[len - 4] = 'q';
        s[len - 3] = 'u';
        s[len - 2] = 'e';
        s[len - 1] = 'r';
        return french_norm(s, len);
    }
    if len > 8 && ends_with(s, len, "atrice") {
        len -= 4;
        s[len - 2] = 'e';
        s[len - 1] = 'r';
        return french_norm(s, len);
    }
    if len > 7 && ends_with(s, len, "ateur") {
        len -= 3;
        s[len - 2] = 'e';
        s[len - 1] = 'r';
        return french_norm(s, len);
    }
    if len > 6 && ends_with(s, len, "trice") {
        len -= 1;
        s[len - 3] = 'e';
        s[len - 2] = 'u';
        s[len - 1] = 'r';
    }
    if len > 5 && ends_with(s, len, "ième") {
        return french_norm(s, len - 4);
    }
    if len > 7 && ends_with(s, len, "teuse") {
        len -= 2;
        s[len - 1] = 'r';
        return french_norm(s, len);
    }
    if len > 6 && ends_with(s, len, "teur") {
        len -= 1;
        s[len - 1] = 'r';
        return french_norm(s, len);
    }
    if len > 5 && ends_with(s, len, "euse") {
        return french_norm(s, len - 2);
    }
    if len > 8 && ends_with(s, len, "ère") {
        len -= 1;
        s[len - 2] = 'e';
        return french_norm(s, len);
    }
    if len > 7 && ends_with(s, len, "ive") {
        len -= 1;
        s[len - 1] = 'f';
        return french_norm(s, len);
    }
    if len > 4 && (ends_with(s, len, "folle") || ends_with(s, len, "molle")) {
        len -= 2;
        s[len - 1] = 'u';
        return french_norm(s, len);
    }
    if len > 9 && ends_with(s, len, "nnelle") {
        return french_norm(s, len - 5);
    }
    if len > 9 && ends_with(s, len, "nnel") {
        return french_norm(s, len - 3);
    }
    if len > 4 && ends_with(s, len, "ète") {
        len -= 1;
        s[len - 2] = 'e';
    }
    if len > 8 && ends_with(s, len, "ique") {
        len -= 4;
    }
    if len > 8 && ends_with(s, len, "esse") {
        return french_norm(s, len - 3);
    }
    if len > 7 && ends_with(s, len, "inage") {
        return french_norm(s, len - 3);
    }
    if len > 9 && ends_with(s, len, "isation") {
        len -= 7;
        if len > 5 && ends_with(s, len, "ual") {
            s[len - 2] = 'e';
        }
        return french_norm(s, len);
    }
    if len > 9 && ends_with(s, len, "isateur") {
        return french_norm(s, len - 7);
    }
    if len > 8 && ends_with(s, len, "ation") {
        return french_norm(s, len - 5);
    }
    if len > 8 && ends_with(s, len, "ition") {
        return french_norm(s, len - 5);
    }
    french_norm(s, len)
}

fn french_norm(s: &mut [char], mut len: usize) -> usize {
    if len > 4 {
        for c in s[..len].iter_mut() {
            *c = match *c {
                'à' | 'á' | 'â' => 'a',
                'ô' => 'o',
                'è' | 'é' | 'ê' => 'e',
                'ù' | 'û' => 'u',
                'î' => 'i',
                'ç' => 'c',
                c => c,
            };
        }
        let mut ch = s[0];
        let mut i = 1;
        while i < len {
            if s[i] == ch && is_letter(ch) {
                len = delete(s, i, len);
            } else {
                ch = s[i];
                i += 1;
            }
        }
    }
    if len > 4 && ends_with(s, len, "ie") {
        len -= 2;
    }
    if len > 4 {
        if s[len - 1] == 'r' {
            len -= 1;
        }
        if s[len - 1] == 'e' {
            len -= 1;
        }
        if s[len - 1] == 'e' {
            len -= 1;
        }
        if s[len - 1] == s[len - 2] && is_letter(s[len - 1]) {
            len -= 1;
        }
    }
    len
}

// ---------------------------------------------------------------------------------------------
// German

/// `GermanLightStemmer` (`light_german`, the German analyzer).
pub(crate) fn german_light(s: &mut [char], len: usize) -> usize {
    for c in s[..len].iter_mut() {
        *c = match *c {
            'ä' | 'à' | 'á' | 'â' => 'a',
            'ö' | 'ò' | 'ó' | 'ô' => 'o',
            'ï' | 'ì' | 'í' | 'î' => 'i',
            'ü' | 'ù' | 'ú' | 'û' => 'u',
            c => c,
        };
    }
    let len = german_light_step1(s, len);
    german_light_step2(s, len)
}

fn st_ending(c: char) -> bool {
    matches!(c, 'b' | 'd' | 'f' | 'g' | 'h' | 'k' | 'l' | 'm' | 'n' | 't')
}

fn german_light_step1(s: &[char], len: usize) -> usize {
    if len > 5 && s[len - 3] == 'e' && s[len - 2] == 'r' && s[len - 1] == 'n' {
        return len - 3;
    }
    if len > 4 && s[len - 2] == 'e' && matches!(s[len - 1], 'm' | 'n' | 'r' | 's') {
        return len - 2;
    }
    if len > 3 && s[len - 1] == 'e' {
        return len - 1;
    }
    if len > 3 && s[len - 1] == 's' && st_ending(s[len - 2]) {
        return len - 1;
    }
    len
}

fn german_light_step2(s: &[char], len: usize) -> usize {
    if len > 5 && s[len - 3] == 'e' && s[len - 2] == 's' && s[len - 1] == 't' {
        return len - 3;
    }
    if len > 4 && s[len - 2] == 'e' && (s[len - 1] == 'r' || s[len - 1] == 'n') {
        return len - 2;
    }
    if len > 4 && s[len - 2] == 's' && s[len - 1] == 't' && st_ending(s[len - 3]) {
        return len - 2;
    }
    len
}

/// `GermanMinimalStemmer` (`minimal_german`).
pub(crate) fn german_minimal(s: &mut [char], len: usize) -> usize {
    if len < 5 {
        return len;
    }
    for c in s[..len].iter_mut() {
        *c = match *c {
            'ä' => 'a',
            'ö' => 'o',
            'ü' => 'u',
            c => c,
        };
    }
    if len > 6 && s[len - 3] == 'n' && s[len - 2] == 'e' && s[len - 1] == 'n' {
        return len - 3;
    }
    if len > 5 {
        match s[len - 1] {
            'n' if s[len - 2] == 'e' => return len - 2,
            'e' if s[len - 2] == 's' => return len - 2,
            's' if s[len - 2] == 'e' => return len - 2,
            'r' if s[len - 2] == 'e' => return len - 2,
            _ => {}
        }
    }
    match s[len - 1] {
        'n' | 'e' | 's' | 'r' => len - 1,
        _ => len,
    }
}

/// `GermanNormalizationFilter`: umlauts and `ß` folded, `ae`/`oe`/`ue` collapsed after the
/// vowel they mark.
pub(crate) fn german_normalize(s: &mut Vec<char>, mut len: usize) -> usize {
    const N: u8 = 0;
    const V: u8 = 1;
    const U: u8 = 2;
    let mut state = N;
    let mut i = 0;
    while i < len {
        match s[i] {
            'a' | 'o' => state = U,
            'u' => state = if state == N { U } else { V },
            'e' => {
                if state == U {
                    len = delete(s, i, len);
                    state = V;
                    continue;
                }
                state = V;
            }
            'i' | 'q' | 'y' => state = V,
            'ä' => {
                s[i] = 'a';
                state = V;
            }
            'ö' => {
                s[i] = 'o';
                state = V;
            }
            'ü' => {
                s[i] = 'u';
                state = V;
            }
            'ß' => {
                s[i] = 's';
                i += 1;
                if s.len() < len + 1 {
                    s.resize(len + 1, '\0');
                }
                s.copy_within(i..len, i + 1);
                s[i] = 's';
                len += 1;
                state = N;
            }
            _ => state = N,
        }
        i += 1;
    }
    len
}

/// Lucene's `GermanStemmer` (`german_stem`): Caumanns' algorithm over a working copy with
/// placeholder substitutions.
#[derive(Default)]
pub(crate) struct GermanStemmer {
    buf: Vec<char>,
    subst: usize,
}

impl GermanStemmer {
    /// Returns the stem of `term` (None when it is not stemmable or unchanged).
    pub fn stem(&mut self, term: &str, out: &mut String) -> bool {
        let lower = term.to_lowercase();
        // `isStemmable` tests `Character.isLetter(char)` per UTF-16 unit: a surrogate is not a
        // letter, so a term with a supplementary character is left alone.
        if !lower.chars().all(|c| (c as u32) < 0x10000 && is_letter(c)) {
            if lower != term {
                out.clear();
                out.push_str(&lower);
                return true;
            }
            return false;
        }
        self.buf.clear();
        self.buf.extend(lower.chars());
        self.substitute();
        self.strip();
        self.optimize();
        self.resubstitute();
        self.remove_particle_denotion();
        out.clear();
        out.extend(self.buf.iter());
        out != term
    }

    fn ends(&self, s: &str) -> bool {
        ends_with(&self.buf, self.buf.len(), s)
    }

    fn strip(&mut self) {
        let mut more = true;
        while more && self.buf.len() > 3 {
            let n = self.buf.len();
            if n + self.subst > 5 && self.ends("nd") {
                self.buf.truncate(n - 2);
            } else if n + self.subst > 4 && self.ends("em") {
                self.buf.truncate(n - 2);
            } else if n + self.subst > 4 && self.ends("er") {
                self.buf.truncate(n - 2);
            } else if matches!(self.buf[n - 1], 'e' | 's' | 'n' | 't') {
                self.buf.truncate(n - 1);
            } else {
                more = false;
            }
        }
    }

    fn optimize(&mut self) {
        if self.buf.len() > 5 && self.ends("erin*") {
            self.buf.pop();
            self.strip();
        }
        if let Some(last) = self.buf.last_mut()
            && *last == 'z'
        {
            *last = 'x';
        }
    }

    fn remove_particle_denotion(&mut self) {
        let n = self.buf.len();
        if n > 4 {
            for c in 0..n - 3 {
                if self.buf[c..c + 4] == ['g', 'e', 'g', 'e'] {
                    self.buf.drain(c..c + 2);
                    return;
                }
            }
        }
    }

    fn substitute(&mut self) {
        self.subst = 0;
        let b = &mut self.buf;
        let mut c = 0;
        while c < b.len() {
            if c > 0 && b[c] == b[c - 1] {
                b[c] = '*';
            } else if b[c] == 'ä' {
                b[c] = 'a';
            } else if b[c] == 'ö' {
                b[c] = 'o';
            } else if b[c] == 'ü' {
                b[c] = 'u';
            } else if b[c] == 'ß' {
                b[c] = 's';
                b.insert(c + 1, 's');
                self.subst += 1;
            }
            if c + 1 < b.len() {
                if c + 2 < b.len() && b[c] == 's' && b[c + 1] == 'c' && b[c + 2] == 'h' {
                    b[c] = '$';
                    b.drain(c + 1..c + 3);
                    self.subst += 2;
                } else if b[c] == 'c' && b[c + 1] == 'h' {
                    b[c] = '§';
                    b.remove(c + 1);
                    self.subst += 1;
                } else if b[c] == 'e' && b[c + 1] == 'i' {
                    b[c] = '%';
                    b.remove(c + 1);
                    self.subst += 1;
                } else if b[c] == 'i' && b[c + 1] == 'e' {
                    b[c] = '&';
                    b.remove(c + 1);
                    self.subst += 1;
                } else if b[c] == 'i' && b[c + 1] == 'g' {
                    b[c] = '#';
                    b.remove(c + 1);
                    self.subst += 1;
                } else if b[c] == 's' && b[c + 1] == 't' {
                    b[c] = '!';
                    b.remove(c + 1);
                    self.subst += 1;
                }
            }
            c += 1;
        }
    }

    fn resubstitute(&mut self) {
        let b = &mut self.buf;
        let mut c = 0;
        while c < b.len() {
            match b[c] {
                '*' => b[c] = b[c - 1],
                '$' => {
                    b[c] = 's';
                    b.insert(c + 1, 'c');
                    b.insert(c + 2, 'h');
                }
                '§' => {
                    b[c] = 'c';
                    b.insert(c + 1, 'h');
                }
                '%' => {
                    b[c] = 'e';
                    b.insert(c + 1, 'i');
                }
                '&' => {
                    b[c] = 'i';
                    b.insert(c + 1, 'e');
                }
                '#' => {
                    b[c] = 'i';
                    b.insert(c + 1, 'g');
                }
                '!' => {
                    b[c] = 's';
                    b.insert(c + 1, 't');
                }
                _ => {}
            }
            c += 1;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Spanish, Italian, Portuguese

fn fold_vowels(s: &mut [char], len: usize, n_tilde: bool) {
    for c in s[..len].iter_mut() {
        *c = match *c {
            'à' | 'á' | 'â' | 'ä' => 'a',
            'ò' | 'ó' | 'ô' | 'ö' => 'o',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ñ' if n_tilde => 'n',
            c => c,
        };
    }
}

/// `SpanishLightStemmer` (`light_spanish`, the Spanish analyzer).
pub(crate) fn spanish_light(s: &mut [char], len: usize) -> usize {
    if len < 5 {
        return len;
    }
    fold_vowels(s, len, false);
    match s[len - 1] {
        'o' | 'a' | 'e' => len - 1,
        's' => {
            if s[len - 2] == 'e' && s[len - 3] == 's' && s[len - 4] == 'e' {
                return len - 2;
            }
            if s[len - 2] == 'e' && s[len - 3] == 'c' {
                s[len - 3] = 'z';
                return len - 2;
            }
            if matches!(s[len - 2], 'o' | 'a' | 'e') {
                return len - 2;
            }
            len
        }
        _ => len,
    }
}

const SPANISH_INVARIANTS: &[&str] = &[
    "abrebotellas",
    "abrecartas",
    "abrelatas",
    "afueras",
    "albatros",
    "albricias",
    "aledaños",
    "alexis",
    "alicates",
    "analisis",
    "andurriales",
    "antitesis",
    "añicos",
    "apendicitis",
    "apocalipsis",
    "arcoiris",
    "aries",
    "bilis",
    "boletus",
    "boris",
    "brindis",
    "cactus",
    "canutas",
    "caries",
    "cascanueces",
    "cascarrabias",
    "ciempies",
    "cifosis",
    "cortaplumas",
    "corpus",
    "cosmos",
    "cosquillas",
    "creces",
    "crisis",
    "cuatrocientas",
    "cuatrocientos",
    "cuelgacapas",
    "cuentacuentos",
    "cuentapasos",
    "cumpleaños",
    "doscientas",
    "doscientos",
    "dosis",
    "enseres",
    "entonces",
    "esponsales",
    "estatus",
    "exequias",
    "fauces",
    "forceps",
    "fotosintesis",
    "gafas",
    "gafotas",
    "gargaras",
    "gris",
    "honorarios",
    "ictus",
    "jueves",
    "lapsus",
    "lavacoches",
    "lavaplatos",
    "limpiabotas",
    "lunes",
    "maitines",
    "martes",
    "mondadientes",
    "novecientas",
    "novecientos",
    "nupcias",
    "ochocientas",
    "ochocientos",
    "pais",
    "paris",
    "parabrisas",
    "paracaidas",
    "parachoques",
    "paraguas",
    "pararrayos",
    "pisapapeles",
    "piscis",
    "portaaviones",
    "portamaletas",
    "portamantas",
    "quinientas",
    "quinientos",
    "quitamanchas",
    "recogepelotas",
    "rictus",
    "rompeolas",
    "sacacorchos",
    "sacapuntas",
    "saltamontes",
    "salvavidas",
    "seis",
    "seiscientas",
    "seiscientos",
    "setecientas",
    "setecientos",
    "sintesis",
    "tenis",
    "tifus",
    "trabalenguas",
    "vacaciones",
    "venus",
    "versus",
    "viacrucis",
    "virus",
    "viveres",
    "volandas",
];

const SPANISH_SPECIAL: &[&str] = &[
    "yoes",
    "noes",
    "sies",
    "clubes",
    "faralaes",
    "albalaes",
    "itemes",
    "albumes",
    "sandwiches",
    "relojes",
    "bojes",
    "contrarreloj",
    "carcajes",
];

fn in_list_ci(list: &[&str], s: &[char], len: usize) -> bool {
    // Case-insensitive `CharArraySet` lookup (`Character.toLowerCase` per char).
    list.iter().any(|w| {
        let mut it = w.chars();
        for &c in &s[..len] {
            match it.next() {
                Some(x) if x == crate::unicode::to_lower(c) => {}
                _ => return false,
            }
        }
        it.next().is_none()
    })
}

/// `SpanishPluralStemmer` (`spanish_plural`).
pub(crate) fn spanish_plural(s: &mut [char], len: usize) -> usize {
    if len < 4 {
        return len;
    }
    fold_vowels(s, len, false);
    if in_list_ci(SPANISH_INVARIANTS, s, len) {
        return len;
    }
    if in_list_ci(SPANISH_SPECIAL, s, len) {
        return len - 2;
    }
    let v = |c: char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u');
    if s[len - 1] == 's' {
        if !v(s[len - 2]) {
            return len - 1;
        }
        if s[len - 4] == 'q'
            || (s[len - 4] == 'g' && s[len - 3] == 'u' && (s[len - 2] == 'i' || s[len - 2] == 'e'))
        {
            return len - 1;
        }
        if v(s[len - 4]) && s[len - 3] == 'r' && s[len - 2] == 'e' {
            return len - 2;
        }
        if v(s[len - 4]) && matches!(s[len - 3], 'd' | 'l' | 'n' | 'x') && s[len - 2] == 'e' {
            return len - 2;
        }
        if (s[len - 3] == 'y' || s[len - 3] == 'u') && s[len - 2] == 'e' {
            return len - 2;
        }
        if matches!(s[len - 4], 'u' | 'l' | 'r' | 't' | 'n')
            && s[len - 3] == 'i'
            && s[len - 2] == 'e'
        {
            return len - 2;
        }
        if s[len - 3] == 's' && s[len - 2] == 'e' {
            return len - 2;
        }
        if v(s[len - 3]) && s[len - 2] == 'i' {
            s[len - 2] = 'y';
            return len - 1;
        }
        if s[len - 3] == 'd' && s[len - 2] == 'i' {
            s[len - 2] = 'y';
            return len - 1;
        }
        if s[len - 2] == 'e' && s[len - 3] == 'c' {
            s[len - 3] = 'z';
            return len - 2;
        }
        if v(s[len - 2]) {
            return len - 1;
        }
    }
    len
}

/// `ItalianLightStemmer` (`light_italian`, the Italian analyzer).
pub(crate) fn italian_light(s: &mut [char], len: usize) -> usize {
    if len < 6 {
        return len;
    }
    fold_vowels(s, len, false);
    match s[len - 1] {
        'e' => {
            if s[len - 2] == 'i' || s[len - 2] == 'h' {
                len - 2
            } else {
                len - 1
            }
        }
        'i' => {
            if s[len - 2] == 'h' || s[len - 2] == 'i' {
                len - 2
            } else {
                len - 1
            }
        }
        'a' | 'o' => {
            if s[len - 2] == 'i' {
                len - 2
            } else {
                len - 1
            }
        }
        _ => len,
    }
}

/// `PortugueseLightStemmer` (`light_portuguese`, the Portuguese analyzer).
pub(crate) fn portuguese_light(s: &mut [char], mut len: usize) -> usize {
    if len < 4 {
        return len;
    }
    len = pt_remove_suffix(s, len);
    if len > 3 && s[len - 1] == 'a' {
        len = pt_norm_feminine(s, len);
    }
    if len > 4 && matches!(s[len - 1], 'e' | 'a' | 'o') {
        len -= 1;
    }
    for c in s[..len].iter_mut() {
        *c = match *c {
            'à' | 'á' | 'â' | 'ä' | 'ã' => 'a',
            'ò' | 'ó' | 'ô' | 'ö' | 'õ' => 'o',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ç' => 'c',
            c => c,
        };
    }
    len
}

fn pt_remove_suffix(s: &mut [char], len: usize) -> usize {
    if len > 4 && ends_with(s, len, "es") && matches!(s[len - 3], 'r' | 's' | 'l' | 'z') {
        return len - 2;
    }
    if len > 3 && ends_with(s, len, "ns") {
        s[len - 2] = 'm';
        return len - 1;
    }
    if len > 4 && (ends_with(s, len, "eis") || ends_with(s, len, "éis")) {
        s[len - 3] = 'e';
        s[len - 2] = 'l';
        return len - 1;
    }
    if len > 4 && ends_with(s, len, "ais") {
        s[len - 2] = 'l';
        return len - 1;
    }
    if len > 4 && ends_with(s, len, "óis") {
        s[len - 3] = 'o';
        s[len - 2] = 'l';
        return len - 1;
    }
    if len > 4 && ends_with(s, len, "is") {
        s[len - 1] = 'l';
        return len;
    }
    if len > 3 && (ends_with(s, len, "ões") || ends_with(s, len, "ães")) {
        let len = len - 1;
        s[len - 2] = 'ã';
        s[len - 1] = 'o';
        return len;
    }
    if len > 6 && ends_with(s, len, "mente") {
        return len - 5;
    }
    if len > 3 && s[len - 1] == 's' {
        return len - 1;
    }
    len
}

fn pt_norm_feminine(s: &mut [char], len: usize) -> usize {
    if len > 7
        && (ends_with(s, len, "inha") || ends_with(s, len, "iaca") || ends_with(s, len, "eira"))
    {
        s[len - 1] = 'o';
        return len;
    }
    if len > 6 {
        if ["osa", "ica", "ida", "ada", "iva", "ama"]
            .iter()
            .any(|x| ends_with(s, len, x))
        {
            s[len - 1] = 'o';
            return len;
        }
        if ends_with(s, len, "ona") {
            s[len - 3] = 'ã';
            s[len - 2] = 'o';
            return len - 1;
        }
        if ends_with(s, len, "ora") {
            return len - 1;
        }
        if ends_with(s, len, "esa") {
            s[len - 3] = 'ê';
            return len - 1;
        }
        if ends_with(s, len, "na") {
            s[len - 1] = 'o';
            return len;
        }
    }
    len
}

// ---------------------------------------------------------------------------------------------
// Finnish, Hungarian

fn fi_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}

/// `FinnishLightStemmer` (`light_finnish`).
pub(crate) fn finnish_light(s: &mut [char], mut len: usize) -> usize {
    if len < 4 {
        return len;
    }
    for c in s[..len].iter_mut() {
        *c = match *c {
            'ä' | 'å' => 'a',
            'ö' => 'o',
            c => c,
        };
    }
    len = fi_step1(s, len);
    len = fi_step2(s, len);
    len = fi_step3(s, len);
    len = fi_norm1(s, len);
    fi_norm2(s, len)
}

fn fi_step1(s: &[char], len: usize) -> usize {
    if len > 8 {
        if ends_with(s, len, "kin") {
            return fi_step1(s, len - 3);
        }
        if ends_with(s, len, "ko") {
            return fi_step1(s, len - 2);
        }
    }
    if len > 11 {
        if ends_with(s, len, "dellinen") {
            return len - 8;
        }
        if ends_with(s, len, "dellisuus") {
            return len - 9;
        }
    }
    len
}

fn fi_step2(s: &[char], len: usize) -> usize {
    if len > 5 {
        if ends_with(s, len, "lla") || ends_with(s, len, "tse") || ends_with(s, len, "sti") {
            return len - 3;
        }
        if ends_with(s, len, "ni") {
            return len - 2;
        }
        if ends_with(s, len, "aa") {
            return len - 1;
        }
    }
    len
}

fn fi_step3(s: &mut [char], len: usize) -> usize {
    if len > 8 {
        if ends_with(s, len, "nnen") {
            s[len - 4] = 's';
            return len - 3;
        }
        if ends_with(s, len, "ntena") {
            s[len - 5] = 's';
            return len - 4;
        }
        if ends_with(s, len, "tten") {
            return len - 4;
        }
        if ends_with(s, len, "eiden") {
            return len - 5;
        }
    }
    if len > 6 {
        if ["neen", "niin", "seen", "teen", "inen"]
            .iter()
            .any(|x| ends_with(s, len, x))
        {
            return len - 4;
        }
        if s[len - 3] == 'h' && fi_vowel(s[len - 2]) && s[len - 1] == 'n' {
            return len - 3;
        }
        if ends_with(s, len, "den") {
            s[len - 3] = 's';
            return len - 2;
        }
        if ends_with(s, len, "ksen") {
            s[len - 4] = 's';
            return len - 3;
        }
        if ["ssa", "sta", "lla", "lta", "tta", "ksi", "lle"]
            .iter()
            .any(|x| ends_with(s, len, x))
        {
            return len - 3;
        }
    }
    if len > 5 {
        if ends_with(s, len, "na") || ends_with(s, len, "ne") {
            return len - 2;
        }
        if ends_with(s, len, "nei") {
            return len - 3;
        }
    }
    if len > 4 {
        if ends_with(s, len, "ja") || ends_with(s, len, "ta") {
            return len - 2;
        }
        if s[len - 1] == 'a' {
            return len - 1;
        }
        if s[len - 1] == 'n' && fi_vowel(s[len - 2]) {
            return len - 2;
        }
        if s[len - 1] == 'n' {
            return len - 1;
        }
    }
    len
}

fn fi_norm1(s: &mut [char], len: usize) -> usize {
    if len > 5 && ends_with(s, len, "hde") {
        s[len - 3] = 'k';
        s[len - 2] = 's';
        s[len - 1] = 'i';
    }
    if len > 4 && (ends_with(s, len, "ei") || ends_with(s, len, "at")) {
        return len - 2;
    }
    if len > 3 && matches!(s[len - 1], 't' | 's' | 'j' | 'e' | 'a' | 'i') {
        return len - 1;
    }
    len
}

fn fi_norm2(s: &mut [char], mut len: usize) -> usize {
    if len > 8 && matches!(s[len - 1], 'e' | 'o' | 'u') {
        len -= 1;
    }
    if len > 4 {
        if s[len - 1] == 'i' {
            len -= 1;
        }
        if len > 4 {
            let mut ch = s[0];
            let mut i = 1;
            while i < len {
                if s[i] == ch && (ch == 'k' || ch == 'p' || ch == 't') {
                    len = delete(s, i, len);
                } else {
                    ch = s[i];
                    i += 1;
                }
            }
        }
    }
    len
}

/// `HungarianLightStemmer` (`light_hungarian`).
pub(crate) fn hungarian_light(s: &mut [char], mut len: usize) -> usize {
    for c in s[..len].iter_mut() {
        *c = match *c {
            'á' => 'a',
            'ë' | 'é' => 'e',
            'í' => 'i',
            'ó' | 'ő' | 'õ' | 'ö' => 'o',
            'ú' | 'ű' | 'ũ' | 'û' | 'ü' => 'u',
            c => c,
        };
    }
    len = hu_remove_case(s, len);
    len = hu_remove_possessive(s, len);
    len = hu_remove_plural(s, len);
    if len > 3 && matches!(s[len - 1], 'a' | 'e' | 'i' | 'o') {
        return len - 1;
    }
    len
}

fn hu_remove_case(s: &[char], len: usize) -> usize {
    if len > 6 && ends_with(s, len, "kent") {
        return len - 4;
    }
    if len > 5 {
        if [
            "nak", "nek", "val", "vel", "ert", "rol", "ban", "ben", "bol", "nal", "nel", "hoz",
            "hez", "tol",
        ]
        .iter()
        .any(|x| ends_with(s, len, x))
        {
            return len - 3;
        }
        if (ends_with(s, len, "al") || ends_with(s, len, "el"))
            && !fi_vowel(s[len - 3])
            && s[len - 3] == s[len - 4]
        {
            return len - 3;
        }
    }
    if len > 4 {
        if [
            "at", "et", "ot", "va", "ve", "ra", "re", "ba", "be", "ul", "ig",
        ]
        .iter()
        .any(|x| ends_with(s, len, x))
        {
            return len - 2;
        }
        if (ends_with(s, len, "on") || ends_with(s, len, "en")) && !fi_vowel(s[len - 3]) {
            return len - 2;
        }
        match s[len - 1] {
            't' | 'n' => return len - 1,
            'a' | 'e' if s[len - 2] == s[len - 3] && !fi_vowel(s[len - 2]) => return len - 2,
            _ => {}
        }
    }
    len
}

fn hu_remove_possessive(s: &[char], len: usize) -> usize {
    if len > 6 {
        if !fi_vowel(s[len - 5])
            && (ends_with(s, len, "atok") || ends_with(s, len, "otok") || ends_with(s, len, "etek"))
        {
            return len - 4;
        }
        if ends_with(s, len, "itek") || ends_with(s, len, "itok") {
            return len - 4;
        }
    }
    if len > 5 {
        if !fi_vowel(s[len - 4])
            && (ends_with(s, len, "unk") || ends_with(s, len, "tok") || ends_with(s, len, "tek"))
        {
            return len - 3;
        }
        if fi_vowel(s[len - 4]) && ends_with(s, len, "juk") {
            return len - 3;
        }
        if ends_with(s, len, "ink") {
            return len - 3;
        }
    }
    if len > 4 {
        if !fi_vowel(s[len - 3])
            && ["am", "em", "om", "ad", "ed", "od", "uk"]
                .iter()
                .any(|x| ends_with(s, len, x))
        {
            return len - 2;
        }
        if fi_vowel(s[len - 3])
            && (ends_with(s, len, "nk") || ends_with(s, len, "ja") || ends_with(s, len, "je"))
        {
            return len - 2;
        }
        if ends_with(s, len, "im") || ends_with(s, len, "id") || ends_with(s, len, "ik") {
            return len - 2;
        }
    }
    if len > 3 {
        match s[len - 1] {
            'a' | 'e' => {
                if !fi_vowel(s[len - 2]) {
                    return len - 1;
                }
            }
            'm' | 'd' => {
                if fi_vowel(s[len - 2]) {
                    return len - 1;
                }
            }
            'i' => return len - 1,
            _ => {}
        }
    }
    len
}

fn hu_remove_plural(s: &[char], len: usize) -> usize {
    if len > 3 && s[len - 1] == 'k' {
        return match s[len - 2] {
            'a' | 'o' | 'e' if len > 4 => len - 2,
            _ => len - 1,
        };
    }
    len
}

// ---------------------------------------------------------------------------------------------
// Swedish, Norwegian

/// `SwedishLightStemmer` (`light_swedish`).
pub(crate) fn swedish_light(s: &mut [char], mut len: usize) -> usize {
    if len > 4 && s[len - 1] == 's' {
        len -= 1;
    }
    if len > 7 && (ends_with(s, len, "elser") || ends_with(s, len, "heten")) {
        return len - 5;
    }
    if len > 6
        && ["arne", "erna", "ande", "else", "aste", "orna", "aren"]
            .iter()
            .any(|x| ends_with(s, len, x))
    {
        return len - 4;
    }
    if len > 5 && ["are", "ast", "het"].iter().any(|x| ends_with(s, len, x)) {
        return len - 3;
    }
    if len > 4
        && ["ar", "er", "or", "en", "at", "te", "et"]
            .iter()
            .any(|x| ends_with(s, len, x))
    {
        return len - 2;
    }
    if len > 3 && matches!(s[len - 1], 't' | 'a' | 'e' | 'n') {
        return len - 1;
    }
    len
}

pub(crate) const BOKMAAL: u8 = 1;
pub(crate) const NYNORSK: u8 = 2;

/// `NorwegianLightStemmer` (`light_norwegian`, `light_nynorsk`).
pub(crate) fn norwegian_light(s: &mut [char], mut len: usize, flags: u8) -> usize {
    let (nb, nn) = (flags & BOKMAAL != 0, flags & NYNORSK != 0);
    let e = |s: &[char], len: usize, x: &str| ends_with(s, len, x);
    if len > 4 && s[len - 1] == 's' {
        len -= 1;
    }
    if len > 7
        && ((e(s, len, "heter") && nb) || (e(s, len, "heten") && nb) || (e(s, len, "heita") && nn))
    {
        return len - 5;
    }
    if len > 8 && nn && (e(s, len, "heiter") || e(s, len, "leiken") || e(s, len, "leikar")) {
        return len - 6;
    }
    if len > 5 && (e(s, len, "dom") || (e(s, len, "het") && nb)) {
        return len - 3;
    }
    if len > 6 && nn && (e(s, len, "heit") || e(s, len, "semd") || e(s, len, "leik")) {
        return len - 4;
    }
    if len > 7 && (e(s, len, "elser") || e(s, len, "elsen")) {
        return len - 5;
    }
    if len > 6
        && ((e(s, len, "ende") && nb)
            || (e(s, len, "ande") && nn)
            || e(s, len, "else")
            || (e(s, len, "este") && nb)
            || (e(s, len, "aste") && nn)
            || (e(s, len, "eren") && nb)
            || (e(s, len, "aren") && nn))
    {
        return len - 4;
    }
    if len > 5
        && ((e(s, len, "ere") && nb)
            || (e(s, len, "are") && nn)
            || (e(s, len, "est") && nb)
            || (e(s, len, "ast") && nn)
            || e(s, len, "ene")
            || (e(s, len, "ane") && nn))
    {
        return len - 3;
    }
    if len > 4
        && (e(s, len, "er")
            || e(s, len, "en")
            || e(s, len, "et")
            || (e(s, len, "ar") && nn)
            || (e(s, len, "st") && nb)
            || e(s, len, "te"))
    {
        return len - 2;
    }
    if len > 3 && matches!(s[len - 1], 'a' | 'e' | 'n') {
        return len - 1;
    }
    len
}

/// `NorwegianMinimalStemmer` (`minimal_norwegian`, `minimal_nynorsk`).
pub(crate) fn norwegian_minimal(s: &mut [char], mut len: usize, flags: u8) -> usize {
    let nn = flags & NYNORSK != 0;
    if len > 4 && s[len - 1] == 's' {
        len -= 1;
    }
    if len > 5 && (ends_with(s, len, "ene") || (ends_with(s, len, "ane") && nn)) {
        return len - 3;
    }
    if len > 4
        && (ends_with(s, len, "er")
            || ends_with(s, len, "en")
            || ends_with(s, len, "et")
            || (ends_with(s, len, "ar") && nn))
    {
        return len - 2;
    }
    if len > 3 && matches!(s[len - 1], 'a' | 'e') {
        return len - 1;
    }
    len
}

// ---------------------------------------------------------------------------------------------
// Russian, Czech, Bulgarian, Latvian

/// `RussianLightStemmer` (`light_russian`).
pub(crate) fn russian_light(s: &mut [char], len: usize) -> usize {
    let len = ru_remove_case(s, len);
    if len > 3 {
        match s[len - 1] {
            'ь' | 'и' => return len - 1,
            'н' if s[len - 2] == 'н' => return len - 1,
            _ => {}
        }
    }
    len
}

fn ru_remove_case(s: &[char], len: usize) -> usize {
    if len > 6 && (ends_with(s, len, "иями") || ends_with(s, len, "оями")) {
        return len - 4;
    }
    if len > 5
        && [
            "иям", "иях", "оях", "ями", "оям", "оьв", "ами", "его", "ему", "ери", "ими", "ого",
            "ому", "ыми", "оев",
        ]
        .iter()
        .any(|x| ends_with(s, len, x))
    {
        return len - 3;
    }
    if len > 4
        && [
            "ая", "яя", "ях", "юю", "ах", "ею", "их", "ия", "ию", "ьв", "ою", "ую", "ям", "ых",
            "ея", "ам", "ем", "ей", "ём", "ев", "ий", "им", "ое", "ой", "ом", "ов", "ые", "ый",
            "ым", "ми",
        ]
        .iter()
        .any(|x| ends_with(s, len, x))
    {
        return len - 2;
    }
    if len > 3
        && matches!(
            s[len - 1],
            'а' | 'е' | 'и' | 'о' | 'у' | 'й' | 'ы' | 'я' | 'ь'
        )
    {
        return len - 1;
    }
    len
}

/// `CzechStemmer` (`czech_stem`, the Czech analyzer).
pub(crate) fn czech(s: &mut [char], len: usize) -> usize {
    let mut len = cz_remove_case(s, len);
    if len > 5 && (ends_with(s, len, "ov") || ends_with(s, len, "in") || ends_with(s, len, "ův")) {
        len -= 2;
    }
    if len > 0 {
        len = cz_normalize(s, len);
    }
    len
}

fn cz_remove_case(s: &[char], len: usize) -> usize {
    if len > 7 && ends_with(s, len, "atech") {
        return len - 5;
    }
    if len > 6
        && (ends_with(s, len, "ětem") || ends_with(s, len, "etem") || ends_with(s, len, "atům"))
    {
        return len - 4;
    }
    if len > 5
        && [
            "ech", "ich", "ích", "ého", "ěmi", "emi", "ému", "ěte", "ete", "ěti", "eti", "ího",
            "iho", "ími", "ímu", "imu", "ách", "ata", "aty", "ých", "ama", "ami", "ové", "ovi",
            "ými",
        ]
        .iter()
        .any(|x| ends_with(s, len, x))
    {
        return len - 3;
    }
    if len > 4
        && [
            "em", "es", "ém", "ím", "ům", "at", "ám", "os", "us", "ým", "mi", "ou",
        ]
        .iter()
        .any(|x| ends_with(s, len, x))
    {
        return len - 2;
    }
    if len > 3
        && matches!(
            s[len - 1],
            'a' | 'e' | 'i' | 'o' | 'u' | 'ů' | 'y' | 'á' | 'é' | 'í' | 'ý' | 'ě'
        )
    {
        return len - 1;
    }
    len
}

fn cz_normalize(s: &mut [char], len: usize) -> usize {
    if ends_with(s, len, "čt") {
        s[len - 2] = 'c';
        s[len - 1] = 'k';
        return len;
    }
    if ends_with(s, len, "št") {
        s[len - 2] = 's';
        s[len - 1] = 'k';
        return len;
    }
    match s[len - 1] {
        'c' | 'č' => {
            s[len - 1] = 'k';
            return len;
        }
        'z' | 'ž' => {
            s[len - 1] = 'h';
            return len;
        }
        _ => {}
    }
    if len > 1 && s[len - 2] == 'e' {
        s[len - 2] = s[len - 1];
        return len - 1;
    }
    if len > 2 && s[len - 2] == 'ů' {
        s[len - 2] = 'o';
    }
    len
}

/// `BulgarianStemmer` (`bulgarian` stemmer and analyzer).
pub(crate) fn bulgarian(s: &mut [char], mut len: usize) -> usize {
    if len < 4 {
        return len;
    }
    if len > 5 && ends_with(s, len, "ища") {
        return len - 3;
    }
    len = bg_remove_article(s, len);
    len = bg_remove_plural(s, len);
    if len > 3 {
        if ends_with(s, len, "я") {
            len -= 1;
        }
        if ends_with(s, len, "а") || ends_with(s, len, "о") || ends_with(s, len, "е") {
            len -= 1;
        }
    }
    if len > 4 && ends_with(s, len, "ен") {
        s[len - 2] = 'н';
        len -= 1;
    }
    if len > 5 && s[len - 2] == 'ъ' {
        s[len - 2] = s[len - 1];
        len -= 1;
    }
    len
}

fn bg_remove_article(s: &[char], len: usize) -> usize {
    if len > 6 && ends_with(s, len, "ият") {
        return len - 3;
    }
    if len > 5
        && ["ът", "то", "те", "та", "ия"]
            .iter()
            .any(|x| ends_with(s, len, x))
    {
        return len - 2;
    }
    if len > 4 && ends_with(s, len, "ят") {
        return len - 2;
    }
    len
}

fn bg_remove_plural(s: &mut [char], len: usize) -> usize {
    if len > 6 {
        if ends_with(s, len, "овци") || ends_with(s, len, "ове") {
            return len - 3;
        }
        if ends_with(s, len, "еве") {
            s[len - 3] = 'й';
            return len - 2;
        }
    }
    if len > 5 {
        if ends_with(s, len, "ища") {
            return len - 3;
        }
        if ends_with(s, len, "та") {
            return len - 2;
        }
        if ends_with(s, len, "ци") {
            s[len - 2] = 'к';
            return len - 1;
        }
        if ends_with(s, len, "зи") {
            s[len - 2] = 'г';
            return len - 1;
        }
        if s[len - 3] == 'е' && s[len - 1] == 'и' {
            s[len - 3] = 'я';
            return len - 1;
        }
    }
    if len > 4 {
        if ends_with(s, len, "си") {
            s[len - 2] = 'х';
            return len - 1;
        }
        if ends_with(s, len, "и") {
            return len - 1;
        }
    }
    len
}

const LV_AFFIXES: &[(&str, usize, bool)] = &[
    ("ajiem", 3, false),
    ("ajai", 3, false),
    ("ajam", 2, false),
    ("ajām", 2, false),
    ("ajos", 2, false),
    ("ajās", 2, false),
    ("iem", 2, true),
    ("ajā", 2, false),
    ("ais", 2, false),
    ("ai", 2, false),
    ("ei", 2, false),
    ("ām", 1, false),
    ("am", 1, false),
    ("ēm", 1, false),
    ("īm", 1, false),
    ("im", 1, false),
    ("um", 1, false),
    ("us", 1, true),
    ("as", 1, false),
    ("ās", 1, false),
    ("es", 1, false),
    ("os", 1, true),
    ("ij", 1, false),
    ("īs", 1, false),
    ("ēs", 1, false),
    ("is", 1, false),
    ("ie", 1, false),
    ("u", 1, true),
    ("a", 1, true),
    ("i", 1, true),
    ("e", 1, false),
    ("ā", 1, false),
    ("ē", 1, false),
    ("ī", 1, false),
    ("ū", 1, false),
    ("o", 1, false),
    ("s", 0, false),
    ("š", 0, false),
];

/// `LatvianStemmer` (`latvian` stemmer and analyzer).
pub(crate) fn latvian(s: &mut [char], len: usize) -> usize {
    let vowels = s[..len]
        .iter()
        .filter(|c| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'ā' | 'ī' | 'ē' | 'ū'))
        .count();
    for &(affix, vc, palatalizes) in LV_AFFIXES {
        let alen = affix.chars().count();
        if vowels > vc && len >= alen + 3 && ends_with(s, len, affix) {
            let len = len - alen;
            return if palatalizes {
                lv_unpalatalize(s, len)
            } else {
                len
            };
        }
    }
    len
}

fn lv_unpalatalize(s: &mut [char], mut len: usize) -> usize {
    if s[len] == 'u' {
        if ends_with(s, len, "kš") {
            len += 1;
            s[len - 2] = 's';
            s[len - 1] = 't';
            return len;
        }
        if ends_with(s, len, "ņņ") {
            s[len - 2] = 'n';
            s[len - 1] = 'n';
            return len;
        }
    }
    if ["pj", "bj", "mj", "vj"]
        .iter()
        .any(|x| ends_with(s, len, x))
    {
        return len - 1;
    }
    for (from, to) in [
        ("šņ", ['s', 'n']),
        ("žņ", ['z', 'n']),
        ("šļ", ['s', 'l']),
        ("žļ", ['z', 'l']),
        ("ļņ", ['l', 'n']),
        ("ļļ", ['l', 'l']),
    ] {
        if ends_with(s, len, from) {
            s[len - 2] = to[0];
            s[len - 1] = to[1];
            return len;
        }
    }
    match s[len - 1] {
        'č' => s[len - 1] = 'c',
        'ļ' => s[len - 1] = 'l',
        'ņ' => s[len - 1] = 'n',
        _ => {}
    }
    len
}

// ---------------------------------------------------------------------------------------------
// Serbian

/// `SerbianNormalizationFilter`: Cyrillic to Latin, Latin diacritics folded (`đ`/`ђ` → `dj`).
pub(crate) fn serbian_normalize(term: &mut String, scratch: &mut String) {
    if term.is_ascii() {
        return;
    }
    scratch.clear();
    for c in term.chars() {
        match c {
            'а' => scratch.push('a'),
            'б' => scratch.push('b'),
            'в' => scratch.push('v'),
            'г' => scratch.push('g'),
            'д' => scratch.push('d'),
            'ђ' | 'đ' => scratch.push_str("dj"),
            'е' => scratch.push('e'),
            'ж' | 'з' | 'ž' => scratch.push('z'),
            'и' => scratch.push('i'),
            'ј' => scratch.push('j'),
            'к' => scratch.push('k'),
            'л' => scratch.push('l'),
            'љ' => scratch.push_str("lj"),
            'м' => scratch.push('m'),
            'н' => scratch.push('n'),
            'њ' => scratch.push_str("nj"),
            'о' => scratch.push('o'),
            'п' => scratch.push('p'),
            'р' => scratch.push('r'),
            'с' => scratch.push('s'),
            'т' => scratch.push('t'),
            'ћ' | 'ц' | 'ч' | 'č' | 'ć' => scratch.push('c'),
            'у' => scratch.push('u'),
            'ф' => scratch.push('f'),
            'х' => scratch.push('h'),
            'џ' => scratch.push_str("dz"),
            'ш' | 'š' => scratch.push('s'),
            c => scratch.push(c),
        }
    }
    std::mem::swap(term, scratch);
}
