//! Postgres's regular expression engine: a port of Henry Spencer's ARE package as Postgres 18
//! carries it (src/backend/regex). Patterns compile to an NFA over character "colors", which
//! runs as lazy DFAs; the subexpression tree is dissected by backtracking for captures and
//! back-references. Matching, the choice of submatches, limits and error texts are Postgres's.

mod comp;
mod exec;
pub mod locale;
mod nfa;
mod prefix;
pub(crate) mod unicode_tables;

pub use prefix::Prefix;

pub use locale::{Strategy, default_strategy};

use crate::error::{Error, Result, code};
use std::cell::RefCell;
use std::sync::Arc;

/// A character (`chr`, a pg_wchar: the Unicode code point in UTF-8 databases).
pub type Chr = u32;
/// A character equivalence class.
pub type Color = i16;

/// The cutoff between the simple color map array and the "high" color map.
pub const MAX_SIMPLE_CHR: Chr = 0x7FF;
const CHR_MAX: Chr = 0x7fff_fffe;
const DUPMAX: i32 = 255;
const DUPINF: i32 = DUPMAX + 1;

type R<T> = std::result::Result<T, i32>;

// compile flags
pub const REG_BASIC: i32 = 0o0;
pub const REG_EXTENDED: i32 = 0o1;
pub const REG_ADVF: i32 = 0o2;
pub const REG_ADVANCED: i32 = 0o3;
pub const REG_QUOTE: i32 = 0o4;
pub const REG_ICASE: i32 = 0o10;
pub const REG_NOSUB: i32 = 0o20;
pub const REG_EXPANDED: i32 = 0o40;
pub const REG_NLSTOP: i32 = 0o100;
pub const REG_NLANCH: i32 = 0o200;
pub const REG_NEWLINE: i32 = 0o300;
const REG_BOSONLY: i32 = 0o2000;

// execution flags
const REG_NOTBOL: i32 = 0o1;
const REG_NOTEOL: i32 = 0o2;

// re_info bits
const REG_UBACKREF: i64 = 0o1;
const REG_ULOOKAROUND: i64 = 0o2;
const REG_UBOUNDS: i64 = 0o4;
const REG_UBRACES: i64 = 0o10;
const REG_UBSALNUM: i64 = 0o20;
const REG_UPBOTCH: i64 = 0o40;
const REG_UBBS: i64 = 0o100;
const REG_UNONPOSIX: i64 = 0o200;
const REG_UUNSPEC: i64 = 0o400;
const REG_UUNPORT: i64 = 0o1000;
const REG_ULOCALE: i64 = 0o2000;
const REG_UEMPTYMATCH: i64 = 0o4000;
const REG_UIMPOSSIBLE: i64 = 0o10000;
const REG_USHORTEST: i64 = 0o20000;

// error codes
const REG_OKAY: i32 = 0;
const REG_NOMATCH: i32 = 1;
const REG_BADPAT: i32 = 2;
const REG_ECOLLATE: i32 = 3;
const REG_ECTYPE: i32 = 4;
const REG_EESCAPE: i32 = 5;
const REG_ESUBREG: i32 = 6;
const REG_EBRACK: i32 = 7;
const REG_EPAREN: i32 = 8;
const REG_EBRACE: i32 = 9;
const REG_BADBR: i32 = 10;
const REG_ERANGE: i32 = 11;
const REG_ESPACE: i32 = 12;
const REG_BADRPT: i32 = 13;
const REG_ASSERT: i32 = 15;
const REG_INVARG: i32 = 16;
const REG_MIXED: i32 = 17;
const REG_BADOPT: i32 = 18;
const REG_ETOOBIG: i32 = 19;
const REG_ECOLORS: i32 = 20;

// ── stack depth ──
//
// Postgres's regex code checks `stack_is_too_deep()` (the C stack against max_stack_depth,
// 2MB) at the entry of its recursive functions and fails with REG_ETOOBIG ("regular
// expression is too complex"). The depth is emulated, so the same patterns fail: each
// recursive function adds the frame Postgres 18.6's build gives it, over the stack a backend
// has in use when it compiles or runs a pattern. The real stack is checked too, as a
// backstop.
//
// The frames are those of the reference image's build for the architecture this engine is
// built for (the pinned image is multi-architecture, so a host runs the reference natively):
// x86-64 from its binary's prologues (return address, pushes, `sub rsp`; objdump of
// /usr/local/bin/postgres), aarch64 from its build (gcc -O2) likewise. The base is what
// makes the deepest nesting of groups match: `(` × n `a` `)` × n compiles up to n = 8,710
// on x86-64.

/// The value for this build's architecture: (x86-64, aarch64).
const fn arch(x86_64: usize, aarch64: usize) -> usize {
    if cfg!(target_arch = "aarch64") { aarch64 } else { x86_64 }
}

/// `max_stack_depth` (2048kB).
const MAX_STACK_DEPTH: usize = 2048 * 1024;
/// The stack in use when `pg_regcomp` runs (its own frame included), below a plain query.
const BASE_DEPTH: usize = arch(6248, 6744);
/// `rstacktoodeep` and `stack_is_too_deep`'s frames at a check.
const CHECK_FRAMES: usize = 32;

/// Frame sizes of the recursive functions (and the callers between them and the entry
/// points), as Postgres's build lays them out.
pub(crate) mod frame {
    use super::arch;
    pub const PARSE: usize = arch(80, 96);
    /// parsebranch, with parseqatom inlined into it.
    pub const PARSEBRANCH: usize = arch(160, 176);
    pub const SUBRE: usize = 64;
    pub const NFATREE: usize = 112;
    pub const NFANODE: usize = 64;
    pub const OPTIMIZE: usize = 144;
    pub const CLEANUP: usize = 32;
    pub const MARKREACHABLE: usize = arch(32, 48);
    pub const MARKCANREACH: usize = 48;
    pub const DUPTRAVERSE: usize = arch(32, 48);
    pub const DELTRAVERSE: usize = 48;
    pub const REMOVETRAVERSE: usize = arch(48, 64);
    pub const CLEARTRAVERSE: usize = 32;
    pub const EMPTYREACHABLE: usize = 48;
    pub const FINDCONSTRAINTLOOP: usize = 64;
    pub const CLONESUCCESSORSTATES: usize = 112;
    pub const CHECKMATCHALL_RECURSE: usize = arch(64, 80);
    /// The stack in use when pg_regexec's searches run, less that of pg_regcomp's body:
    /// pg_regexec keeps its matching state (`struct vars`, with its small DFAs) on the
    /// stack. (x86-64: RE_wchar_execute's 160 and pg_regexec's 34,768, with find and
    /// cfind inlined, against RE_compile_and_cache's 288 and pg_regcomp's 320.)
    pub const REGEXEC_LESS_REGCOMP: isize = arch(34320, 34520) as isize;
    /// cdissect, with the c*dissect functions inlined into it.
    pub const CDISSECT: usize = 128;
    /// miss, with lacon inlined into it; and the DFA searches lacon runs. (x86-64: 208, and the
    /// 16 it pushes for shortest's seventh argument.)
    pub const MISS: usize = 224;
    pub const LONGEST: usize = arch(96, 112);
    pub const SHORTEST: usize = arch(112, 128);
}

thread_local! {
    /// The emulated stack in use beyond BASE_DEPTH.
    static VSTACK: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// A frame of the emulated stack, popped when dropped.
pub(crate) struct Frame(isize);

impl Frame {
    #[inline]
    pub(crate) fn enter(bytes: usize) -> Frame {
        Frame::enter_signed(bytes as isize)
    }

    #[inline]
    pub(crate) fn enter_signed(bytes: isize) -> Frame {
        VSTACK.with(|v| v.set(v.get() + bytes));
        Frame(bytes)
    }
}

impl Drop for Frame {
    #[inline]
    fn drop(&mut self) {
        VSTACK.with(|v| v.set(v.get() - self.0));
    }
}

/// `STACK_TOO_DEEP`: recursion in the compiler and executor fails with REG_ETOOBIG.
fn too_deep() -> bool {
    let used = BASE_DEPTH as isize + VSTACK.with(|v| v.get()) + CHECK_FRAMES as isize;
    if used > MAX_STACK_DEPTH as isize {
        return true;
    }
    if crate::stack::near_end() || crate::stack::check().is_err() {
        // The stack it runs on is nearly used up: the caller retries on a bigger one.
        REAL_STACK_OUT.with(|r| r.set(true));
        return true;
    }
    false
}

thread_local! {
    /// The real stack, not the emulated one, stopped the last compile or match.
    static REAL_STACK_OUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f`, again on a big-stack thread if the thread's own stack ran out (`big`: from the
/// start). Returns whether the big stack was needed.
fn with_enough_stack<T: Send>(big: bool, f: impl Fn() -> T + Send + Sync) -> (T, bool) {
    if big {
        return (on_big_stack(&f), true);
    }
    REAL_STACK_OUT.with(|r| r.set(false));
    let r = f();
    if REAL_STACK_OUT.with(|r| r.replace(false)) {
        PENDING.with(|p| *p.borrow_mut() = None);
        return (on_big_stack(&f), true);
    }
    (r, false)
}

// ── interrupts ──
//
// Where Postgres's regex code says INTERRUPT (CHECK_FOR_INTERRUPTS), a pending query cancel
// or an expired statement_timeout fails the statement; the engine stops and reports it.

/// What a running pattern checks: the session's cancel flag and the statement deadline.
#[derive(Clone)]
pub struct Interrupts {
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub deadline: Option<std::time::Instant>,
}

thread_local! {
    static INTERRUPTS: RefCell<Option<Interrupts>> = const { RefCell::new(None) };
    /// The cancel error an INTERRUPT point raised.
    static PENDING: RefCell<Option<Error>> = const { RefCell::new(None) };
    static TICKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Run `f` with interrupt checks against `i`.
pub fn with_interrupts<T>(i: Interrupts, f: impl FnOnce() -> T) -> T {
    let _g = crate::scoped::set_ref(&INTERRUPTS, Some(i), "regex::INTERRUPTS");
    f()
}

/// The state not at rest (`scoped::unsettled_for_test`).
pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    if INTERRUPTS.with_borrow(|i| i.is_some()) {
        out.push("regex::INTERRUPTS");
    }
}

/// Error code of an interrupted compile or match (the error itself is PENDING).
pub(crate) const REG_INTERRUPTED: i32 = -1000;

/// `INTERRUPT(re)`: Err when the statement was canceled or timed out.
pub(crate) fn interrupt() -> R<()> {
    let t = TICKS.with(|t| {
        let n = t.get().wrapping_add(1);
        t.set(n);
        n
    });
    if !t.is_multiple_of(64) {
        return Ok(());
    }
    let err = INTERRUPTS.with(|x| {
        let x = x.borrow();
        let i = x.as_ref()?;
        if i.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            return Some(Error::new(
                code::QUERY_CANCELED,
                "canceling statement due to user request",
            ));
        }
        if i.deadline.is_some_and(|d| std::time::Instant::now() > d) {
            return Some(Error::new(
                code::QUERY_CANCELED,
                "canceling statement due to statement timeout",
            ));
        }
        None
    });
    match err {
        Some(e) => {
            PENDING.with(|p| *p.borrow_mut() = Some(e));
            Err(REG_INTERRUPTED)
        }
        None => Ok(()),
    }
}

/// `MaxAllocSize`: the largest request palloc grants.
const MAX_ALLOC_SIZE: usize = 0x3fff_ffff;

/// The regex library's `MALLOC(n)` is `palloc_extended`, which fails for a request above
/// MaxAllocSize whatever memory there is ("invalid memory alloc request size"): Err (the
/// error PENDING) for such an `n`.
pub(crate) fn palloc(n: usize) -> R<()> {
    if n <= MAX_ALLOC_SIZE {
        return Ok(());
    }
    PENDING.with(|p| {
        *p.borrow_mut() = Some(Error::new(
            code::INTERNAL_ERROR,
            format!("invalid memory alloc request size {n}"),
        ))
    });
    Err(REG_INTERRUPTED)
}

/// `CHECK_FOR_INTERRUPTS` between matches (replace_text_regexp's loop).
pub fn check_interrupts() -> Result<()> {
    match interrupt() {
        Ok(()) => Ok(()),
        Err(_) => Err(PENDING
            .with(|p| p.borrow_mut().take())
            .unwrap_or_else(|| Error::new(code::QUERY_CANCELED, "canceling statement"))),
    }
}

/// Whether an INTERRUPT point fired (for code that can't return an error there).
pub(crate) fn interrupted() -> bool {
    PENDING.with(|p| p.borrow().is_some())
}

/// The error a failed compile or match reports: a pending cancel, else the regex error.
fn take_error(e: i32, what: impl FnOnce(i32) -> Error) -> Error {
    match PENDING.with(|p| p.borrow_mut().take()) {
        Some(c) => c,
        None => what(e),
    }
}

/// `pg_regerror`'s message for an error code.
pub fn regerror(e: i32) -> String {
    match e {
        REG_OKAY => "no errors detected",
        REG_NOMATCH => "failed to match",
        REG_BADPAT => "invalid regexp (reg version 0.8)",
        REG_ECOLLATE => "invalid collating element",
        REG_ECTYPE => "invalid character class",
        REG_EESCAPE => "invalid escape \\ sequence",
        REG_ESUBREG => "invalid backreference number",
        REG_EBRACK => "brackets [] not balanced",
        REG_EPAREN => "parentheses () not balanced",
        REG_EBRACE => "braces {} not balanced",
        REG_BADBR => "invalid repetition count(s)",
        REG_ERANGE => "invalid character range",
        REG_ESPACE => "out of memory",
        REG_BADRPT => "quantifier operand invalid",
        REG_ASSERT => "\"cannot happen\" -- you found a bug",
        REG_INVARG => "invalid argument to regex function",
        REG_MIXED => "character widths of regex and string differ",
        REG_BADOPT => "invalid embedded option",
        REG_ETOOBIG => "regular expression is too complex",
        REG_ECOLORS => "too many colors",
        _ => return format!("*** unknown regex error code 0x{e:x} ***"),
    }
    .to_string()
}

/// A compiled regular expression.
#[derive(Debug)]
pub struct Regex {
    c: comp::Compiled,
    /// Compiled from a long pattern: its recursion can go as deep as Postgres's 2MB stack
    /// allows, which in this engine's frames needs a bigger stack than a session thread's.
    deep: bool,
}

/// Patterns at least this long compile and run on a big-stack helper thread.
const DEEP_PATTERN: usize = 1000;

/// Run `f` on a thread with a stack big enough for the deepest recursion the emulated
/// 2MB stack admits, carrying the interrupt checks there and a pending cancel back.
fn on_big_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    let intr = INTERRUPTS.with(|x| x.borrow().clone());
    let (r, pending) = std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(1 << 30)
            .spawn_scoped(s, move || {
                INTERRUPTS.with(|x| *x.borrow_mut() = intr);
                let r = f();
                (r, PENDING.with(|p| p.borrow_mut().take()))
            })
            .expect("regex thread")
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    });
    PENDING.with(|p| *p.borrow_mut() = pending);
    r
}

/// The characters of a string, as the engine matches them.
pub fn chrs(s: &str) -> Vec<Chr> {
    s.chars().map(|c| c as Chr).collect()
}

impl Regex {
    /// `pg_regcomp`, reporting failures as Postgres's regexp.c does.
    pub fn new(pattern: &str, cflags: i32, strategy: Strategy) -> Result<Regex> {
        PENDING.with(|p| *p.borrow_mut() = None);
        let pat = chrs(pattern);
        let (r, deep) = with_enough_stack(pat.len() >= DEEP_PATTERN, || {
            comp::regcomp(&pat, cflags, strategy)
        });
        if interrupted() {
            return Err(take_error(REG_INTERRUPTED, |_| unreachable!()));
        }
        r.map(|c| Regex { c, deep }).map_err(|e| {
            take_error(e, |e| {
                Error::new(
                    code::INVALID_REGULAR_EXPRESSION,
                    format!("invalid regular expression: {}", regerror(e)),
                )
            })
        })
    }

    /// The number of capturing subexpressions (`re_nsub`).
    pub fn nsub(&self) -> usize {
        self.c.nsub
    }

    /// `pg_regexec`: search `data` from `search_start` (a character offset). On a match,
    /// returns `nmatch` (start, end) character offsets: the whole match, then subexpressions
    /// (-1 where one didn't take part).
    pub fn exec(
        &self,
        data: &[Chr],
        search_start: usize,
        nmatch: usize,
    ) -> Result<Option<Vec<(isize, isize)>>> {
        PENDING.with(|p| *p.borrow_mut() = None);
        let run = || {
            let _f = Frame::enter_signed(frame::REGEXEC_LESS_REGCOMP);
            exec::regexec(&self.c, data, search_start, nmatch)
        };
        let (r, _) = with_enough_stack(self.deep, run);
        if interrupted() {
            return Err(take_error(REG_INTERRUPTED, |_| unreachable!()));
        }
        r.map_err(|e| {
            take_error(e, |e| {
                Error::new(
                    code::INVALID_REGULAR_EXPRESSION,
                    format!("regular expression failed: {}", regerror(e)),
                )
            })
        })
    }

    /// Whether the pattern matches anywhere in `s`.
    pub fn is_match(&self, s: &str) -> Result<bool> {
        Ok(self.exec(&chrs(s), 0, 0)?.is_some())
    }
}

/// At most this many compiled patterns are cached (regexp.c's MAX_CACHED_RES).
const MAX_CACHED_RES: usize = 32;

type CacheEntry = (String, i32, Strategy, Arc<Regex>);

thread_local! {
    /// Recently used patterns, most recent first (regexp.c's self-organizing list).
    static CACHE: RefCell<Vec<CacheEntry>> = const { RefCell::new(Vec::new()) };
}

/// `RE_compile_and_cache`.
pub fn compile(pattern: &str, cflags: i32, strategy: Strategy) -> Result<Arc<Regex>> {
    let hit = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let i = c
            .iter()
            .position(|(p, f, s, _)| *f == cflags && *s == strategy && p == pattern)?;
        let e = c.remove(i);
        let re = e.3.clone();
        c.insert(0, e);
        Some(re)
    });
    if let Some(re) = hit {
        return Ok(re);
    }
    let re = Arc::new(Regex::new(pattern, cflags, strategy)?);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.truncate(MAX_CACHED_RES - 1);
        c.insert(0, (pattern.to_string(), cflags, strategy, re.clone()));
    });
    Ok(re)
}

/// `RE_compile_and_execute`: whether `pattern` matches in `s`.
pub fn compile_and_match(pattern: &str, s: &str, cflags: i32, strategy: Strategy) -> Result<bool> {
    compile(pattern, cflags | REG_NOSUB, strategy)?.is_match(s)
}
