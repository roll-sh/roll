//! `java.util.Locale` semantics: the parts of a locale, BCP 47 language tags in both
//! directions, `toString`, the legacy constructor rules, `Locale.Builder` validation, and the
//! English display names of languages, scripts, regions and variants.
//!
//! Script values carry a locale as its `toString()` form (`sr_RS_#Latn_u-nu-latn-x-priv`), which
//! is what `equals` compares and what locale data is keyed by; [`Loc`] parses that form back into
//! its parts when a method needs them. The rules follow the JDK Elasticsearch 9.5.5 runs:
//! deprecated language codes become their new ones (`iw` → `he`), `ja_JP_JP` and `th_TH_TH`
//! gain their compatibility extensions (`u-ca-japanese`, `u-nu-thai`), tags parse leniently (the
//! first ill-formed subtag ends the tag), `x-lvariant-…` carries variants that are not BCP 47
//! variants, and extensions print in key order with private use last.
//!
//! Display names (`locale_names.tsv`) were captured from that JDK in English, the default
//! display locale; other display locales get the same English names.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

/// Marks a stored locale key that is a language tag rather than a `toString()` form.
const TAG_MARK: char = '\u{1}';

/// A locale's parts. `ext` maps an extension singleton to its canonical value (`'u'` →
/// `"ca-japanese"`, `'x'` → private use).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loc {
    pub lang: String,
    pub script: String,
    pub region: String,
    pub variant: String,
    pub ext: BTreeMap<char, String>,
}

fn is_alpha(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic())
}
fn is_alnum(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn is_digit(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

pub fn is_language(s: &str) -> bool {
    (2..=8).contains(&s.len()) && is_alpha(s)
}
fn is_extlang(s: &str) -> bool {
    s.len() == 3 && is_alpha(s)
}
pub fn is_script(s: &str) -> bool {
    s.len() == 4 && is_alpha(s)
}
pub fn is_region(s: &str) -> bool {
    (s.len() == 2 && is_alpha(s)) || (s.len() == 3 && is_digit(s))
}
pub fn is_variant(s: &str) -> bool {
    ((5..=8).contains(&s.len()) && is_alnum(s)) || (s.len() == 4 && s.as_bytes()[0].is_ascii_digit() && is_alnum(s))
}
fn is_extension_singleton(s: &str) -> bool {
    s.len() == 1 && is_alnum(s) && !s.eq_ignore_ascii_case("x")
}
fn is_extension_subtag(s: &str) -> bool {
    (2..=8).contains(&s.len()) && is_alnum(s)
}
fn is_privateuse_subtag(s: &str) -> bool {
    (1..=8).contains(&s.len()) && is_alnum(s)
}

fn title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        out.push(if i == 0 { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() });
    }
    out
}

/// `Locale.convertOldISOCodes` (JDK 17+: the new codes win).
fn convert_old(lang: &str) -> String {
    match lang {
        "iw" => "he".into(),
        "ji" => "yi".into(),
        "in" => "id".into(),
        _ => lang.into(),
    }
}

/// Canonical `u` extension value: attributes first (sorted, deduplicated), then keywords by key.
fn canonical_unicode(value: &str) -> String {
    let mut attrs: Vec<String> = Vec::new();
    let mut keywords: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for sub in value.split('-').map(|s| s.to_ascii_lowercase()) {
        if sub.len() == 2 {
            keywords.entry(sub.clone()).or_default();
            current = Some(sub);
        } else if let Some(k) = &current {
            keywords.get_mut(k).unwrap().push(sub);
        } else if !attrs.contains(&sub) {
            attrs.push(sub);
        }
    }
    attrs.sort();
    let mut parts: Vec<String> = attrs;
    for (k, v) in keywords {
        parts.push(k);
        parts.extend(v);
    }
    parts.join("-")
}

impl Loc {
    /// `new Locale(language, country, variant)`.
    pub fn new(lang: &str, country: &str, variant: &str) -> Loc {
        let mut l = Loc { lang: convert_old(&lang.to_ascii_lowercase()), script: String::new(), region: country.to_ascii_uppercase(), variant: variant.to_string(), ext: BTreeMap::new() };
        l.compat();
        l
    }

    /// `Locale.getCompatibilityExtensions`: the two legacy locales whose variant implied an
    /// extension.
    fn compat(&mut self) {
        if !self.script.is_empty() || !self.ext.is_empty() {
            return;
        }
        match (self.lang.as_str(), self.region.as_str(), self.variant.as_str()) {
            ("ja", "JP", "JP") => {
                self.ext.insert('u', "ca-japanese".into());
            }
            ("th", "TH", "TH") => {
                self.ext.insert('u', "nu-thai".into());
            }
            _ => {}
        }
    }

    /// The extension ID (`u-ca-japanese-x-priv`): keys in order, private use last.
    pub fn ext_id(&self) -> String {
        let mut parts: Vec<String> = self.ext.iter().filter(|(k, _)| **k != 'x').map(|(k, v)| format!("{k}-{v}")).collect();
        if let Some(x) = self.ext.get(&'x') {
            parts.push(format!("x-{x}"));
        }
        parts.join("-")
    }

    /// `Locale.toString()`.
    pub fn to_java_string(&self) -> String {
        let (l, s, r, v, e) = (!self.lang.is_empty(), !self.script.is_empty(), !self.region.is_empty(), !self.variant.is_empty(), !self.ext.is_empty());
        let mut out = self.lang.clone();
        if r || (l && (v || s || e)) {
            out.push('_');
            out.push_str(&self.region);
        }
        if v && (l || r) {
            out.push('_');
            out.push_str(&self.variant);
        }
        if s && (l || r) {
            out.push_str("_#");
            out.push_str(&self.script);
        }
        if e && (l || r) {
            out.push('_');
            if !s {
                out.push('#');
            }
            out.push_str(&self.ext_id());
        }
        out
    }

    /// The string a script value stores: `toString()`, or, when `toString()` would drop parts
    /// (a script, variant or extensions without language and region), a marked language tag.
    pub fn key(&self) -> String {
        if self.lang.is_empty() && self.region.is_empty() && (!self.script.is_empty() || !self.variant.is_empty() || !self.ext.is_empty()) {
            format!("{TAG_MARK}{}", self.to_tag())
        } else {
            self.to_java_string()
        }
    }

    /// `toString()` of a stored key.
    pub fn display_key(key: &str) -> std::borrow::Cow<'_, str> {
        if key.starts_with(TAG_MARK) { Loc::parse(key).to_java_string().into() } else { key.into() }
    }

    /// Parses a stored key (a `toString()` form, see [`Loc::key`]) back into its parts.
    pub fn parse(s: &str) -> Loc {
        if let Some(tag) = s.strip_prefix(TAG_MARK) {
            return Loc::from_tag(tag);
        }
        let mut l = Loc::default();
        let (base, hash) = match s.find("_#") {
            Some(i) => (&s[..i], Some(&s[i + 2..])),
            None => (s, None),
        };
        let mut it = base.splitn(3, '_');
        l.lang = it.next().unwrap_or("").to_string();
        l.region = it.next().unwrap_or("").to_string();
        l.variant = it.next().unwrap_or("").to_string();
        if let Some(h) = hash {
            let (script, ext) = if h.len() >= 4 && is_script(&h[..4]) && (h.len() == 4 || h.as_bytes()[4] == b'_') {
                (&h[..4], h.get(5..).unwrap_or(""))
            } else {
                ("", h)
            };
            l.script = script.to_string();
            l.ext = parse_extensions(ext);
        }
        l
    }

    /// `Locale.forLanguageTag`.
    pub fn from_tag(tag: &str) -> Loc {
        let lower = tag.to_ascii_lowercase();
        let tag = match GRANDFATHERED.iter().find(|(g, _)| *g == lower) {
            Some((_, pref)) => pref.to_string(),
            None => tag.to_string(),
        };
        let subs: Vec<&str> = tag.split('-').collect();
        let mut i = 0;
        let mut l = Loc::default();
        let mut extlangs: Vec<String> = Vec::new();
        let mut variants: Vec<String> = Vec::new();
        let mut privateuse = String::new();
        let at = |i: usize| subs.get(i).copied().unwrap_or("");
        if is_language(at(i)) {
            l.lang = at(i).to_ascii_lowercase();
            i += 1;
            if l.lang.len() <= 3 {
                while extlangs.len() < 3 && is_extlang(at(i)) {
                    extlangs.push(at(i).to_ascii_lowercase());
                    i += 1;
                }
            }
            if is_script(at(i)) {
                l.script = title(at(i));
                i += 1;
            }
            if is_region(at(i)) {
                l.region = at(i).to_ascii_uppercase();
                i += 1;
            }
            while is_variant(at(i)) {
                variants.push(at(i).to_string());
                i += 1;
            }
            while is_extension_singleton(at(i)) {
                let key = at(i).to_ascii_lowercase().chars().next().unwrap();
                if l.ext.contains_key(&key) {
                    break;
                }
                let start = i + 1;
                let mut j = start;
                while is_extension_subtag(at(j)) {
                    j += 1;
                }
                if j == start {
                    break;
                }
                let value = subs[start..j].join("-").to_ascii_lowercase();
                l.ext.insert(key, if key == 'u' { canonical_unicode(&value) } else { value });
                i = j;
            }
        }
        if at(i).eq_ignore_ascii_case("x") {
            let start = i + 1;
            let mut j = start;
            while is_privateuse_subtag(at(j)) {
                j += 1;
            }
            if j > start {
                privateuse = subs[start..j].join("-");
            }
        }
        if let Some(first) = extlangs.first() {
            l.lang = first.clone();
        }
        if l.lang == "und" {
            l.lang.clear();
        }
        l.lang = convert_old(&l.lang);
        // `x-lvariant-…` carries variants that are not BCP 47 variants
        if !privateuse.is_empty() {
            let parts: Vec<&str> = privateuse.split('-').collect();
            if let Some(p) = parts.iter().position(|s| s.eq_ignore_ascii_case("lvariant")) {
                variants.extend(parts[p + 1..].iter().map(|s| s.to_string()));
                privateuse = parts[..p].join("-");
            }
        }
        if !privateuse.is_empty() {
            l.ext.insert('x', privateuse.to_ascii_lowercase());
        }
        l.variant = variants.join("_");
        l.compat();
        l
    }

    /// `Locale.toLanguageTag()`.
    pub fn to_tag(&self) -> String {
        let mut lang = if is_language(&self.lang) { convert_old(&self.lang) } else { String::new() };
        let script = if is_script(&self.script) { title(&self.script) } else { String::new() };
        let region = if is_region(&self.region) { self.region.to_ascii_uppercase() } else { String::new() };
        let mut has_subtag = !script.is_empty() || !region.is_empty();
        let mut base_variant = self.variant.clone();
        if lang == "no" && region == "NO" && base_variant == "NY" {
            lang = "nn".into();
            base_variant.clear();
        }
        let mut variants: Vec<&str> = Vec::new();
        let mut privuse_var: Option<String> = None;
        if !base_variant.is_empty() {
            let subs: Vec<&str> = base_variant.split(['_', '-']).collect();
            let mut k = 0;
            while k < subs.len() && is_variant(subs[k]) {
                variants.push(subs[k]);
                k += 1;
            }
            if !variants.is_empty() {
                has_subtag = true;
            }
            let rest: Vec<&str> = subs[k..].iter().copied().take_while(|s| is_privateuse_subtag(s)).collect();
            if !rest.is_empty() {
                privuse_var = Some(rest.join("-"));
            }
        }
        let mut extensions: Vec<String> = Vec::new();
        let mut privateuse: Option<String> = None;
        for (k, v) in &self.ext {
            if *k == 'x' {
                privateuse = Some(v.clone());
            } else {
                extensions.push(format!("{k}-{v}"));
            }
        }
        if !extensions.is_empty() {
            has_subtag = true;
        }
        if let Some(pv) = privuse_var {
            privateuse = Some(match privateuse {
                None => format!("lvariant-{pv}"),
                Some(p) => format!("{p}-lvariant-{pv}"),
            });
        }
        if lang.is_empty() && (has_subtag || privateuse.is_none()) {
            lang = "und".into();
        }
        let mut out = lang;
        for s in [script, region] {
            if !s.is_empty() {
                out.push('-');
                out.push_str(&s);
            }
        }
        for v in variants {
            out.push('-');
            out.push_str(v);
        }
        for e in extensions {
            out.push('-');
            out.push_str(&e);
        }
        if let Some(p) = privateuse {
            if !out.is_empty() {
                out.push('-');
            }
            out.push_str("x-");
            out.push_str(&p);
        }
        out
    }

    /// `Locale.hashCode()`: the base locale's hash (case-folded except the variant) xor the
    /// extension ID's string hash.
    pub fn hash(&self) -> i32 {
        let mut h: i32 = 0;
        for s in [&self.lang, &self.script, &self.region] {
            for c in s.encode_utf16() {
                h = h.wrapping_mul(31).wrapping_add(if (b'A' as u16..=b'Z' as u16).contains(&c) { c + 32 } else { c } as i32);
            }
        }
        for c in self.variant.encode_utf16() {
            h = h.wrapping_mul(31).wrapping_add(c as i32);
        }
        if !self.ext.is_empty() {
            h ^= crate::java::string_hash(&self.ext_id());
        }
        h
    }

    /// The Unicode locale keywords (`u` extension) as `key → type`.
    pub fn unicode_keywords(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        if let Some(u) = self.ext.get(&'u') {
            for sub in u.split('-') {
                if sub.len() == 2 {
                    out.push((sub.to_string(), String::new()));
                } else if let Some(last) = out.last_mut() {
                    if !last.1.is_empty() {
                        last.1.push('-');
                    }
                    last.1.push_str(sub);
                }
            }
        }
        out
    }

    /// The Unicode locale attributes (`u` subtags before the first key).
    pub fn unicode_attributes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(u) = self.ext.get(&'u') {
            for sub in u.split('-') {
                if sub.len() == 2 {
                    break;
                }
                out.push(sub.to_string());
            }
        }
        out
    }

    /// `getDisplayName()` in English: the language, then script, region, variants and the
    /// display names of known Unicode keywords in parentheses.
    pub fn display_name(&self) -> String {
        let lang = self.display_language();
        let mut qualifiers: Vec<String> = Vec::new();
        for s in [self.display_script(), self.display_country()] {
            if !s.is_empty() {
                qualifiers.push(s);
            }
        }
        for v in self.variant.split('_').filter(|v| !v.is_empty()) {
            qualifiers.push(display_variant(v));
        }
        for (k, t) in self.unicode_keywords() {
            if let Some(n) = keyword_display(&k, &t) {
                qualifiers.push(n.to_string());
            }
        }
        if lang.is_empty() {
            if qualifiers.is_empty() {
                return String::new();
            }
            let first = qualifiers.remove(0);
            if qualifiers.is_empty() {
                return first;
            }
            return format!("{first} ({})", qualifiers.join(", "));
        }
        if qualifiers.is_empty() {
            lang
        } else {
            format!("{lang} ({})", qualifiers.join(", "))
        }
    }

    pub fn display_language(&self) -> String {
        if self.lang.is_empty() {
            return String::new();
        }
        names().langs.get(self.lang.as_str()).map(|(_, n, _)| n.to_string()).unwrap_or_else(|| self.lang.clone())
    }

    pub fn display_country(&self) -> String {
        if self.region.is_empty() {
            return String::new();
        }
        names().countries.get(self.region.as_str()).map(|(_, n, _)| n.to_string()).unwrap_or_else(|| self.region.clone())
    }

    pub fn display_script(&self) -> String {
        if self.script.is_empty() {
            return String::new();
        }
        names().scripts.get(self.script.as_str()).map(|n| n.to_string()).unwrap_or_else(|| self.script.clone())
    }

    pub fn display_variant(&self) -> String {
        self.variant.split('_').filter(|v| !v.is_empty()).map(display_variant).collect::<Vec<_>>().join(", ")
    }

    /// `getISO3Language()`: `None` when there is no three-letter code (the JDK throws
    /// `MissingResourceException`).
    pub fn iso3_language(&self) -> Option<String> {
        if self.lang.len() == 3 || self.lang.is_empty() {
            return Some(self.lang.clone());
        }
        names().langs.get(self.lang.as_str()).filter(|(i3, _, _)| !i3.is_empty()).map(|(i3, _, _)| i3.to_string())
    }

    pub fn iso3_country(&self) -> Option<String> {
        if self.region.is_empty() {
            return Some(String::new());
        }
        names().countries.get(self.region.as_str()).filter(|(i3, _, _)| !i3.is_empty()).map(|(i3, _, _)| i3.to_string())
    }
}

fn display_variant(v: &str) -> String {
    names().variants.get(v).map(|n| n.to_string()).unwrap_or_else(|| v.to_string())
}

/// English names of the Unicode keyword types Elasticsearch's JDK shows in display names.
fn keyword_display(key: &str, ty: &str) -> Option<&'static str> {
    Some(match (key, ty) {
        ("nu", "latn") => "Western Digits",
        ("nu", "thai") => "Thai Digits",
        ("nu", "arab") => "Arabic-Indic Digits",
        ("ca", "japanese") => "Japanese Calendar",
        ("ca", "buddhist") => "Buddhist Calendar",
        ("ca", "gregory") => "Gregorian Calendar",
        _ => return None,
    })
}

fn parse_extensions(id: &str) -> BTreeMap<char, String> {
    let mut out = BTreeMap::new();
    let subs: Vec<&str> = id.split('-').filter(|s| !s.is_empty()).collect();
    let mut i = 0;
    while i < subs.len() {
        let key = subs[i];
        if key.len() != 1 {
            i += 1;
            continue;
        }
        let k = key.chars().next().unwrap().to_ascii_lowercase();
        let start = i + 1;
        let mut j = start;
        // private use runs to the end; other values stop at the next singleton
        while j < subs.len() && (k == 'x' || subs[j].len() > 1) {
            j += 1;
        }
        out.insert(k, subs[start..j].join("-"));
        i = j;
    }
    out
}

/// RFC 5646 grandfathered tags and the tags the JDK maps them to.
const GRANDFATHERED: &[(&str, &str)] = &[
    ("art-lojban", "jbo"),
    ("cel-gaulish", "xtg-x-cel-gaulish"),
    ("en-gb-oed", "en-GB-x-oed"),
    ("i-ami", "ami"),
    ("i-bnn", "bnn"),
    ("i-default", "en-x-i-default"),
    ("i-enochian", "und-x-i-enochian"),
    ("i-hak", "hak"),
    ("i-klingon", "tlh"),
    ("i-lux", "lb"),
    ("i-mingo", "see-x-i-mingo"),
    ("i-navajo", "nv"),
    ("i-pwn", "pwn"),
    ("i-tao", "tao"),
    ("i-tay", "tay"),
    ("i-tsu", "tsu"),
    ("no-bok", "nb"),
    ("no-nyn", "nn"),
    ("sgn-be-fr", "sfb"),
    ("sgn-be-nl", "vgt"),
    ("sgn-ch-de", "sgg"),
    ("zh-guoyu", "cmn"),
    ("zh-hakka", "hak"),
    ("zh-min", "nan-x-zh-min"),
    ("zh-min-nan", "nan"),
    ("zh-xiang", "hsn"),
];

struct Names {
    /// code → (ISO 639-2 code, English name, in `getISOLanguages()`)
    langs: HashMap<&'static str, (&'static str, &'static str, bool)>,
    countries: HashMap<&'static str, (&'static str, &'static str, bool)>,
    scripts: HashMap<&'static str, &'static str>,
    variants: HashMap<&'static str, &'static str>,
    iso_langs: Vec<&'static str>,
    iso_countries: Vec<&'static str>,
}

fn names() -> &'static Names {
    static N: OnceLock<Names> = OnceLock::new();
    N.get_or_init(|| {
        let mut n = Names { langs: HashMap::new(), countries: HashMap::new(), scripts: HashMap::new(), variants: HashMap::new(), iso_langs: Vec::new(), iso_countries: Vec::new() };
        for line in include_str!("locale_names.tsv").lines() {
            let f: Vec<&'static str> = line.split('\t').collect();
            match f[0] {
                "L" => {
                    let iso = f.get(4) == Some(&"I");
                    if iso {
                        n.iso_langs.push(f[1]);
                    }
                    n.langs.insert(f[1], (f[2], f[3], iso));
                }
                "C" => {
                    let iso = f.get(4) == Some(&"I");
                    if iso {
                        n.iso_countries.push(f[1]);
                    }
                    n.countries.insert(f[1], (f[2], f[3], iso));
                }
                "S" => {
                    n.scripts.insert(f[1], f[2]);
                }
                "V" => {
                    n.variants.insert(f[1], f[2]);
                }
                _ => {}
            }
        }
        n
    })
}

/// `Locale.getISOLanguages()`.
pub fn iso_languages() -> &'static [&'static str] {
    &names().iso_langs
}

/// `Locale.getISOCountries()`.
pub fn iso_countries() -> &'static [&'static str] {
    &names().iso_countries
}

/// `Locale.Builder`'s validation failures (`IllformedLocaleException`): message and index.
#[derive(Debug, PartialEq, Eq)]
pub struct Illformed {
    pub message: String,
    pub index: i32,
}

impl Illformed {
    fn new(what: &str, value: &str, index: usize) -> Illformed {
        Illformed { message: format!("Ill-formed {what}: {value}"), index: index as i32 }
    }
}

/// The state of a `Locale.Builder`.
#[derive(Clone, Debug, Default)]
pub struct Builder {
    pub loc: Loc,
}

impl Builder {
    pub fn set_language(&mut self, s: &str) -> Result<(), Illformed> {
        if !s.is_empty() && !is_language(s) {
            return Err(Illformed::new("language", s, 0));
        }
        self.loc.lang = s.to_ascii_lowercase();
        Ok(())
    }

    pub fn set_script(&mut self, s: &str) -> Result<(), Illformed> {
        if !s.is_empty() && !is_script(s) {
            return Err(Illformed::new("script", s, 0));
        }
        self.loc.script = if s.is_empty() { String::new() } else { title(s) };
        Ok(())
    }

    pub fn set_region(&mut self, s: &str) -> Result<(), Illformed> {
        if !s.is_empty() && !is_region(s) {
            return Err(Illformed::new("region", s, 0));
        }
        self.loc.region = s.to_ascii_uppercase();
        Ok(())
    }

    pub fn set_variant(&mut self, s: &str) -> Result<(), Illformed> {
        let mut index = 0;
        for sub in s.split(['_', '-']) {
            if !s.is_empty() && !is_variant(sub) {
                return Err(Illformed::new("variant", s, index));
            }
            index += sub.len() + 1;
        }
        self.loc.variant = s.replace('-', "_");
        Ok(())
    }

    pub fn set_extension(&mut self, key: char, value: &str) -> Result<(), Illformed> {
        let k = key.to_ascii_lowercase();
        if !(k.is_ascii_alphanumeric()) {
            return Err(Illformed { message: format!("Ill-formed extension key: {key}"), index: -1 });
        }
        if value.is_empty() {
            self.loc.ext.remove(&k);
            return Ok(());
        }
        let subs: Vec<&str> = value.split(['_', '-']).collect();
        let mut index = 0;
        for sub in &subs {
            let ok = if k == 'x' { is_privateuse_subtag(sub) } else { is_extension_subtag(sub) };
            if !ok {
                return Err(Illformed::new("extension value", value, index));
            }
            index += sub.len() + 1;
        }
        let v = subs.join("-").to_ascii_lowercase();
        self.loc.ext.insert(k, if k == 'u' { canonical_unicode(&v) } else { v });
        Ok(())
    }

    pub fn set_unicode_keyword(&mut self, key: &str, ty: Option<&str>) -> Result<(), Illformed> {
        if key.len() != 2 || !is_alnum(key) {
            return Err(Illformed { message: format!("Ill-formed Unicode locale keyword key: {key}"), index: -1 });
        }
        let key = key.to_ascii_lowercase();
        let mut attrs = self.loc.unicode_attributes();
        let mut kws: Vec<(String, String)> = self.loc.unicode_keywords().into_iter().filter(|(k, _)| *k != key).collect();
        if let Some(t) = ty {
            let mut index = 0;
            for sub in t.split(['_', '-']) {
                if !t.is_empty() && !((3..=8).contains(&sub.len()) && is_alnum(sub)) {
                    return Err(Illformed::new("Unicode locale keyword type", t, index));
                }
                index += sub.len() + 1;
            }
            kws.push((key, t.replace('_', "-").to_ascii_lowercase()));
        }
        attrs.sort();
        self.set_unicode(attrs, kws);
        Ok(())
    }

    pub fn add_unicode_attribute(&mut self, attr: &str) -> Result<(), Illformed> {
        if !((3..=8).contains(&attr.len()) && is_alnum(attr)) {
            return Err(Illformed::new("Unicode locale attribute", attr, 0));
        }
        let mut attrs = self.loc.unicode_attributes();
        let a = attr.to_ascii_lowercase();
        if !attrs.contains(&a) {
            attrs.push(a);
        }
        let kws = self.loc.unicode_keywords();
        self.set_unicode(attrs, kws);
        Ok(())
    }

    pub fn remove_unicode_attribute(&mut self, attr: &str) -> Result<(), Illformed> {
        if !((3..=8).contains(&attr.len()) && is_alnum(attr)) {
            return Err(Illformed::new("Unicode locale attribute", attr, 0));
        }
        let a = attr.to_ascii_lowercase();
        let attrs: Vec<String> = self.loc.unicode_attributes().into_iter().filter(|x| *x != a).collect();
        let kws = self.loc.unicode_keywords();
        self.set_unicode(attrs, kws);
        Ok(())
    }

    fn set_unicode(&mut self, mut attrs: Vec<String>, mut kws: Vec<(String, String)>) {
        attrs.sort();
        kws.sort();
        let mut parts = attrs;
        for (k, t) in kws {
            parts.push(k);
            if !t.is_empty() {
                parts.push(t);
            }
        }
        if parts.is_empty() {
            self.loc.ext.remove(&'u');
        } else {
            self.loc.ext.insert('u', parts.join("-"));
        }
    }

    /// `build()`.
    pub fn build(&self) -> Loc {
        let mut l = self.loc.clone();
        l.lang = convert_old(&l.lang);
        l.compat();
        l
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(t: &str) -> String {
        Loc::from_tag(t).to_java_string()
    }

    #[test]
    fn language_tags() {
        // captured from Elasticsearch 9.5.5's JDK
        assert_eq!(tag("en_US"), "");
        assert_eq!(tag("EN-us"), "en_US");
        assert_eq!(tag("zh-hant-tw"), "zh_TW_#Hant");
        assert_eq!(tag("de-1996"), "de__1996");
        assert_eq!(tag("de-DE-1996-x-foo"), "de_DE_1996_#x-foo");
        assert_eq!(tag("en-US-u-ca-buddhist-nu-thai"), "en_US_#u-ca-buddhist-nu-thai");
        assert_eq!(tag("i-klingon"), "tlh");
        assert_eq!(tag("zh-yue-HK"), "yue_HK");
        assert_eq!(tag("ja-JP-x-lvariant-JP"), "ja_JP_JP_#u-ca-japanese");
        assert_eq!(tag("und-US"), "_US");
        assert_eq!(tag(""), "");
        assert_eq!(tag("sr-Latn-RS-u-nu-latn-x-priv"), "sr_RS_#Latn_u-nu-latn-x-priv");
        assert_eq!(Loc::from_tag("en-US-u-ca-buddhist-nu-thai").to_tag(), "en-US-u-ca-buddhist-nu-thai");
        assert_eq!(Loc::from_tag("de-1996").to_tag(), "de-1996");
        assert_eq!(Loc::new("iw", "", "").to_tag(), "he");
        assert_eq!(Loc::new("IW", "il", "").to_java_string(), "he_IL");
        assert_eq!(Loc::new("no", "NO", "NY").to_tag(), "nn-NO");
        assert_eq!(Loc::new("en", "US", "WIN_x").to_java_string(), "en_US_WIN_x");
        assert_eq!(Loc::new("en", "US", "WIN_x").to_tag(), "en-US-x-lvariant-WIN-x");
        assert_eq!(Loc::new("", "US", "").to_tag(), "und-US");
        assert_eq!(Loc::new("", "", "X").to_tag(), "x-lvariant-X");
        assert_eq!(Loc::new("ja", "JP", "JP").to_java_string(), "ja_JP_JP_#u-ca-japanese");
        assert_eq!(Loc::new("ja", "JP", "JP").to_tag(), "ja-JP-u-ca-japanese-x-lvariant-JP");
        assert_eq!(Loc::new("th", "TH", "TH").to_tag(), "th-TH-u-nu-thai-x-lvariant-TH");
        assert_eq!(Loc::new("", "", "").to_tag(), "und");
    }

    #[test]
    fn round_trip_and_display() {
        for s in ["sr_RS_#Latn_u-nu-latn-x-priv", "ja_JP_JP_#u-ca-japanese", "hi__#Latn", "de_DE_1996_#x-foo", "en_US_POSIX", "_US"] {
            assert_eq!(Loc::parse(s).to_java_string(), s);
            assert_eq!(Loc::parse(s).key(), s);
        }
        // parts toString() drops survive in the stored key
        let x = Loc::from_tag("x-whatever");
        assert_eq!(x.to_java_string(), "");
        assert_eq!(Loc::parse(&x.key()).to_tag(), "x-whatever");
        assert_eq!(Loc::display_key(&x.key()), "");
        assert_eq!(Loc::parse(&Loc::new("", "", "X").key()).to_tag(), "x-lvariant-X");
        let l = Loc::from_tag("sr-Latn-RS-u-nu-latn-x-priv");
        assert_eq!(l.display_name(), "Serbian (Latin, Serbia, Western Digits)");
        assert_eq!(Loc::new("xx", "YY", "").display_name(), "xx (YY)");
        assert_eq!(Loc::parse("th_TH_TH_#u-nu-thai").display_name(), "Thai (Thailand, TH, Thai Digits)");
        assert_eq!(Loc::parse("en_US_POSIX").display_name(), "English (United States, Computer)");
        assert_eq!(Loc::new("en", "", "").iso3_language().as_deref(), Some("eng"));
        assert_eq!(Loc::new("xx", "", "").iso3_language(), None);
        assert_eq!(iso_languages().len(), 188);
        assert_eq!(iso_countries().len(), 249);
    }

    #[test]
    fn builder() {
        let mut b = Builder::default();
        b.set_language("sr").unwrap();
        b.set_script("Latn").unwrap();
        b.set_region("RS").unwrap();
        b.set_unicode_keyword("nu", Some("latn")).unwrap();
        b.add_unicode_attribute("foo").unwrap();
        assert_eq!(b.build().to_tag(), "sr-Latn-RS-u-foo-nu-latn");
        assert_eq!(Builder::default().set_language("1"), Err(Illformed { message: "Ill-formed language: 1".into(), index: 0 }));
    }
}
