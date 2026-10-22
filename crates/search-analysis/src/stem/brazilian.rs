//! Lucene's `BrazilianStemmer` (`brazilian_stem`, the `brazilian` stemmer and analyzer): an
//! RSLP-like algorithm over Portuguese suffixes with R1/R2/RV regions.
//!
//! Ported from Lucene step by step, keeping its quirks: the regions are computed once from the
//! normalized term and not updated as suffixes are removed, and the `-logias` rule matches but
//! leaves the term unchanged. Terms are lowercased (Portuguese locale) and stripped of accents
//! first; words with non-letters, or shorter than 3 or longer than 29 characters, are not stemmed.

#[derive(Default)]
pub(crate) struct BrazilianStemmer {
    ct: String,
    /// `ct` as it was when the regions were computed: R1, R2 and RV are suffixes of it, given
    /// as byte offsets (Lucene keeps them as strings and never updates them).
    orig: String,
    r1: Option<usize>,
    r2: Option<usize>,
    rv: Option<usize>,
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

fn suffix(value: Option<&str>, suffix: &str) -> bool {
    value.is_some_and(|v| v.ends_with(suffix))
}

fn remove_suffix(value: &mut String, suffix: &str) {
    if value.ends_with(suffix) {
        value.truncate(value.len() - suffix.len());
    }
}

fn replace_suffix(value: &mut String, to_replace: &str, change_to: &str) {
    if value.ends_with(to_replace) {
        value.truncate(value.len() - to_replace.len());
        value.push_str(change_to);
    }
}

fn suffix_preceded(value: Option<&str>, suffix: &str, preceded: &str) -> bool {
    match value.and_then(|v| v.strip_suffix(suffix)) {
        Some(rest) => rest.ends_with(preceded),
        None => false,
    }
}

/// The characters of a term being stemmed (fewer than 30) with their byte offsets.
struct Chars {
    c: [char; 32],
    at: [usize; 33],
    n: usize,
}

impl Chars {
    fn of(v: &str) -> Chars {
        let mut ch = Chars {
            c: ['\0'; 32],
            at: [v.len(); 33],
            n: 0,
        };
        for (i, c) in v.char_indices().take(32) {
            ch.c[ch.n] = c;
            ch.at[ch.n] = i;
            ch.n += 1;
        }
        ch
    }
}

/// `getR1`: the region after the first non-vowel following a vowel (a byte offset into `v`).
fn r1_of(v: &str) -> Option<usize> {
    let ch = Chars::of(v);
    let i = ch.n.checked_sub(1)?;
    let mut j = 0;
    while j < i && !is_vowel(ch.c[j]) {
        j += 1;
    }
    if j >= i {
        return None;
    }
    while j < i && is_vowel(ch.c[j]) {
        j += 1;
    }
    if j >= i {
        return None;
    }
    Some(ch.at[j + 1])
}

/// `getRV` (a byte offset into `v`).
fn rv_of(v: &str) -> Option<usize> {
    let ch = Chars::of(v);
    let c = &ch.c;
    let i = ch.n.checked_sub(1)?;
    if i > 0 && !is_vowel(c[1]) {
        let mut j = 2;
        while j < i && !is_vowel(c[j]) {
            j += 1;
        }
        if j < i {
            return Some(ch.at[j + 1]);
        }
    }
    if i > 1 && is_vowel(c[0]) && is_vowel(c[1]) {
        let mut j = 2;
        while j < i && is_vowel(c[j]) {
            j += 1;
        }
        if j < i {
            return Some(ch.at[j + 1]);
        }
    }
    if i > 2 {
        return Some(ch.at[3]);
    }
    None
}

fn fold_accent(c: char) -> char {
    match c {
        'á' | 'â' | 'ã' => 'a',
        'é' | 'ê' => 'e',
        'í' => 'i',
        'ó' | 'ô' | 'õ' => 'o',
        'ú' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        c => c,
    }
}

impl BrazilianStemmer {
    /// The length in UTF-16 units (`String.length()`).
    fn ct_len(&self) -> usize {
        crate::unicode::utf16_len(&self.ct)
    }

    fn region(&self, r: Option<usize>) -> Option<&str> {
        r.map(|i| &self.orig[i..])
    }

    fn rv_len(&self) -> usize {
        self.region(self.rv).map_or(0, |r| r.chars().count())
    }

    /// `changeTerm`: lowercase (Portuguese locale) and strip accents.
    fn change_term(&mut self, value: &str) {
        self.ct.clear();
        if value.contains('Σ') {
            // Final sigma depends on context, which only the whole-string mapping sees.
            let lower = value.to_lowercase();
            self.ct.extend(lower.chars().map(fold_accent));
            return;
        }
        for c in value.chars() {
            self.ct.extend(c.to_lowercase().map(fold_accent));
        }
    }

    fn create_ct(&mut self, term: &str) {
        self.change_term(term);
        if self.ct_len() < 2 {
            return;
        }
        if self
            .ct
            .starts_with(['"', '\'', '-', ',', ';', '.', '?', '!'])
        {
            self.ct.remove(0);
        }
        if self.ct_len() < 2 {
            return;
        }
        if self.ct.ends_with(['-', ',', ';', '.', '?', '!', '\'', '"']) {
            self.ct.pop();
        }
    }

    /// The stem of `term`, or None when Lucene does not index it (`BrazilianStemFilter` then
    /// keeps the term).
    pub fn stem(&mut self, term: &str) -> Option<&str> {
        self.create_ct(term);
        let len = self.ct_len();
        if !(len < 30 && len > 2) {
            return None;
        }
        // `isStemmable` tests `Character.isLetter(char)` per UTF-16 unit (a surrogate is not
        // a letter).
        if !self
            .ct
            .chars()
            .all(|c| (c as u32) < 0x10000 && crate::unicode::is_letter(c))
        {
            return Some(&self.ct);
        }
        self.orig.clear();
        self.orig.push_str(&self.ct);
        self.r1 = r1_of(&self.orig);
        self.r2 = self
            .r1
            .and_then(|r1| r1_of(&self.orig[r1..]).map(|r2| r1 + r2));
        self.rv = rv_of(&self.orig);
        let mut altered = self.step1();
        if !altered {
            altered = self.step2();
        }
        if altered {
            self.step3();
        } else {
            self.step4();
        }
        self.step5();
        Some(&self.ct)
    }

    fn step3(&mut self) {
        if self.rv.is_none() {
            return;
        }
        if suffix(self.region(self.rv), "i") && suffix_preceded(self.region(self.rv), "i", "c") {
            remove_suffix(&mut self.ct, "i");
        }
    }

    fn step4(&mut self) {
        if self.rv.is_none() {
            return;
        }
        for s in ["os", "a", "i", "o"] {
            if suffix(self.region(self.rv), s) {
                remove_suffix(&mut self.ct, s);
                return;
            }
        }
    }

    fn step5(&mut self) {
        if self.rv.is_none() {
            return;
        }
        if suffix(self.region(self.rv), "e") {
            if suffix_preceded(self.region(self.rv), "e", "gu") {
                remove_suffix(&mut self.ct, "e");
                remove_suffix(&mut self.ct, "u");
                return;
            }
            if suffix_preceded(self.region(self.rv), "e", "ci") {
                remove_suffix(&mut self.ct, "e");
                remove_suffix(&mut self.ct, "i");
                return;
            }
            remove_suffix(&mut self.ct, "e");
        }
    }

    fn step1(&mut self) -> bool {
        if suffix(Some(self.ct.as_str()), "uciones") && suffix(self.region(self.r2), "uciones") {
            replace_suffix(&mut self.ct, "uciones", "u");
            return true;
        }

        if self.ct_len() >= 6 {
            if suffix(Some(self.ct.as_str()), "imentos") && suffix(self.region(self.r2), "imentos")
            {
                remove_suffix(&mut self.ct, "imentos");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "amentos") && suffix(self.region(self.r2), "amentos")
            {
                remove_suffix(&mut self.ct, "amentos");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "adores") && suffix(self.region(self.r2), "adores") {
                remove_suffix(&mut self.ct, "adores");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "adoras") && suffix(self.region(self.r2), "adoras") {
                remove_suffix(&mut self.ct, "adoras");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "logias") && suffix(self.region(self.r2), "logias") {
                // Lucene computes `-log` here but discards it.
                return true;
            }
            if suffix(Some(self.ct.as_str()), "encias") && suffix(self.region(self.r2), "encias") {
                replace_suffix(&mut self.ct, "encias", "ente");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "amente") && suffix(self.region(self.r1), "amente") {
                remove_suffix(&mut self.ct, "amente");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "idades") && suffix(self.region(self.r2), "idades") {
                remove_suffix(&mut self.ct, "idades");
                return true;
            }
        }

        if self.ct_len() >= 5 {
            if suffix(Some(self.ct.as_str()), "acoes") && suffix(self.region(self.r2), "acoes") {
                remove_suffix(&mut self.ct, "acoes");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "imento") && suffix(self.region(self.r2), "imento") {
                remove_suffix(&mut self.ct, "imento");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "amento") && suffix(self.region(self.r2), "amento") {
                remove_suffix(&mut self.ct, "amento");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "adora") && suffix(self.region(self.r2), "adora") {
                remove_suffix(&mut self.ct, "adora");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ismos") && suffix(self.region(self.r2), "ismos") {
                remove_suffix(&mut self.ct, "ismos");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "istas") && suffix(self.region(self.r2), "istas") {
                remove_suffix(&mut self.ct, "istas");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "logia") && suffix(self.region(self.r2), "logia") {
                replace_suffix(&mut self.ct, "logia", "log");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ucion") && suffix(self.region(self.r2), "ucion") {
                replace_suffix(&mut self.ct, "ucion", "u");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "encia") && suffix(self.region(self.r2), "encia") {
                replace_suffix(&mut self.ct, "encia", "ente");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "mente") && suffix(self.region(self.r2), "mente") {
                remove_suffix(&mut self.ct, "mente");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "idade") && suffix(self.region(self.r2), "idade") {
                remove_suffix(&mut self.ct, "idade");
                return true;
            }
        }

        if self.ct_len() >= 4 {
            if suffix(Some(self.ct.as_str()), "acao") && suffix(self.region(self.r2), "acao") {
                remove_suffix(&mut self.ct, "acao");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ezas") && suffix(self.region(self.r2), "ezas") {
                remove_suffix(&mut self.ct, "ezas");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "icos") && suffix(self.region(self.r2), "icos") {
                remove_suffix(&mut self.ct, "icos");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "icas") && suffix(self.region(self.r2), "icas") {
                remove_suffix(&mut self.ct, "icas");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ismo") && suffix(self.region(self.r2), "ismo") {
                remove_suffix(&mut self.ct, "ismo");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "avel") && suffix(self.region(self.r2), "avel") {
                remove_suffix(&mut self.ct, "avel");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ivel") && suffix(self.region(self.r2), "ivel") {
                remove_suffix(&mut self.ct, "ivel");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ista") && suffix(self.region(self.r2), "ista") {
                remove_suffix(&mut self.ct, "ista");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "osos") && suffix(self.region(self.r2), "osos") {
                remove_suffix(&mut self.ct, "osos");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "osas") && suffix(self.region(self.r2), "osas") {
                remove_suffix(&mut self.ct, "osas");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ador") && suffix(self.region(self.r2), "ador") {
                remove_suffix(&mut self.ct, "ador");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ivas") && suffix(self.region(self.r2), "ivas") {
                remove_suffix(&mut self.ct, "ivas");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ivos") && suffix(self.region(self.r2), "ivos") {
                remove_suffix(&mut self.ct, "ivos");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "iras")
                && suffix(self.region(self.rv), "iras")
                && suffix_preceded(Some(self.ct.as_str()), "iras", "e")
            {
                replace_suffix(&mut self.ct, "iras", "ir");
                return true;
            }
        }

        if self.ct_len() >= 3 {
            if suffix(Some(self.ct.as_str()), "eza") && suffix(self.region(self.r2), "eza") {
                remove_suffix(&mut self.ct, "eza");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ico") && suffix(self.region(self.r2), "ico") {
                remove_suffix(&mut self.ct, "ico");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ica") && suffix(self.region(self.r2), "ica") {
                remove_suffix(&mut self.ct, "ica");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "oso") && suffix(self.region(self.r2), "oso") {
                remove_suffix(&mut self.ct, "oso");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "osa") && suffix(self.region(self.r2), "osa") {
                remove_suffix(&mut self.ct, "osa");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "iva") && suffix(self.region(self.r2), "iva") {
                remove_suffix(&mut self.ct, "iva");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ivo") && suffix(self.region(self.r2), "ivo") {
                remove_suffix(&mut self.ct, "ivo");
                return true;
            }
            if suffix(Some(self.ct.as_str()), "ira")
                && suffix(self.region(self.rv), "ira")
                && suffix_preceded(Some(self.ct.as_str()), "ira", "e")
            {
                replace_suffix(&mut self.ct, "ira", "ir");
                return true;
            }
        }

        false
    }

    fn step2(&mut self) -> bool {
        if self.rv_len() >= 7 {
            if suffix(self.region(self.rv), "issemos") {
                remove_suffix(&mut self.ct, "issemos");
                return true;
            }
            if suffix(self.region(self.rv), "essemos") {
                remove_suffix(&mut self.ct, "essemos");
                return true;
            }
            if suffix(self.region(self.rv), "assemos") {
                remove_suffix(&mut self.ct, "assemos");
                return true;
            }
            if suffix(self.region(self.rv), "ariamos") {
                remove_suffix(&mut self.ct, "ariamos");
                return true;
            }
            if suffix(self.region(self.rv), "eriamos") {
                remove_suffix(&mut self.ct, "eriamos");
                return true;
            }
            if suffix(self.region(self.rv), "iriamos") {
                remove_suffix(&mut self.ct, "iriamos");
                return true;
            }
        }

        if self.rv_len() >= 6 {
            if suffix(self.region(self.rv), "iremos") {
                remove_suffix(&mut self.ct, "iremos");
                return true;
            }
            if suffix(self.region(self.rv), "eremos") {
                remove_suffix(&mut self.ct, "eremos");
                return true;
            }
            if suffix(self.region(self.rv), "aremos") {
                remove_suffix(&mut self.ct, "aremos");
                return true;
            }
            if suffix(self.region(self.rv), "avamos") {
                remove_suffix(&mut self.ct, "avamos");
                return true;
            }
            if suffix(self.region(self.rv), "iramos") {
                remove_suffix(&mut self.ct, "iramos");
                return true;
            }
            if suffix(self.region(self.rv), "eramos") {
                remove_suffix(&mut self.ct, "eramos");
                return true;
            }
            if suffix(self.region(self.rv), "aramos") {
                remove_suffix(&mut self.ct, "aramos");
                return true;
            }
            if suffix(self.region(self.rv), "asseis") {
                remove_suffix(&mut self.ct, "asseis");
                return true;
            }
            if suffix(self.region(self.rv), "esseis") {
                remove_suffix(&mut self.ct, "esseis");
                return true;
            }
            if suffix(self.region(self.rv), "isseis") {
                remove_suffix(&mut self.ct, "isseis");
                return true;
            }
            if suffix(self.region(self.rv), "arieis") {
                remove_suffix(&mut self.ct, "arieis");
                return true;
            }
            if suffix(self.region(self.rv), "erieis") {
                remove_suffix(&mut self.ct, "erieis");
                return true;
            }
            if suffix(self.region(self.rv), "irieis") {
                remove_suffix(&mut self.ct, "irieis");
                return true;
            }
        }

        if self.rv_len() >= 5 {
            if suffix(self.region(self.rv), "irmos") {
                remove_suffix(&mut self.ct, "irmos");
                return true;
            }
            if suffix(self.region(self.rv), "iamos") {
                remove_suffix(&mut self.ct, "iamos");
                return true;
            }
            if suffix(self.region(self.rv), "armos") {
                remove_suffix(&mut self.ct, "armos");
                return true;
            }
            if suffix(self.region(self.rv), "ermos") {
                remove_suffix(&mut self.ct, "ermos");
                return true;
            }
            if suffix(self.region(self.rv), "areis") {
                remove_suffix(&mut self.ct, "areis");
                return true;
            }
            if suffix(self.region(self.rv), "ereis") {
                remove_suffix(&mut self.ct, "ereis");
                return true;
            }
            if suffix(self.region(self.rv), "ireis") {
                remove_suffix(&mut self.ct, "ireis");
                return true;
            }
            if suffix(self.region(self.rv), "asses") {
                remove_suffix(&mut self.ct, "asses");
                return true;
            }
            if suffix(self.region(self.rv), "esses") {
                remove_suffix(&mut self.ct, "esses");
                return true;
            }
            if suffix(self.region(self.rv), "isses") {
                remove_suffix(&mut self.ct, "isses");
                return true;
            }
            if suffix(self.region(self.rv), "astes") {
                remove_suffix(&mut self.ct, "astes");
                return true;
            }
            if suffix(self.region(self.rv), "assem") {
                remove_suffix(&mut self.ct, "assem");
                return true;
            }
            if suffix(self.region(self.rv), "essem") {
                remove_suffix(&mut self.ct, "essem");
                return true;
            }
            if suffix(self.region(self.rv), "issem") {
                remove_suffix(&mut self.ct, "issem");
                return true;
            }
            if suffix(self.region(self.rv), "ardes") {
                remove_suffix(&mut self.ct, "ardes");
                return true;
            }
            if suffix(self.region(self.rv), "erdes") {
                remove_suffix(&mut self.ct, "erdes");
                return true;
            }
            if suffix(self.region(self.rv), "irdes") {
                remove_suffix(&mut self.ct, "irdes");
                return true;
            }
            if suffix(self.region(self.rv), "ariam") {
                remove_suffix(&mut self.ct, "ariam");
                return true;
            }
            if suffix(self.region(self.rv), "eriam") {
                remove_suffix(&mut self.ct, "eriam");
                return true;
            }
            if suffix(self.region(self.rv), "iriam") {
                remove_suffix(&mut self.ct, "iriam");
                return true;
            }
            if suffix(self.region(self.rv), "arias") {
                remove_suffix(&mut self.ct, "arias");
                return true;
            }
            if suffix(self.region(self.rv), "erias") {
                remove_suffix(&mut self.ct, "erias");
                return true;
            }
            if suffix(self.region(self.rv), "irias") {
                remove_suffix(&mut self.ct, "irias");
                return true;
            }
            if suffix(self.region(self.rv), "estes") {
                remove_suffix(&mut self.ct, "estes");
                return true;
            }
            if suffix(self.region(self.rv), "istes") {
                remove_suffix(&mut self.ct, "istes");
                return true;
            }
            if suffix(self.region(self.rv), "areis") {
                remove_suffix(&mut self.ct, "areis");
                return true;
            }
            if suffix(self.region(self.rv), "aveis") {
                remove_suffix(&mut self.ct, "aveis");
                return true;
            }
        }

        if self.rv_len() >= 4 {
            if suffix(self.region(self.rv), "aria") {
                remove_suffix(&mut self.ct, "aria");
                return true;
            }
            if suffix(self.region(self.rv), "eria") {
                remove_suffix(&mut self.ct, "eria");
                return true;
            }
            if suffix(self.region(self.rv), "iria") {
                remove_suffix(&mut self.ct, "iria");
                return true;
            }
            if suffix(self.region(self.rv), "asse") {
                remove_suffix(&mut self.ct, "asse");
                return true;
            }
            if suffix(self.region(self.rv), "esse") {
                remove_suffix(&mut self.ct, "esse");
                return true;
            }
            if suffix(self.region(self.rv), "isse") {
                remove_suffix(&mut self.ct, "isse");
                return true;
            }
            if suffix(self.region(self.rv), "aste") {
                remove_suffix(&mut self.ct, "aste");
                return true;
            }
            if suffix(self.region(self.rv), "este") {
                remove_suffix(&mut self.ct, "este");
                return true;
            }
            if suffix(self.region(self.rv), "iste") {
                remove_suffix(&mut self.ct, "iste");
                return true;
            }
            if suffix(self.region(self.rv), "arei") {
                remove_suffix(&mut self.ct, "arei");
                return true;
            }
            if suffix(self.region(self.rv), "erei") {
                remove_suffix(&mut self.ct, "erei");
                return true;
            }
            if suffix(self.region(self.rv), "irei") {
                remove_suffix(&mut self.ct, "irei");
                return true;
            }
            if suffix(self.region(self.rv), "aram") {
                remove_suffix(&mut self.ct, "aram");
                return true;
            }
            if suffix(self.region(self.rv), "eram") {
                remove_suffix(&mut self.ct, "eram");
                return true;
            }
            if suffix(self.region(self.rv), "iram") {
                remove_suffix(&mut self.ct, "iram");
                return true;
            }
            if suffix(self.region(self.rv), "avam") {
                remove_suffix(&mut self.ct, "avam");
                return true;
            }
            if suffix(self.region(self.rv), "arem") {
                remove_suffix(&mut self.ct, "arem");
                return true;
            }
            if suffix(self.region(self.rv), "erem") {
                remove_suffix(&mut self.ct, "erem");
                return true;
            }
            if suffix(self.region(self.rv), "irem") {
                remove_suffix(&mut self.ct, "irem");
                return true;
            }
            if suffix(self.region(self.rv), "ando") {
                remove_suffix(&mut self.ct, "ando");
                return true;
            }
            if suffix(self.region(self.rv), "endo") {
                remove_suffix(&mut self.ct, "endo");
                return true;
            }
            if suffix(self.region(self.rv), "indo") {
                remove_suffix(&mut self.ct, "indo");
                return true;
            }
            if suffix(self.region(self.rv), "arao") {
                remove_suffix(&mut self.ct, "arao");
                return true;
            }
            if suffix(self.region(self.rv), "erao") {
                remove_suffix(&mut self.ct, "erao");
                return true;
            }
            if suffix(self.region(self.rv), "irao") {
                remove_suffix(&mut self.ct, "irao");
                return true;
            }
            if suffix(self.region(self.rv), "adas") {
                remove_suffix(&mut self.ct, "adas");
                return true;
            }
            if suffix(self.region(self.rv), "idas") {
                remove_suffix(&mut self.ct, "idas");
                return true;
            }
            if suffix(self.region(self.rv), "aras") {
                remove_suffix(&mut self.ct, "aras");
                return true;
            }
            if suffix(self.region(self.rv), "eras") {
                remove_suffix(&mut self.ct, "eras");
                return true;
            }
            if suffix(self.region(self.rv), "iras") {
                remove_suffix(&mut self.ct, "iras");
                return true;
            }
            if suffix(self.region(self.rv), "avas") {
                remove_suffix(&mut self.ct, "avas");
                return true;
            }
            if suffix(self.region(self.rv), "ares") {
                remove_suffix(&mut self.ct, "ares");
                return true;
            }
            if suffix(self.region(self.rv), "eres") {
                remove_suffix(&mut self.ct, "eres");
                return true;
            }
            if suffix(self.region(self.rv), "ires") {
                remove_suffix(&mut self.ct, "ires");
                return true;
            }
            if suffix(self.region(self.rv), "ados") {
                remove_suffix(&mut self.ct, "ados");
                return true;
            }
            if suffix(self.region(self.rv), "idos") {
                remove_suffix(&mut self.ct, "idos");
                return true;
            }
            if suffix(self.region(self.rv), "amos") {
                remove_suffix(&mut self.ct, "amos");
                return true;
            }
            if suffix(self.region(self.rv), "emos") {
                remove_suffix(&mut self.ct, "emos");
                return true;
            }
            if suffix(self.region(self.rv), "imos") {
                remove_suffix(&mut self.ct, "imos");
                return true;
            }
            if suffix(self.region(self.rv), "iras") {
                remove_suffix(&mut self.ct, "iras");
                return true;
            }
            if suffix(self.region(self.rv), "ieis") {
                remove_suffix(&mut self.ct, "ieis");
                return true;
            }
        }

        if self.rv_len() >= 3 {
            if suffix(self.region(self.rv), "ada") {
                remove_suffix(&mut self.ct, "ada");
                return true;
            }
            if suffix(self.region(self.rv), "ida") {
                remove_suffix(&mut self.ct, "ida");
                return true;
            }
            if suffix(self.region(self.rv), "ara") {
                remove_suffix(&mut self.ct, "ara");
                return true;
            }
            if suffix(self.region(self.rv), "era") {
                remove_suffix(&mut self.ct, "era");
                return true;
            }
            if suffix(self.region(self.rv), "ira") {
                remove_suffix(&mut self.ct, "ava");
                return true;
            }
            if suffix(self.region(self.rv), "iam") {
                remove_suffix(&mut self.ct, "iam");
                return true;
            }
            if suffix(self.region(self.rv), "ado") {
                remove_suffix(&mut self.ct, "ado");
                return true;
            }
            if suffix(self.region(self.rv), "ido") {
                remove_suffix(&mut self.ct, "ido");
                return true;
            }
            if suffix(self.region(self.rv), "ias") {
                remove_suffix(&mut self.ct, "ias");
                return true;
            }
            if suffix(self.region(self.rv), "ais") {
                remove_suffix(&mut self.ct, "ais");
                return true;
            }
            if suffix(self.region(self.rv), "eis") {
                remove_suffix(&mut self.ct, "eis");
                return true;
            }
            if suffix(self.region(self.rv), "ira") {
                remove_suffix(&mut self.ct, "ira");
                return true;
            }
            if suffix(self.region(self.rv), "ear") {
                remove_suffix(&mut self.ct, "ear");
                return true;
            }
        }

        if self.rv_len() >= 2 {
            if suffix(self.region(self.rv), "ia") {
                remove_suffix(&mut self.ct, "ia");
                return true;
            }
            if suffix(self.region(self.rv), "ei") {
                remove_suffix(&mut self.ct, "ei");
                return true;
            }
            if suffix(self.region(self.rv), "am") {
                remove_suffix(&mut self.ct, "am");
                return true;
            }
            if suffix(self.region(self.rv), "em") {
                remove_suffix(&mut self.ct, "em");
                return true;
            }
            if suffix(self.region(self.rv), "ar") {
                remove_suffix(&mut self.ct, "ar");
                return true;
            }
            if suffix(self.region(self.rv), "er") {
                remove_suffix(&mut self.ct, "er");
                return true;
            }
            if suffix(self.region(self.rv), "ir") {
                remove_suffix(&mut self.ct, "ir");
                return true;
            }
            if suffix(self.region(self.rv), "as") {
                remove_suffix(&mut self.ct, "as");
                return true;
            }
            if suffix(self.region(self.rv), "es") {
                remove_suffix(&mut self.ct, "es");
                return true;
            }
            if suffix(self.region(self.rv), "is") {
                remove_suffix(&mut self.ct, "is");
                return true;
            }
            if suffix(self.region(self.rv), "eu") {
                remove_suffix(&mut self.ct, "eu");
                return true;
            }
            if suffix(self.region(self.rv), "iu") {
                remove_suffix(&mut self.ct, "iu");
                return true;
            }
            if suffix(self.region(self.rv), "iu") {
                remove_suffix(&mut self.ct, "iu");
                return true;
            }
            if suffix(self.region(self.rv), "ou") {
                remove_suffix(&mut self.ct, "ou");
                return true;
            }
        }

        false
    }
}
