//! A Rust port of the TRE POSIX regex library, as vendored by Redis 8.10
//! (`deps/tre`), for the `RE` predicate of `ARGREP`.
//!
//! Redis compiles with `tre_regncompb(.., REG_EXTENDED | REG_NOSUB |
//! REG_USEBYTES [| REG_ICASE])`, rejects patterns with back references, and
//! matches with `tre_regnexecb(.., 0, NULL, 0)`. This module reproduces that
//! configuration byte for byte: which patterns compile, the error each bad
//! pattern gets, and whether a byte string matches, including TRE's quirks
//! (empty atoms, `(?i)` scoping, `\Q..\E`, `$` matching before a NUL byte in
//! the automaton but not in the literal fast path, the negated-bracket
//! complement, ...). Character classes are the C locale (ASCII only), and the
//! order-sensitive bracket complement uses a port of musl's `qsort()`, the
//! libc of the reference build.
//!
//! Matching uses TRE's own TNFA (a position automaton with assertions on its
//! transitions) simulated breadth first, so it is linear in the haystack and
//! never backtracks.
//!
//! Original copyright and license (BSD 2-clause), from `deps/tre/LICENSE`:
//!
//! ```text
//! This is the license, copyright notice, and disclaimer for TRE, a regex
//! matching package (library and tools) with support for approximate
//! matching.
//!
//! Copyright (c) 2001-2009 Ville Laurikari <vl@iki.fi>
//! All rights reserved.
//!
//! Redistribution and use in source and binary forms, with or without
//! modification, are permitted provided that the following conditions
//! are met:
//!
//!   1. Redistributions of source code must retain the above copyright
//!      notice, this list of conditions and the following disclaimer.
//!
//!   2. Redistributions in binary form must reproduce the above copyright
//!      notice, this list of conditions and the following disclaimer in the
//!      documentation and/or other materials provided with the distribution.
//!
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDER AND CONTRIBUTORS
//! ``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
//! LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
//! A PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE COPYRIGHT
//! HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//! SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
//! LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//! DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//! THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//! (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
//! OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//! ```
//!
//! The musl `qsort()` port in `qsort.rs` is Copyright (C) 2011 Valentin
//! Ochs, MIT licensed.

mod compile;
mod ctype;
mod exec;
mod parse;
mod qsort;

#[cfg(test)]
mod reference_table;
#[cfg(test)]
mod tests;

use compile::{LitOpt, Nfa};
use parse::{REG_EXTENDED, REG_ICASE, REG_NOSUB, REG_USEBYTES};

/// `TRE_MAX_RE`: longer patterns fail with `REG_ESPACE`.
const MAX_RE: usize = 65536;

/// A `tre_regcomp()` error code (`reg_errcode_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileError {
    BadPat,
    ECollate,
    ECtype,
    EEscape,
    ESubreg,
    EBrack,
    EParen,
    EBrace,
    BadBr,
    ERange,
    ESpace,
    BadRpt,
    BadMax,
}

impl CompileError {
    /// The `tre_regerror()` message.
    pub fn message(self) -> &'static str {
        match self {
            CompileError::BadPat => "Invalid regexp",
            CompileError::ECollate => "Unknown collating element",
            CompileError::ECtype => "Unknown character class name",
            CompileError::EEscape => "Trailing backslash",
            CompileError::ESubreg => "Invalid back reference",
            CompileError::EBrack => "Missing ']'",
            CompileError::EParen => "Missing ')'",
            CompileError::EBrace => "Missing '}'",
            CompileError::BadBr => "Invalid contents of {}",
            CompileError::ERange => "Invalid character range",
            CompileError::ESpace => "Out of memory",
            CompileError::BadRpt => "Invalid use of repetition operators",
            CompileError::BadMax => "Maximum repetition in {} larger than 255",
        }
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for CompileError {}

enum Program {
    /// `tnfa->literal_opt` is set: TRE matches without the automaton.
    Literal(LitOpt),
    Automaton(Box<Nfa>),
}

/// A compiled pattern.
pub struct Regex {
    has_backrefs: bool,
    program: Program,
}

impl std::fmt::Debug for Regex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let program = match &self.program {
            Program::Literal(l) => format!("literal({:?})", l.mode),
            Program::Automaton(n) => format!("tnfa({} states)", n.states.len()),
        };
        f.debug_struct("Regex")
            .field("has_backrefs", &self.has_backrefs)
            .field("program", &program)
            .finish()
    }
}

impl Regex {
    /// `tre_regncompb(pattern, REG_EXTENDED | REG_NOSUB | REG_USEBYTES`,
    /// plus `REG_ICASE` when `icase`). The pattern is length-delimited, so
    /// NUL bytes in it are ordinary characters.
    pub fn new(pattern: &[u8], icase: bool) -> Result<Regex, CompileError> {
        if pattern.len() > MAX_RE {
            return Err(CompileError::ESpace);
        }
        let cflags = REG_EXTENDED | REG_NOSUB | REG_USEBYTES | if icase { REG_ICASE } else { 0 };
        let mut ast = parse::Ast::default();
        let parsed = parse::parse(&mut ast, pattern, cflags)?;
        let re_nsub = parsed.submatch_id - 1;
        // Back references and approximate matching cannot be combined.
        if parsed.max_backref >= 0 && parsed.have_approx {
            return Err(CompileError::BadPat);
        }
        // Referring to a nonexistent subexpression.
        if parsed.max_backref > re_nsub {
            return Err(CompileError::ESubreg);
        }
        let has_backrefs = parsed.max_backref >= 0;

        // The literal optimiser looks at the parsed tree with the outer
        // flags, so TRE skips it when flags change inline.
        if !parsed.have_inline_cflags
            && let Some(opt) = compile::litopt(&ast, parsed.root, cflags)
        {
            return Ok(Regex {
                has_backrefs,
                program: Program::Literal(opt),
            });
        }
        compile::expand(&mut ast, parsed.root)?;
        let nfa = compile::build_nfa(&mut ast, parsed.root)?;
        Ok(Regex {
            has_backrefs,
            program: Program::Automaton(Box::new(nfa)),
        })
    }

    /// `tre_have_backrefs()`.
    pub fn has_backrefs(&self) -> bool {
        self.has_backrefs
    }

    /// Unanchored search: true when `tre_regnexecb(.., haystack, len, 0,
    /// NULL, 0)` would return `REG_OK`. The haystack is length-delimited.
    ///
    /// Patterns with back references are not supported (Redis rejects them
    /// before matching); for those a back reference matches any single byte,
    /// which is what TRE's parallel matcher would do.
    pub fn is_match(&self, haystack: &[u8]) -> bool {
        match &self.program {
            Program::Literal(opt) => exec::literal_match(opt, haystack),
            Program::Automaton(nfa) => exec::nfa_match(nfa, haystack),
        }
    }
}
