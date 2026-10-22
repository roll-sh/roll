//! What a file defines and references, from per-language patterns. Definitions are matched
//! line by line (`fn`, `struct`, `class`, `def`, `func`, `interface`, `type`, `const`, …);
//! references are identifier occurrences outside line comments, minus each language's
//! keywords.

use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolDef {
    pub name: String,
    pub line: u64,
    pub kind: String,
}

#[derive(Clone, Debug, Default)]
pub struct FileFacts {
    pub lang: Option<&'static str>,
    pub symbols: Vec<SymbolDef>,
    /// identifier → (occurrences, the first few lines it's on).
    pub refs: BTreeMap<String, (u64, Vec<u64>)>,
}

/// Lines kept per referenced identifier.
const REF_LINES: usize = 5;
/// Distinct identifiers kept per file.
const MAX_REFS: usize = 3000;

pub fn language(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" | "pyi" => "python",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "rb" => "ruby",
        "php" => "php",
        _ => return None,
    })
}

/// Test files: under `tests/`, `test/`, `__tests__/`, `spec/`, or named like one.
pub fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    let dirs = p.starts_with("tests/") || p.starts_with("test/") || p.contains("/tests/") || p.contains("/test/") || p.contains("__tests__/") || p.starts_with("spec/") || p.contains("/spec/");
    let named = name.ends_with("_test.go")
        || name.starts_with("test_") && name.ends_with(".py")
        || name.ends_with("_test.py")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("test.java")
        || name.ends_with("test.kt")
        || name.ends_with("_spec.rb")
        || name.ends_with("_test.rb")
        || name.ends_with("test.php");
    dirs || named
}

struct Lang {
    defs: Vec<(Regex, &'static str)>,
    comment: &'static str,
    keywords: &'static [&'static str],
}

fn re(s: &str) -> Regex {
    Regex::new(s).expect("pattern")
}

const COMMON: &[&str] = &[
    "and", "as", "async", "await", "break", "case", "catch", "class", "const", "continue", "def", "default", "del", "do", "elif", "else", "enum",
    "export", "extends", "false", "final", "finally", "fn", "for", "from", "func", "function", "go", "if", "impl", "import", "in", "interface",
    "let", "loop", "match", "mod", "module", "mut", "new", "nil", "none", "not", "null", "or", "package", "pass", "private", "protected", "pub",
    "public", "raise", "return", "self", "static", "struct", "super", "switch", "this", "throw", "trait", "true", "try", "type", "typeof", "undefined",
    "use", "var", "void", "where", "while", "with", "yield", "None", "True", "False", "Self", "Some", "Ok", "Err", "String", "Vec", "Option", "Result",
    "int", "str", "bool", "char", "float", "double", "long", "string", "number", "any", "unsafe", "extern", "crate", "dyn", "ref", "move", "end",
    "then", "begin", "elsif", "unless", "require", "include", "namespace", "using", "template", "typename", "virtual", "override", "readonly",
];

fn langs() -> &'static BTreeMap<&'static str, Lang> {
    static L: OnceLock<BTreeMap<&'static str, Lang>> = OnceLock::new();
    L.get_or_init(|| {
        let mut m = BTreeMap::new();
        m.insert(
            "rust",
            Lang {
                defs: vec![
                    (re(r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_]\w*)"#), "function"),
                    (re(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(struct|enum|trait|union|type)\s+([A-Za-z_]\w*)"), "type"),
                    (re(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+(?:mut\s+)?([A-Za-z_]\w*)\s*:"), "const"),
                    (re(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)"), "module"),
                    (re(r"^\s*macro_rules!\s*([A-Za-z_]\w*)"), "macro"),
                ],
                comment: "//",
                keywords: COMMON,
            },
        );
        let ts = || Lang {
            defs: vec![
                (re(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)"), "function"),
                (re(r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)"), "class"),
                (re(r"^\s*(?:export\s+)?interface\s+([A-Za-z_$][\w$]*)"), "interface"),
                (re(r"^\s*(?:export\s+)?type\s+([A-Za-z_$][\w$]*)\s*(?:<[^=]*>)?\s*="), "type"),
                (re(r"^\s*(?:export\s+)?enum\s+([A-Za-z_$][\w$]*)"), "type"),
                (re(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*(?::[^=]+)?=>"), "function"),
                (re(r"^\s*(?:export\s+)?const\s+([A-Z][A-Z0-9_]+)\s*="), "const"),
            ],
            comment: "//",
            keywords: COMMON,
        };
        m.insert("typescript", ts());
        m.insert("javascript", ts());
        m.insert(
            "python",
            Lang {
                defs: vec![
                    (re(r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)"), "function"),
                    (re(r"^\s*class\s+([A-Za-z_]\w*)"), "class"),
                    (re(r"^([A-Z][A-Z0-9_]+)\s*(?::[^=]+)?="), "const"),
                ],
                comment: "#",
                keywords: COMMON,
            },
        );
        m.insert(
            "go",
            Lang {
                defs: vec![
                    (re(r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)"), "function"),
                    (re(r"^type\s+([A-Za-z_]\w*)\s+(?:struct|interface)"), "type"),
                    (re(r"^type\s+([A-Za-z_]\w*)\s"), "type"),
                    (re(r"^(?:const|var)\s+([A-Za-z_]\w*)"), "const"),
                ],
                comment: "//",
                keywords: COMMON,
            },
        );
        let jvm = || Lang {
            defs: vec![
                (re(r"^\s*(?:(?:public|private|protected|internal|abstract|final|static|sealed|data|open|inner)\s+)*(?:class|interface|enum|record|object)\s+([A-Za-z_]\w*)"), "class"),
                (re(r"^\s*(?:(?:public|private|protected|internal|override|suspend|inline|open)\s+)*fun\s+(?:<[^>]*>\s*)?(?:[A-Za-z_][\w.]*\.)?([A-Za-z_]\w*)"), "function"),
                (re(r"^\s*(?:(?:public|private|protected|static|final|abstract|synchronized|native|default)\s+)+[\w<>\[\],.? ]+\s+([A-Za-z_]\w*)\s*\("), "function"),
            ],
            comment: "//",
            keywords: COMMON,
        };
        m.insert("java", jvm());
        m.insert("kotlin", jvm());
        let c = || Lang {
            defs: vec![
                (re(r"^\s*(?:typedef\s+)?(?:struct|class|enum|union)\s+([A-Za-z_]\w*)\s*(?:\{|:|$|final)"), "type"),
                (re(r"^#define\s+([A-Za-z_]\w*)"), "macro"),
                (re(r"^(?:[A-Za-z_][\w:<>,*&\s]*?[\s*&])([A-Za-z_][\w:]*)\s*\([^;]*$"), "function"),
            ],
            comment: "//",
            keywords: COMMON,
        };
        m.insert("c", c());
        m.insert("cpp", c());
        m.insert(
            "ruby",
            Lang {
                defs: vec![(re(r"^\s*def\s+(?:self\.)?([A-Za-z_]\w*[?!]?)"), "function"), (re(r"^\s*(?:class|module)\s+([A-Z]\w*)"), "class")],
                comment: "#",
                keywords: COMMON,
            },
        );
        m.insert(
            "php",
            Lang {
                defs: vec![
                    (re(r"^\s*(?:(?:public|private|protected|static|abstract|final)\s+)*function\s+&?([A-Za-z_]\w*)"), "function"),
                    (re(r"^\s*(?:(?:abstract|final)\s+)*(?:class|interface|trait|enum)\s+([A-Za-z_]\w*)"), "class"),
                ],
                comment: "//",
                keywords: COMMON,
            },
        );
        m
    })
}

fn ident_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"[A-Za-z_$][A-Za-z0-9_$]*"))
}

const C_KEYWORDS: &[&str] = &["if", "for", "while", "switch", "return", "sizeof", "else"];

pub fn extract(path: &str, text: &str) -> FileFacts {
    let lang = language(path);
    let mut facts = FileFacts { lang, ..Default::default() };
    let l = lang.and_then(|n| langs().get(n));
    for (i, line) in text.lines().enumerate() {
        let n = i as u64 + 1;
        if line.len() > 2000 {
            continue;
        }
        let code = match l {
            Some(l) => strip_comment(line, l.comment),
            None => line,
        };
        if let Some(l) = l {
            for (re, kind) in &l.defs {
                if let Some(c) = re.captures(code) {
                    // The name is the last capture (rust `type` patterns capture the keyword first).
                    let Some(m) = (1..c.len()).rev().find_map(|g| c.get(g)) else { continue };
                    let name = m.as_str();
                    if C_KEYWORDS.contains(&name) || l.keywords.contains(&name) {
                        continue;
                    }
                    facts.symbols.push(SymbolDef { name: name.to_string(), line: n, kind: kind.to_string() });
                    break;
                }
            }
        }
        for m in ident_re().find_iter(code) {
            let w = m.as_str();
            if w.len() < 3 || w.len() > 64 || COMMON.contains(&w) {
                continue;
            }
            if !facts.refs.contains_key(w) && facts.refs.len() >= MAX_REFS {
                continue;
            }
            let e = facts.refs.entry(w.to_string()).or_insert((0, Vec::new()));
            e.0 += 1;
            if e.1.len() < REF_LINES && e.1.last() != Some(&n) {
                e.1.push(n);
            }
        }
    }
    facts
}

/// The line without a trailing line comment (not inside a string, roughly).
fn strip_comment<'a>(line: &'a str, marker: &str) -> &'a str {
    let mut in_str: Option<char> = None;
    let b: Vec<char> = line.chars().collect();
    let mut byte = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match in_str {
            Some(q) if c == '\\' => {
                byte += c.len_utf8() + b.get(i + 1).map_or(0, |x| x.len_utf8());
                i += 2;
                let _ = q;
                continue;
            }
            Some(q) if c == q => in_str = None,
            Some(_) => {}
            None if c == '"' || c == '\'' || c == '`' => in_str = Some(c),
            None if line[byte..].starts_with(marker) => return &line[..byte],
            None => {}
        }
        byte += c.len_utf8();
        i += 1;
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(path: &str, src: &str) -> Vec<(String, u64, String)> {
        extract(path, src).symbols.into_iter().map(|s| (s.name, s.line, s.kind)).collect()
    }

    #[test]
    fn rust() {
        let src = "pub fn parse(x: &str) -> u32 {\n    helper(x)\n}\npub(crate) async fn helper() {}\nstruct Point;\npub enum E {}\nconst MAX: usize = 3;\nmod inner {}\nmacro_rules! m {}\n// fn commented() {}\nimpl Foo { fn method(&self) {} }\n";
        let n = names("a.rs", src);
        let want = [("parse", 1), ("helper", 4), ("Point", 5), ("E", 6), ("MAX", 7), ("inner", 8), ("m", 9)];
        for (w, l) in want {
            assert!(n.iter().any(|(x, ln, _)| x == w && *ln == l), "{w}: {n:?}");
        }
        assert!(!n.iter().any(|(x, _, _)| x == "commented"));
        let f = extract("a.rs", src);
        assert_eq!(f.refs["helper"].0, 2);
        assert_eq!(f.refs["helper"].1, vec![2, 4]);
        assert!(!f.refs.contains_key("pub") && !f.refs.contains_key("fn"));
        assert!(!f.refs.contains_key("commented"));
    }

    #[test]
    fn other_languages() {
        let ts = names("a.ts", "export function load() {}\nexport class Store {}\ninterface Props {}\nexport type Id = string;\nconst handler = async (req) => {};\nexport const LIMIT = 5;\n");
        for w in ["load", "Store", "Props", "Id", "handler", "LIMIT"] {
            assert!(ts.iter().any(|(x, _, _)| x == w), "{w}: {ts:?}");
        }
        let py = names("a.py", "def run():\n    pass\nclass Job:\n    async def fetch(self): pass\nMAX_RETRIES = 3\n# def nope(): pass\n");
        for w in ["run", "Job", "fetch", "MAX_RETRIES"] {
            assert!(py.iter().any(|(x, _, _)| x == w), "{w}: {py:?}");
        }
        assert!(!py.iter().any(|(x, _, _)| x == "nope"));
        let go = names("a.go", "func Serve() {}\nfunc (s *Server) Handle() {}\ntype Server struct {}\ntype Handler interface {}\nconst Port = 1\n");
        for w in ["Serve", "Handle", "Server", "Handler", "Port"] {
            assert!(go.iter().any(|(x, _, _)| x == w), "{w}: {go:?}");
        }
        let java = names("A.java", "public class Account {\n  public static int balance(int x) {\n  private void reset() {\n}\n");
        for w in ["Account", "balance", "reset"] {
            assert!(java.iter().any(|(x, _, _)| x == w), "{w}: {java:?}");
        }
        let kt = names("a.kt", "data class User(val id: Int)\nfun greet() {}\nsuspend fun load() {}\nobject Registry\n");
        for w in ["User", "greet", "load", "Registry"] {
            assert!(kt.iter().any(|(x, _, _)| x == w), "{w}: {kt:?}");
        }
        let c = names("a.c", "struct node {\nint add(int a, int b) {\n#define MAX 4\nstatic void *alloc_page(size_t n)\nif (x) {\n");
        for w in ["node", "add", "MAX", "alloc_page"] {
            assert!(c.iter().any(|(x, _, _)| x == w), "{w}: {c:?}");
        }
        assert!(!c.iter().any(|(x, _, _)| x == "if"));
        let rb = names("a.rb", "class Cart\n  def total\n  def self.empty?\nmodule Billing\n");
        for w in ["Cart", "total", "empty?", "Billing"] {
            assert!(rb.iter().any(|(x, _, _)| x == w), "{w}: {rb:?}");
        }
        let php = names("a.php", "class Invoice {\n  public function send() {}\nfunction helper() {}\ntrait Loggable {}\n");
        for w in ["Invoice", "send", "helper", "Loggable"] {
            assert!(php.iter().any(|(x, _, _)| x == w), "{w}: {php:?}");
        }
        assert!(names("README.md", "fn not_code").is_empty());
        assert_eq!(strip_comment(r#"let s = "a // b"; // c"#, "//"), r#"let s = "a // b"; "#);
    }

    #[test]
    fn test_paths() {
        for p in ["tests/api.rs", "src/foo_test.go", "pkg/test_x.py", "web/a.test.ts", "web/a.spec.js", "src/test/java/AccountTest.java", "spec/cart_spec.rb", "crates/x/tests/y.rs"] {
            assert!(is_test_path(p), "{p}");
        }
        for p in ["src/api.rs", "src/testing.rs", "latest/x.py"] {
            assert!(!is_test_path(p), "{p}");
        }
    }
}
