//! READMEs (GitHub-flavoured markdown, sanitized), languages by extension, content types.

use comrak::nodes::NodeValue;

/// Where relative links and images in a README point.
pub struct LinkBase<'a> {
    /// `{web}/{owner}/{repo}`: links become `…/blob/{rev}/{path}`.
    pub web: &'a str,
    /// `{api}/v1/code/repos/{owner}/{repo}`: images become `…/blob/{rev}/{path}` (raw bytes).
    pub raw: &'a str,
    pub rev: &'a str,
    /// The README's directory ("" at the root).
    pub dir: &'a str,
}

/// Render markdown to sanitized HTML. Relative links and images are rewritten to absolute URLs
/// in the repo; anything ammonia doesn't allow (scripts, event handlers, `javascript:` URLs,
/// styles) is dropped.
pub fn markdown(md: &str, base: &LinkBase) -> String {
    let mut opts = comrak::Options::default();
    opts.extension.strikethrough = true;
    opts.extension.table = true;
    opts.extension.autolink = true;
    opts.extension.tasklist = true;
    opts.extension.footnotes = true;
    opts.extension.header_id_prefix = Some("user-content-".into());
    // Raw HTML is common in READMEs (centered logos, badges); ammonia sanitizes it below.
    opts.render.r#unsafe = true;
    let arena = comrak::Arena::new();
    let root = comrak::parse_document(&arena, md, &opts);
    for node in root.descendants() {
        let mut data = node.data_mut();
        match &mut data.value {
            NodeValue::Link(l) => l.url = rewrite(&l.url, base, false),
            NodeValue::Image(l) => l.url = rewrite(&l.url, base, true),
            _ => {}
        }
    }
    let mut html = String::new();
    if comrak::format_html(root, &opts, &mut html).is_err() {
        return String::new();
    }
    // Raw HTML (`<img src="docs/logo.png">`, `<a href="CONTRIBUTING.md">`) gets the same
    // rewriting as markdown links. Already-absolute URLs (everything comrak rewrote) pass through.
    let (web, raw, rev, dir) = (base.web.to_string(), base.raw.to_string(), base.rev.to_string(), base.dir.to_string());
    let relative = move |element: &str, attribute: &str, value: &str| -> Option<String> {
        let b = LinkBase { web: &web, raw: &raw, rev: &rev, dir: &dir };
        match (element, attribute) {
            ("a", "href") => Some(rewrite(value, &b, false)),
            ("img" | "source", "src") => Some(rewrite(value, &b, true)),
            ("img" | "source", "srcset") => Some(
                value
                    .split(',')
                    .map(|c| match c.trim().split_once(char::is_whitespace) {
                        Some((u, descriptor)) => format!("{} {}", rewrite(u, &b, true), descriptor.trim()),
                        None => rewrite(c.trim(), &b, true),
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            _ => None,
        }
    };
    ammonia::Builder::default()
        .attribute_filter(move |element, attribute, value| match relative(element, attribute, value) {
            Some(v) => Some(v.into()),
            None => Some(value.into()),
        })
        .add_tags(["details", "summary", "picture", "source", "input"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_tag_attributes("source", ["srcset", "media", "type"])
        .add_tag_attributes("img", ["width", "height", "align"])
        .add_tag_attributes("p", ["align"])
        .add_tag_attributes("div", ["align"])
        .add_tag_attributes("td", ["align"])
        .add_tag_attributes("th", ["align"])
        .add_tag_attributes("code", ["class"])
        .add_tag_attributes("pre", ["lang"])
        .add_tag_attributes("a", ["id", "aria-hidden"])
        .add_generic_attributes(["id"])
        .link_rel(Some("noopener noreferrer nofollow"))
        .clean(&html)
        .to_string()
}

fn rewrite(url: &str, base: &LinkBase, image: bool) -> String {
    let u = url.trim();
    let absolute = u.is_empty()
        || u.starts_with('#')
        || u.starts_with("//")
        || u.split_once(':').is_some_and(|(scheme, _)| !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)));
    if absolute {
        return url.to_string();
    }
    let (path, fragment) = match u.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (u, None),
    };
    let path = path.split('?').next().unwrap_or("");
    let joined = if let Some(abs) = path.strip_prefix('/') { abs.to_string() } else if base.dir.is_empty() { path.to_string() } else { format!("{}/{path}", base.dir) };
    let Some(norm) = normalize(&joined) else { return "#".into() };
    let rev = encode(base.rev);
    let p = norm.split('/').map(encode).collect::<Vec<_>>().join("/");
    let mut out = if image {
        format!("{}/blob/{rev}/{p}", base.raw)
    } else if norm.is_empty() {
        format!("{}/tree/{rev}", base.web)
    } else {
        format!("{}/blob/{rev}/{p}", base.web)
    };
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// Resolve `.` and `..`; `None` when it climbs out of the repo.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    Some(parts.join("/"))
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Pick the README to show from a directory's file names.
pub fn pick_readme<'a>(names: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    const ORDER: &[&str] = &["readme.md", "readme.markdown", "readme.mdown", "readme", "readme.txt", "readme.rst"];
    names.filter_map(|n| ORDER.iter().position(|o| n.eq_ignore_ascii_case(o)).map(|i| (i, n))).min_by_key(|(i, _)| *i).map(|(_, n)| n)
}

/// Is this README rendered as markdown (else shown as text)?
pub fn is_markdown(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".md") || n.ends_with(".markdown") || n.ends_with(".mdown")
}

/// The language of a file, by name.
pub fn language(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "dockerfile" | "containerfile" => return Some("Dockerfile"),
        "makefile" | "gnumakefile" => return Some("Makefile"),
        _ => {}
    }
    // Generated and vendored files don't count.
    if lower.ends_with(".min.js") || lower.ends_with(".lock") || lower == "package-lock.json" || lower == "pnpm-lock.yaml" {
        return None;
    }
    let ext = lower.rsplit_once('.')?.1;
    Some(match ext {
        "rs" => "Rust",
        "ts" | "mts" | "cts" => "TypeScript",
        "tsx" => "TSX",
        "js" | "mjs" | "cjs" => "JavaScript",
        "jsx" => "JSX",
        "py" => "Python",
        "go" => "Go",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "scala" => "Scala",
        "swift" => "Swift",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "C++",
        "cs" => "C#",
        "rb" => "Ruby",
        "php" => "PHP",
        "ex" | "exs" => "Elixir",
        "erl" => "Erlang",
        "hs" => "Haskell",
        "ml" | "mli" => "OCaml",
        "clj" | "cljs" => "Clojure",
        "dart" => "Dart",
        "lua" => "Lua",
        "zig" => "Zig",
        "nim" => "Nim",
        "sh" | "bash" | "zsh" => "Shell",
        "sql" => "SQL",
        "html" | "htm" => "HTML",
        "css" => "CSS",
        "scss" | "sass" => "SCSS",
        "vue" => "Vue",
        "svelte" => "Svelte",
        "md" | "mdx" => "Markdown",
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        "json" => "JSON",
        "proto" => "Protocol Buffers",
        "tf" => "HCL",
        "nix" => "Nix",
        "sol" => "Solidity",
        _ => return None,
    })
}

/// Content type for a raw file. Text is served as `text/plain` (never HTML), so the browser
/// never runs anything from a repo; the response also carries a sandboxing CSP.
pub fn content_type(path: &str, data: &[u8]) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "woff2" => "font/woff2",
        _ if code_store::git::is_binary(data) => "application/octet-stream",
        _ => "text/plain; charset=utf-8",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> LinkBase<'static> {
        LinkBase { web: "https://roll.sh/o/r", raw: "https://api/v1/code/repos/o/r", rev: "main", dir: "" }
    }

    #[test]
    fn sanitizes_and_rewrites() {
        let html = markdown(
            "# Hi\n\n<script>alert(1)</script>\n\n[docs](docs/a.md#x) [ext](https://x.dev) [up](../../etc)\n\n![logo](./img/logo.png)\n\n<a href=\"javascript:alert(1)\" onclick=\"x()\">bad</a>\n\n- [x] done",
            &base(),
        );
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("javascript:"), "{html}");
        assert!(!html.contains("onclick"), "{html}");
        assert!(html.contains("href=\"https://roll.sh/o/r/blob/main/docs/a.md#x\""), "{html}");
        assert!(html.contains("href=\"https://x.dev\""), "{html}");
        assert!(html.contains("src=\"https://api/v1/code/repos/o/r/blob/main/img/logo.png\""), "{html}");
        assert!(html.contains("id=\"user-content-hi\""), "{html}");
        assert!(html.contains("type=\"checkbox\""), "{html}");
    }

    #[test]
    fn relative_urls_resolve_from_the_readme_directory() {
        let b = LinkBase { web: "https://roll.sh/o/r", raw: "https://api/v1/code/repos/o/r", rev: "main", dir: "docs/guide" };
        let html = markdown(
            "[a](a.md) [up](../intro.md#top) [root](/LICENSE) [q](b.md?plain=1) [dir](../../) [out](../../../x) [ftp](ftp://h/f) [anchor](#here) [proto](//cdn/x.js)\n\n![shot](../img/s.png)",
            &b,
        );
        assert!(html.contains("href=\"https://roll.sh/o/r/blob/main/docs/guide/a.md\""), "{html}");
        assert!(html.contains("href=\"https://roll.sh/o/r/blob/main/docs/intro.md#top\""), "{html}");
        assert!(html.contains("href=\"https://roll.sh/o/r/blob/main/LICENSE\""), "{html}");
        assert!(html.contains("href=\"https://roll.sh/o/r/blob/main/docs/guide/b.md\""), "{html}");
        assert!(html.contains("href=\"https://roll.sh/o/r/tree/main\""), "the repo root is a tree: {html}");
        assert!(html.contains("href=\"#\""), "climbing out of the repo goes nowhere: {html}");
        assert!(html.contains("href=\"ftp://h/f\""), "{html}");
        assert!(html.contains("href=\"#here\""), "{html}");
        assert!(html.contains("href=\"//cdn/x.js\""), "{html}");
        assert!(html.contains("src=\"https://api/v1/code/repos/o/r/blob/main/docs/img/s.png\""), "{html}");
    }

    #[test]
    fn raw_html_links_and_images_are_rewritten() {
        let b = LinkBase { web: "https://roll.sh/o/r", raw: "https://api/v1/code/repos/o/r", rev: "release/1.0", dir: "" };
        let html = markdown(
            "<p align=\"center\"><img src=\"assets/logo.png\" width=\"80\"><a href=\"CONTRIBUTING.md\">contribute</a>\
             <picture><source srcset=\"a/dark.png 1x, a/dark@2x.png 2x\" media=\"(prefers-color-scheme: dark)\"><img src=\"https://x.dev/l.png\"></picture></p>",
            &b,
        );
        assert!(html.contains("src=\"https://api/v1/code/repos/o/r/blob/release%2F1.0/assets/logo.png\""), "{html}");
        assert!(html.contains("href=\"https://roll.sh/o/r/blob/release%2F1.0/CONTRIBUTING.md\""), "{html}");
        assert!(
            html.contains("srcset=\"https://api/v1/code/repos/o/r/blob/release%2F1.0/a/dark.png 1x, https://api/v1/code/repos/o/r/blob/release%2F1.0/a/dark%402x.png 2x\""),
            "{html}"
        );
        assert!(html.contains("src=\"https://x.dev/l.png\""), "{html}");
        assert!(html.contains("width=\"80\""), "{html}");
    }

    #[test]
    fn languages_by_name() {
        for (path, lang) in [
            ("src/main.rs", Some("Rust")),
            ("a/b.TS", Some("TypeScript")),
            ("ui/x.tsx", Some("TSX")),
            ("Dockerfile", Some("Dockerfile")),
            ("sub/Makefile", Some("Makefile")),
            ("app.min.js", None),
            ("Cargo.lock", None),
            ("pnpm-lock.yaml", None),
            ("package-lock.json", None),
            ("LICENSE", None),
            ("x.unknownext", None),
            ("README.md", Some("Markdown")),
        ] {
            assert_eq!(language(path), lang, "{path}");
        }
    }

    #[test]
    fn content_types_never_serve_html() {
        assert_eq!(content_type("x.html", b"<script>1</script>"), "text/plain; charset=utf-8");
        assert_eq!(content_type("logo.PNG", b"\x89PNG"), "image/png");
        assert_eq!(content_type("a.svg", b"<svg/>"), "image/svg+xml");
        assert_eq!(content_type("blob.bin", b"\0\x01\x02"), "application/octet-stream");
        assert_eq!(content_type("notes", b"plain"), "text/plain; charset=utf-8");
    }

    #[test]
    fn readme_kinds() {
        assert!(is_markdown("README.md") && is_markdown("x.MARKDOWN") && is_markdown("a.mdown"));
        assert!(!is_markdown("README") && !is_markdown("README.txt"));
        assert_eq!(pick_readme(["README.rst", "README.txt"].into_iter()), Some("README.txt"));
    }

    #[test]
    fn readme_choice() {
        assert_eq!(pick_readme(["a.rs", "README", "readme.md"].into_iter()), Some("readme.md"));
        assert_eq!(pick_readme(["a.rs"].into_iter()), None);
    }
}
