//! ICU4C 78.1, linked statically (see build.rs), with the parts of its C API Postgres's ICU
//! provider calls (pg_locale_icu.c): collators from locale strings and from rules, UTF-8
//! comparison, sort keys, versions, language tags, available locales, case mapping and
//! the character properties of the regex engine.
//!
//! When the ICU build was unavailable, `available()` is false and every entry point fails
//! with `U_UNSUPPORTED_ERROR`.

use std::cmp::Ordering;

/// An ICU error code (`UErrorCode`): positive values are failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcuError(pub i32);

pub const U_ZERO_ERROR: i32 = 0;
pub const U_ILLEGAL_ARGUMENT_ERROR: i32 = 1;
pub const U_BUFFER_OVERFLOW_ERROR: i32 = 15;
pub const U_UNSUPPORTED_ERROR: i32 = 16;
pub const U_STRING_NOT_TERMINATED_WARNING: i32 = -124;

/// `u_strFoldCase` options.
pub const U_FOLD_CASE_DEFAULT: u32 = 0;
pub const U_FOLD_CASE_EXCLUDE_SPECIAL_I: u32 = 1;

impl IcuError {
    /// `u_errorName`.
    pub fn name(self) -> String {
        imp::error_name(self.0)
    }
}

impl std::fmt::Display for IcuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name())
    }
}

/// Whether ICU4C was built in.
pub fn available() -> bool {
    cfg!(pg_icu)
}

/// The ICU version built in ("78.1"), if any.
pub fn version() -> Option<&'static str> {
    available().then_some("78.1")
}

/// An opened collator (`UCollator`). Its const operations (comparison, sort keys) are
/// thread-safe.
pub struct Collator(imp::Handle);

// SAFETY: ICU documents a collator's const API as safe to call from several threads.
unsafe impl Send for Collator {}
unsafe impl Sync for Collator {}

impl std::fmt::Debug for Collator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Collator")
    }
}

impl Collator {
    /// `ucol_open`.
    pub fn open(locale: &str) -> Result<Collator, IcuError> {
        imp::open(locale).map(Collator)
    }

    /// `ucol_openRules(rules, UCOL_DEFAULT, UCOL_DEFAULT_STRENGTH)`.
    pub fn open_rules(rules: &[u16]) -> Result<Collator, IcuError> {
        imp::open_rules(rules).map(Collator)
    }

    /// `ucol_getRules`: the collator's tailoring rules.
    pub fn rules(&self) -> Vec<u16> {
        imp::rules(&self.0)
    }

    /// `ucol_strcollUTF8`.
    pub fn strcoll(&self, a: &str, b: &str) -> Result<Ordering, IcuError> {
        imp::strcoll(&self.0, a, b)
    }

    /// `ucol_getSortKey` of a UTF-16 string: the key with its terminating NUL.
    pub fn sort_key(&self, s: &[u16]) -> Vec<u8> {
        imp::sort_key(&self.0, s)
    }

    /// `ucol_getVersion` as `u_versionToString` writes it.
    pub fn version(&self) -> String {
        imp::coll_version(&self.0)
    }
}

impl Drop for Collator {
    fn drop(&mut self) {
        imp::close(&self.0);
    }
}

/// `uloc_toLanguageTag`.
pub fn to_language_tag(locale: &str, strict: bool) -> Result<String, IcuError> {
    imp::to_language_tag(locale, strict)
}

/// `uloc_getLanguage` into a buffer of `capacity` bytes: the language and the status (which
/// may be a warning, such as U_STRING_NOT_TERMINATED_WARNING).
pub fn get_language(locale: &str, capacity: usize) -> (String, i32) {
    imp::get_language(locale, capacity)
}

/// `uloc_getAvailable` for all of `uloc_countAvailable`.
pub fn available_locales() -> Vec<String> {
    imp::available_locales()
}

/// The case mappings (`u_strToUpper`, `u_strToLower`, `u_strToTitle` with the default word
/// break iterator) under a locale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    Upper,
    Lower,
    Title,
}

/// A case mapping of UTF-16 text.
pub fn to_case(kind: Case, s: &[u16], locale: &str) -> Result<Vec<u16>, IcuError> {
    imp::to_case(kind, s, locale)
}

/// `u_strFoldCase`.
pub fn fold_case(s: &[u16], options: u32) -> Result<Vec<u16>, IcuError> {
    imp::fold_case(s, options)
}

/// The character properties (`u_isalpha` & co.) and simple case mappings (`u_toupper`,
/// `u_tolower`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prop {
    Alpha,
    Digit,
    Alnum,
    Upper,
    Lower,
    Graph,
    Print,
    Punct,
    Space,
}

pub fn has_prop(p: Prop, c: u32) -> bool {
    imp::has_prop(p, c)
}
pub fn toupper(c: u32) -> u32 {
    imp::toupper(c)
}
pub fn tolower(c: u32) -> u32 {
    imp::tolower(c)
}

#[cfg(pg_icu)]
mod imp {
    use super::{Case, IcuError, Prop, U_BUFFER_OVERFLOW_ERROR, U_ZERO_ERROR};
    use std::cmp::Ordering;
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::sync::Once;

    #[repr(C)]
    pub struct UCollator {
        _p: [u8; 0],
    }

    pub struct Handle(*mut UCollator);

    /// The data package, aligned as ICU requires.
    #[repr(C, align(16))]
    struct Aligned<T: ?Sized>(T);
    static DATA: &Aligned<[u8]> = &Aligned(*include_bytes!(env!("PG_ICU_DATA")));

    // ICU's symbols carry its major version (U_DISABLE_RENAMING is off).
    unsafe extern "C" {
        #[link_name = "udata_setCommonData_78"]
        fn udata_setCommonData(data: *const c_void, err: *mut c_int);
        #[link_name = "u_errorName_78"]
        fn u_errorName(code: c_int) -> *const c_char;
        #[link_name = "ucol_open_78"]
        fn ucol_open(loc: *const c_char, err: *mut c_int) -> *mut UCollator;
        #[link_name = "ucol_openRules_78"]
        fn ucol_openRules(
            rules: *const u16,
            len: i32,
            norm: c_int,
            strength: c_int,
            parse_error: *mut c_void,
            err: *mut c_int,
        ) -> *mut UCollator;
        #[link_name = "ucol_getRules_78"]
        fn ucol_getRules(coll: *const UCollator, len: *mut i32) -> *const u16;
        #[link_name = "ucol_close_78"]
        fn ucol_close(coll: *mut UCollator);
        #[link_name = "ucol_strcollUTF8_78"]
        fn ucol_strcollUTF8(
            coll: *const UCollator,
            a: *const c_char,
            alen: i32,
            b: *const c_char,
            blen: i32,
            err: *mut c_int,
        ) -> c_int;
        #[link_name = "ucol_getSortKey_78"]
        fn ucol_getSortKey(
            coll: *const UCollator,
            src: *const u16,
            len: i32,
            out: *mut u8,
            cap: i32,
        ) -> i32;
        #[link_name = "ucol_getVersion_78"]
        fn ucol_getVersion(coll: *const UCollator, info: *mut u8);
        #[link_name = "u_versionToString_78"]
        fn u_versionToString(info: *const u8, out: *mut c_char);
        #[link_name = "uloc_toLanguageTag_78"]
        fn uloc_toLanguageTag(
            loc: *const c_char,
            out: *mut c_char,
            cap: i32,
            strict: i8,
            err: *mut c_int,
        ) -> i32;
        #[link_name = "uloc_getLanguage_78"]
        fn uloc_getLanguage(loc: *const c_char, out: *mut c_char, cap: i32, err: *mut c_int)
        -> i32;
        #[link_name = "uloc_countAvailable_78"]
        fn uloc_countAvailable() -> i32;
        #[link_name = "uloc_getAvailable_78"]
        fn uloc_getAvailable(i: i32) -> *const c_char;
        #[link_name = "u_strToUpper_78"]
        fn u_strToUpper(
            out: *mut u16,
            cap: i32,
            src: *const u16,
            len: i32,
            loc: *const c_char,
            err: *mut c_int,
        ) -> i32;
        #[link_name = "u_strToLower_78"]
        fn u_strToLower(
            out: *mut u16,
            cap: i32,
            src: *const u16,
            len: i32,
            loc: *const c_char,
            err: *mut c_int,
        ) -> i32;
        #[link_name = "u_strToTitle_78"]
        fn u_strToTitle(
            out: *mut u16,
            cap: i32,
            src: *const u16,
            len: i32,
            iter: *mut c_void,
            loc: *const c_char,
            err: *mut c_int,
        ) -> i32;
        #[link_name = "u_strFoldCase_78"]
        fn u_strFoldCase(
            out: *mut u16,
            cap: i32,
            src: *const u16,
            len: i32,
            options: u32,
            err: *mut c_int,
        ) -> i32;
        #[link_name = "u_isalpha_78"]
        fn u_isalpha(c: i32) -> i8;
        #[link_name = "u_isdigit_78"]
        fn u_isdigit(c: i32) -> i8;
        #[link_name = "u_isalnum_78"]
        fn u_isalnum(c: i32) -> i8;
        #[link_name = "u_isupper_78"]
        fn u_isupper(c: i32) -> i8;
        #[link_name = "u_islower_78"]
        fn u_islower(c: i32) -> i8;
        #[link_name = "u_isgraph_78"]
        fn u_isgraph(c: i32) -> i8;
        #[link_name = "u_isprint_78"]
        fn u_isprint(c: i32) -> i8;
        #[link_name = "u_ispunct_78"]
        fn u_ispunct(c: i32) -> i8;
        #[link_name = "u_isspace_78"]
        fn u_isspace(c: i32) -> i8;
        #[link_name = "u_toupper_78"]
        fn u_toupper(c: i32) -> i32;
        #[link_name = "u_tolower_78"]
        fn u_tolower(c: i32) -> i32;
    }

    /// Hand ICU its data before anything loads it.
    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let mut err = U_ZERO_ERROR;
            // SAFETY: DATA is a valid, 16-byte aligned common data package that lives
            // forever.
            unsafe { udata_setCommonData(DATA.0.as_ptr().cast(), &mut err) };
            assert!(err <= 0, "udata_setCommonData: {}", error_name(err));
        });
    }

    fn check(err: c_int) -> Result<(), IcuError> {
        if err > 0 { Err(IcuError(err)) } else { Ok(()) }
    }

    /// A C string of `s` (which can't hold NUL: SQL text never does; cut at one).
    fn cstr(s: &str) -> CString {
        let s = s.split('\0').next().unwrap_or("");
        CString::new(s).unwrap_or_default()
    }

    fn len32(n: usize) -> Result<i32, IcuError> {
        i32::try_from(n).map_err(|_| IcuError(super::U_ILLEGAL_ARGUMENT_ERROR))
    }

    pub fn error_name(code: i32) -> String {
        // SAFETY: u_errorName returns a static string for any code.
        unsafe { CStr::from_ptr(u_errorName(code)) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn open(locale: &str) -> Result<Handle, IcuError> {
        init();
        let loc = cstr(locale);
        let mut err = U_ZERO_ERROR;
        // SAFETY: loc is NUL-terminated; err is a valid out-pointer.
        let c = unsafe { ucol_open(loc.as_ptr(), &mut err) };
        if err > 0 {
            if !c.is_null() {
                // SAFETY: c came from ucol_open.
                unsafe { ucol_close(c) };
            }
            return Err(IcuError(err));
        }
        Ok(Handle(c))
    }

    pub fn open_rules(rules: &[u16]) -> Result<Handle, IcuError> {
        init();
        let mut err = U_ZERO_ERROR;
        // UCOL_DEFAULT normalization, UCOL_DEFAULT_STRENGTH, no parse error details.
        // SAFETY: rules is valid for its length.
        let c = unsafe {
            ucol_openRules(
                rules.as_ptr(),
                len32(rules.len())?,
                -1,
                2,
                std::ptr::null_mut(),
                &mut err,
            )
        };
        if err > 0 {
            if !c.is_null() {
                // SAFETY: c came from ucol_openRules.
                unsafe { ucol_close(c) };
            }
            return Err(IcuError(err));
        }
        Ok(Handle(c))
    }

    pub fn rules(h: &Handle) -> Vec<u16> {
        let mut len = 0;
        // SAFETY: the returned pointer stays valid while the collator does.
        let p = unsafe { ucol_getRules(h.0, &mut len) };
        if p.is_null() || len <= 0 {
            return Vec::new();
        }
        // SAFETY: ICU returned `len` code units at p.
        unsafe { std::slice::from_raw_parts(p, len as usize) }.to_vec()
    }

    pub fn close(h: &Handle) {
        // SAFETY: h.0 came from ucol_open*, and is closed once (Drop).
        unsafe { ucol_close(h.0) };
    }

    pub fn strcoll(h: &Handle, a: &str, b: &str) -> Result<Ordering, IcuError> {
        let mut err = U_ZERO_ERROR;
        // SAFETY: a and b are valid for their lengths.
        let r = unsafe {
            ucol_strcollUTF8(
                h.0,
                a.as_ptr().cast(),
                len32(a.len())?,
                b.as_ptr().cast(),
                len32(b.len())?,
                &mut err,
            )
        };
        check(err)?;
        Ok(r.cmp(&0))
    }

    pub fn sort_key(h: &Handle, s: &[u16]) -> Vec<u8> {
        let Ok(n) = len32(s.len()) else {
            return Vec::new();
        };
        let mut out = vec![0u8; s.len() * 4 + 32];
        loop {
            // SAFETY: s and out are valid for their lengths.
            let need = unsafe {
                ucol_getSortKey(h.0, s.as_ptr(), n, out.as_mut_ptr(), out.len() as i32)
            };
            let need = need.max(0) as usize;
            if need <= out.len() {
                out.truncate(need);
                return out;
            }
            out.resize(need, 0);
        }
    }

    pub fn coll_version(h: &Handle) -> String {
        let mut info = [0u8; 4];
        let mut buf = [0 as c_char; 24];
        // SAFETY: info holds a UVersionInfo; buf holds U_MAX_VERSION_STRING_LENGTH.
        unsafe {
            ucol_getVersion(h.0, info.as_mut_ptr());
            u_versionToString(info.as_ptr(), buf.as_mut_ptr());
            CStr::from_ptr(buf.as_ptr())
        }
        .to_string_lossy()
        .into_owned()
    }

    pub fn to_language_tag(locale: &str, strict: bool) -> Result<String, IcuError> {
        init();
        let loc = cstr(locale);
        let mut cap = 32usize;
        loop {
            let mut buf = vec![0u8; cap];
            let mut err = U_ZERO_ERROR;
            // SAFETY: buf holds cap bytes.
            let n = unsafe {
                uloc_toLanguageTag(
                    loc.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    cap as i32,
                    strict as i8,
                    &mut err,
                )
            };
            if (err == U_BUFFER_OVERFLOW_ERROR || err == super::U_STRING_NOT_TERMINATED_WARNING)
                && cap < 1 << 30
            {
                cap = (cap * 2).max(n.max(0) as usize + 1);
                continue;
            }
            check(err)?;
            buf.truncate(n.max(0) as usize);
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
    }

    pub fn get_language(locale: &str, capacity: usize) -> (String, i32) {
        init();
        let loc = cstr(locale);
        let mut buf = vec![0u8; capacity.max(1)];
        let mut err = U_ZERO_ERROR;
        // SAFETY: buf holds `capacity` bytes.
        let n = unsafe {
            uloc_getLanguage(loc.as_ptr(), buf.as_mut_ptr().cast(), capacity as i32, &mut err)
        };
        let n = (n.max(0) as usize).min(capacity);
        (String::from_utf8_lossy(&buf[..n]).into_owned(), err)
    }

    pub fn available_locales() -> Vec<String> {
        init();
        // SAFETY: uloc_getAvailable returns static strings for indexes below the count.
        let n = unsafe { uloc_countAvailable() };
        (0..n)
            .filter_map(|i| {
                let p = unsafe { uloc_getAvailable(i) };
                (!p.is_null()).then(|| {
                    // SAFETY: p is a static NUL-terminated string.
                    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
                })
            })
            .collect()
    }

    /// Run a preflighting UTF-16 transform: first with the source's length, then with the
    /// length it asked for (icu_convert_case).
    fn preflight(
        s: &[u16],
        f: impl Fn(*mut u16, i32, *mut c_int) -> i32,
    ) -> Result<Vec<u16>, IcuError> {
        let mut cap = s.len().max(1);
        loop {
            let mut out = vec![0u16; cap];
            let mut err = U_ZERO_ERROR;
            let n = f(out.as_mut_ptr(), len32(cap)?, &mut err);
            if err == U_BUFFER_OVERFLOW_ERROR && n as usize > cap {
                cap = n as usize;
                continue;
            }
            check(err)?;
            out.truncate(n.max(0) as usize);
            return Ok(out);
        }
    }

    pub fn to_case(kind: Case, s: &[u16], locale: &str) -> Result<Vec<u16>, IcuError> {
        init();
        let loc = cstr(locale);
        let n = len32(s.len())?;
        preflight(s, |out, cap, err| {
            // SAFETY: out holds cap units; s holds n; loc is NUL-terminated.
            unsafe {
                match kind {
                    Case::Upper => u_strToUpper(out, cap, s.as_ptr(), n, loc.as_ptr(), err),
                    Case::Lower => u_strToLower(out, cap, s.as_ptr(), n, loc.as_ptr(), err),
                    Case::Title => u_strToTitle(
                        out,
                        cap,
                        s.as_ptr(),
                        n,
                        std::ptr::null_mut(),
                        loc.as_ptr(),
                        err,
                    ),
                }
            }
        })
    }

    pub fn fold_case(s: &[u16], options: u32) -> Result<Vec<u16>, IcuError> {
        init();
        let n = len32(s.len())?;
        preflight(s, |out, cap, err| {
            // SAFETY: out holds cap units; s holds n.
            unsafe { u_strFoldCase(out, cap, s.as_ptr(), n, options, err) }
        })
    }

    pub fn has_prop(p: Prop, c: u32) -> bool {
        let c = c as i32;
        // SAFETY: the property functions take any code point value.
        (unsafe {
            match p {
                Prop::Alpha => u_isalpha(c),
                Prop::Digit => u_isdigit(c),
                Prop::Alnum => u_isalnum(c),
                Prop::Upper => u_isupper(c),
                Prop::Lower => u_islower(c),
                Prop::Graph => u_isgraph(c),
                Prop::Print => u_isprint(c),
                Prop::Punct => u_ispunct(c),
                Prop::Space => u_isspace(c),
            }
        }) != 0
    }

    pub fn toupper(c: u32) -> u32 {
        // SAFETY: u_toupper takes any code point value.
        unsafe { u_toupper(c as i32) as u32 }
    }

    pub fn tolower(c: u32) -> u32 {
        // SAFETY: u_tolower takes any code point value.
        unsafe { u_tolower(c as i32) as u32 }
    }
}

#[cfg(not(pg_icu))]
mod imp {
    use super::{Case, IcuError, Prop, U_UNSUPPORTED_ERROR};
    use std::cmp::Ordering;

    pub enum Handle {}

    const NO: IcuError = IcuError(U_UNSUPPORTED_ERROR);

    pub fn error_name(code: i32) -> String {
        match code {
            0 => "U_ZERO_ERROR".into(),
            1 => "U_ILLEGAL_ARGUMENT_ERROR".into(),
            15 => "U_BUFFER_OVERFLOW_ERROR".into(),
            16 => "U_UNSUPPORTED_ERROR".into(),
            c => format!("ICU error {c}"),
        }
    }
    pub fn open(_: &str) -> Result<Handle, IcuError> {
        Err(NO)
    }
    pub fn open_rules(_: &[u16]) -> Result<Handle, IcuError> {
        Err(NO)
    }
    pub fn rules(h: &Handle) -> Vec<u16> {
        match *h {}
    }
    pub fn close(h: &Handle) {
        match *h {}
    }
    pub fn strcoll(h: &Handle, _: &str, _: &str) -> Result<Ordering, IcuError> {
        match *h {}
    }
    pub fn sort_key(h: &Handle, _: &[u16]) -> Vec<u8> {
        match *h {}
    }
    pub fn coll_version(h: &Handle) -> String {
        match *h {}
    }
    pub fn to_language_tag(_: &str, _: bool) -> Result<String, IcuError> {
        Err(NO)
    }
    pub fn get_language(_: &str, _: usize) -> (String, i32) {
        (String::new(), U_UNSUPPORTED_ERROR)
    }
    pub fn available_locales() -> Vec<String> {
        Vec::new()
    }
    pub fn to_case(_: Case, _: &[u16], _: &str) -> Result<Vec<u16>, IcuError> {
        Err(NO)
    }
    pub fn fold_case(_: &[u16], _: u32) -> Result<Vec<u16>, IcuError> {
        Err(NO)
    }
    pub fn has_prop(_: Prop, _: u32) -> bool {
        false
    }
    pub fn toupper(c: u32) -> u32 {
        c
    }
    pub fn tolower(c: u32) -> u32 {
        c
    }
}

#[cfg(all(test, pg_icu))]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn basics() {
        let de = Collator::open("de-u-ks-level2").unwrap();
        assert_eq!(de.strcoll("a", "A").unwrap(), Ordering::Equal);
        assert_eq!(de.strcoll("a", "b").unwrap(), Ordering::Less);
        assert_eq!(Collator::open("und").unwrap().version(), "153.136");
        assert_eq!(Collator::open("sv").unwrap().version(), "153.136.48");
        assert!(Collator::open("und-u-ks-foo").is_err());
        assert_eq!(to_language_tag("de_DE@collation=phonebook", true).unwrap(), "de-DE-u-co-phonebk");
        assert_eq!(
            to_case(Case::Upper, &u("ıi"), "tr").unwrap(),
            u("Iİ")
        );
        assert_eq!(to_case(Case::Title, &u("hello wORLD")
            , "und").unwrap(), u("Hello World"));
        assert!(available_locales().iter().any(|l| l == "de_DE"));
        let k = Collator::open("und").unwrap().sort_key(&u("a"));
        assert_eq!(k.last(), Some(&0));
        let r = Collator::open("und").unwrap().rules();
        assert!(r.is_empty());
        let x = Collator::open_rules(&u("&a < g")).unwrap();
        assert_eq!(x.strcoll("g", "b").unwrap(), Ordering::Less);
        assert!(has_prop(Prop::Alpha, 'é' as u32));
    }
}
