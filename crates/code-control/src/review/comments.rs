//! Is an edit comment-only? For `auto_approve: comment_only`. Conservative on purpose: a file
//! counts only when (a) the language is known, (b) every changed line is blank or comment-only
//! on its side, and (c) the code that's left once comments are dropped (strings kept, whitespace
//! normalised; indentation kept where it matters) is identical. Anything it can't be sure of is
//! "not comment-only", so the change just needs a normal review.

use super::lines;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quote {
    /// `"…"` with backslash escapes.
    Double,
    /// `'…'` with backslash escapes.
    Single,
    /// `` `…` `` (JS/TS template literals, Go raw strings).
    Backtick,
    /// Python `"""…"""` and `'''…'''`.
    Triple,
    /// Rust: `'` is a char literal when it closes right away, else a lifetime (code).
    RustChar,
    /// Rust raw strings `r"…"`, `r#"…"#`, `br##"…"##`.
    RustRaw,
}

#[derive(Clone, Debug)]
struct Syntax {
    line: &'static [&'static str],
    block: &'static [(&'static str, &'static str)],
    nested_blocks: bool,
    quotes: &'static [Quote],
    /// `#`-style line comments only after whitespace (shell: `$#`, `${#x}`).
    hash_needs_space: bool,
    /// Indentation is syntax (Python, YAML): keep it when comparing.
    indent: bool,
}

const C_QUOTES: &[Quote] = &[Quote::Double, Quote::Single];
const JS_QUOTES: &[Quote] = &[Quote::Double, Quote::Single, Quote::Backtick];

fn syntax_for(path: &str) -> Option<Syntax> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let c = |quotes| Syntax { line: &["//"], block: &[("/*", "*/")], nested_blocks: false, quotes, hash_needs_space: false, indent: false };
    let hash = |hash_needs_space, indent| Syntax {
        line: &["#"],
        block: &[],
        nested_blocks: false,
        quotes: C_QUOTES,
        hash_needs_space,
        indent,
    };
    Some(match ext {
        "rs" => Syntax { nested_blocks: true, quotes: &[Quote::Double, Quote::RustChar, Quote::RustRaw], ..c(C_QUOTES) },
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "java" | "cs" | "kt" | "kts" | "scala" | "dart" | "proto" | "m"
        | "mm" => c(C_QUOTES),
        "swift" => Syntax { nested_blocks: true, ..c(C_QUOTES) },
        "go" => Syntax { quotes: &[Quote::Double, Quote::Single, Quote::Backtick], ..c(C_QUOTES) },
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => c(JS_QUOTES),
        "php" => Syntax { line: &["//", "#"], ..c(C_QUOTES) },
        "css" => Syntax { line: &[], ..c(C_QUOTES) },
        "scss" | "less" => c(C_QUOTES),
        "py" | "pyi" => Syntax { quotes: &[Quote::Triple, Quote::Double, Quote::Single], ..hash(false, true) },
        "rb" | "pl" | "pm" | "r" | "toml" | "cmake" | "tf" | "hcl" => hash(false, false),
        "sh" | "bash" | "zsh" | "fish" => hash(true, false),
        "yaml" | "yml" => hash(true, true),
        "sql" => Syntax { line: &["--"], block: &[("/*", "*/")], nested_blocks: false, quotes: &[Quote::Single, Quote::Double], hash_needs_space: false, indent: false },
        "lua" => Syntax { line: &["--"], block: &[("--[[", "]]")], nested_blocks: false, quotes: C_QUOTES, hash_needs_space: false, indent: false },
        "hs" => Syntax { line: &["--"], block: &[("{-", "-}")], nested_blocks: true, quotes: &[Quote::Double], hash_needs_space: false, indent: true },
        "html" | "htm" | "xml" | "svg" | "vue" | "svelte" => {
            Syntax { line: &[], block: &[("<!--", "-->")], nested_blocks: false, quotes: &[], hash_needs_space: false, indent: false }
        }
        _ => match lower.as_str() {
            "dockerfile" | "makefile" | "gemfile" | "rakefile" | "cmakelists.txt" => hash(true, false),
            _ => return None,
        },
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Code,
    Comment,
}

/// Classify every byte of `text` as code (strings included), comment or whitespace.
fn classify(text: &[u8], s: &Syntax) -> Vec<Class> {
    let mut out = vec![Class::Code; text.len()];
    let mut i = 0;
    let starts = |i: usize, tok: &str| text[i..].starts_with(tok.as_bytes());
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    while i < text.len() {
        let b = text[i];
        // Block comments (possibly nested).
        if let Some(&(open, close)) = s.block.iter().find(|(o, _)| starts(i, o)) {
            let mut depth = 1;
            let start = i;
            i += open.len();
            while i < text.len() && depth > 0 {
                if s.nested_blocks && starts(i, open) {
                    depth += 1;
                    i += open.len();
                } else if starts(i, close) {
                    depth -= 1;
                    i += close.len();
                } else {
                    i += 1;
                }
            }
            out[start..i].iter_mut().for_each(|c| *c = Class::Comment);
            continue;
        }
        // Line comments.
        if let Some(tok) = s.line.iter().find(|t| starts(i, t)) {
            let ok = *tok != "#" || !s.hash_needs_space || i == 0 || text[i - 1].is_ascii_whitespace();
            if ok {
                let start = i;
                while i < text.len() && text[i] != b'\n' {
                    i += 1;
                }
                out[start..i].iter_mut().for_each(|c| *c = Class::Comment);
                continue;
            }
        }
        // Strings: code, but their contents never start comments.
        let mut matched = false;
        for q in s.quotes {
            let end = match q {
                Quote::Triple if starts(i, "\"\"\"") || starts(i, "'''") => {
                    let delim = &text[i..i + 3];
                    let mut j = i + 3;
                    while j + 3 <= text.len() && &text[j..j + 3] != delim {
                        j += if text[j] == b'\\' { 2 } else { 1 };
                    }
                    Some((j + 3).min(text.len()))
                }
                Quote::Double if b == b'"' => Some(escaped(text, i, b'"')),
                Quote::Single if b == b'\'' => Some(escaped(text, i, b'\'')),
                Quote::Backtick if b == b'`' => Some(escaped(text, i, b'`')),
                Quote::RustChar if b == b'\'' => {
                    if text.get(i + 1) == Some(&b'\\') {
                        Some(escaped(text, i, b'\''))
                    } else {
                        // One character (maybe multi-byte) then `'`: a char literal.
                        let ch_len = std::str::from_utf8(&text[i + 1..text.len().min(i + 5)])
                            .ok()
                            .or_else(|| std::str::from_utf8(&text[i + 1..text.len().min(i + 4)]).ok())
                            .or_else(|| std::str::from_utf8(&text[i + 1..text.len().min(i + 3)]).ok())
                            .or_else(|| std::str::from_utf8(&text[i + 1..text.len().min(i + 2)]).ok())
                            .and_then(|s| s.chars().next())
                            .map(char::len_utf8);
                        match ch_len {
                            Some(n) if text.get(i + 1 + n) == Some(&b'\'') => Some(i + 2 + n),
                            // A lifetime: just code.
                            _ => None,
                        }
                    }
                }
                Quote::RustRaw if (b == b'r' || (b == b'b' && text.get(i + 1) == Some(&b'r'))) && (i == 0 || !ident(text[i - 1])) => {
                    let mut j = i + if b == b'b' { 2 } else { 1 };
                    let hashes = text[j..].iter().take_while(|&&c| c == b'#').count();
                    j += hashes;
                    if text.get(j) == Some(&b'"') {
                        let mut close = Vec::with_capacity(hashes + 1);
                        close.push(b'"');
                        close.extend(std::iter::repeat_n(b'#', hashes));
                        let mut k = j + 1;
                        while k < text.len() && !text[k..].starts_with(&close) {
                            k += 1;
                        }
                        Some((k + close.len()).min(text.len()))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(end) = end {
                i = end;
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if b.is_ascii_whitespace() {
            out[i] = Class::Space;
        }
        i += 1;
    }
    out
}

/// End (exclusive) of a quoted string starting at `i`, with backslash escapes. Unclosed: the
/// end of the line for `"`/`'`, the end of the text for backticks.
fn escaped(text: &[u8], i: usize, q: u8) -> usize {
    let mut j = i + 1;
    while j < text.len() {
        match text[j] {
            b'\\' => j += 2,
            c if c == q => return j + 1,
            b'\n' if q != b'`' => return j,
            _ => j += 1,
        }
    }
    text.len()
}

/// The code with comments dropped: whitespace runs become one space; with `indent`, each
/// line's leading whitespace is kept and lines stay lines.
fn code_only(text: &[u8], classes: &[Class], indent: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut line_start = 0;
    while line_start < text.len() {
        let end = text[line_start..].iter().position(|&b| b == b'\n').map(|p| line_start + p).unwrap_or(text.len());
        let line = &text[line_start..end];
        let cls = &classes[line_start..end];
        let mut buf = Vec::new();
        let mut pending_space = false;
        let mut seen_code = false;
        for (k, &b) in line.iter().enumerate() {
            match cls[k] {
                Class::Comment => pending_space = seen_code,
                Class::Space if !seen_code && indent => buf.push(b),
                Class::Space => pending_space = seen_code,
                Class::Code => {
                    if pending_space {
                        buf.push(b' ');
                        pending_space = false;
                    }
                    seen_code = true;
                    buf.push(b);
                }
            }
        }
        if seen_code {
            out.extend_from_slice(&buf);
            out.push(if indent { b'\n' } else { b' ' });
        }
        line_start = end + 1;
    }
    out
}

/// Line numbers (1-based) that hold only whitespace and comments.
fn comment_lines(text: &[u8], classes: &[Class]) -> Vec<bool> {
    let mut out = vec![true]; // index 0 unused
    let mut line_ok = true;
    for (i, &b) in text.iter().enumerate() {
        if b == b'\n' {
            out.push(line_ok);
            line_ok = true;
        } else if classes[i] == Class::Code {
            line_ok = false;
        }
    }
    out.push(line_ok);
    out
}

/// Is changing `old` into `new` at `path` comment-only?
pub fn comment_only(path: &str, old: &[u8], new: &[u8]) -> bool {
    let Some(s) = syntax_for(path) else { return false };
    if code_store::git::is_binary(old) || code_store::git::is_binary(new) {
        return false;
    }
    let (oc, nc) = (classify(old, &s), classify(new, &s));
    if code_only(old, &oc, s.indent) != code_only(new, &nc, s.indent) {
        return false;
    }
    let (ol, nl) = (comment_lines(old, &oc), comment_lines(new, &nc));
    let (Ok(os), Ok(ns)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else { return false };
    let changed = lines::changed_lines(os, ns);
    changed.removed.iter().all(|&n| ol.get(n).copied().unwrap_or(false)) && changed.added.iter().all(|&n| nl.get(n).copied().unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::comment_only as co;

    #[test]
    fn rust() {
        assert!(co("a.rs", b"fn a() {}\n", b"// hello\nfn a() {}\n"));
        assert!(co("a.rs", b"/// docs\nfn a() {}\n", b"/// better docs\n/// more\nfn a() {}\n"));
        assert!(co("a.rs", b"fn a() {\n    /* x */\n}\n", b"fn a() {\n    /* y\n       z */\n}\n"));
        assert!(co("a.rs", b"/* a /* nested */ b */\nfn a() {}\n", b"/* c /* nested */ d */\nfn a() {}\n"));
        assert!(!co("a.rs", b"fn a() { 1 }\n", b"fn a() { 2 }\n"));
        // A trailing comment's line also holds code: not comment-only (conservative).
        assert!(!co("a.rs", b"let x = 1; // a\n", b"let x = 1; // b\n"));
        // Comment markers inside strings are strings.
        assert!(!co("a.rs", b"let u = \"http://a\";\n", b"let u = \"http://b\";\n"));
        assert!(!co("a.rs", b"let u = r#\"// a\"#;\n", b"let u = r#\"// b\"#;\n"));
        assert!(!co("a.rs", b"let c = '/';\nlet x = 1;\n", b"let c = '/';\nlet x = 2;\n"));
        // Lifetimes aren't strings.
        assert!(co("a.rs", b"fn f<'a>(x: &'a str) {}\n", b"// lifetimes\nfn f<'a>(x: &'a str) {}\n"));
        // Moving code is a code change, even if the tokens are the same.
        assert!(!co("a.rs", b"a();\nb();\n", b"b();\na();\n"));
        // Whitespace-only changes in brace languages are fine (with comments).
        assert!(!co("a.rs", b"fn a(){}\n", b"fn a() {}\n"), "whitespace inside a code line changes a code line");
        assert!(co("a.rs", b"fn a() {}\n", b"fn a() {}\n\n// end\n"));
    }

    #[test]
    fn js_ts_go_c() {
        assert!(co("a.ts", b"const a = 1;\n", b"/** the answer */\nconst a = 1;\n"));
        assert!(!co("a.ts", b"const s = `// ${a}`;\n", b"const s = `// ${b}`;\n"));
        assert!(!co("a.js", b"const r = '/* x */';\n", b"const r = '/* y */';\n"));
        assert!(co("main.go", b"package main\n", b"// Package main does things.\npackage main\n"));
        assert!(!co("main.go", b"var s = `a // b`\n", b"var s = `a // c`\n"));
        assert!(co("x.c", b"int x;\n", b"/* header */\nint x;\n"));
        assert!(co("x.css", b"a { color: red; }\n", b"/* links */\na { color: red; }\n"));
    }

    #[test]
    fn hash_languages() {
        assert!(co("a.py", b"def f():\n    return 1\n", b"# helper\ndef f():\n    # one\n    return 1\n"));
        assert!(!co("a.py", b"def f():\n    return 1\n", b"def f():\n        return 1\n"), "indentation is code in Python");
        assert!(!co("a.py", b"s = '# a'\n", b"s = '# b'\n"));
        assert!(!co("a.py", b"\"\"\"doc a\"\"\"\n", b"\"\"\"doc b\"\"\"\n"), "docstrings are strings");
        assert!(co("run.sh", b"echo hi\n", b"# say hi\necho hi\n"));
        assert!(!co("run.sh", b"echo ${#a}\n", b"echo ${#b}\n"), "${{#x}} isn't a comment");
        assert!(co("a.yaml", b"a: 1\n", b"# config\na: 1\n"));
        assert!(!co("a.yaml", b"a:\n  b: 1\n", b"a:\nb: 1\n"));
        assert!(co("Dockerfile", b"FROM alpine\n", b"# base\nFROM alpine\n"));
        assert!(co("q.sql", b"SELECT 1;\n", b"-- check\nSELECT 1;\n"));
        assert!(!co("q.sql", b"SELECT '--a';\n", b"SELECT '--b';\n"));
    }

    #[test]
    fn unknown_and_binary_files_are_not_comment_only() {
        assert!(!co("README.md", b"# a\n", b"# b\n"));
        assert!(!co("data.bin", b"\0\x01", b"\0\x02"));
        assert!(!co("a.rs", b"\0fn a() {}", b"\0// x\nfn a() {}"));
        assert!(!co("noext", b"a", b"b"));
        assert!(co("a.rs", b"fn a() {}\n", b"fn a() {}\n"), "no change is trivially comment-only");
    }
}
