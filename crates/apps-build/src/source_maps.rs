//! Readable Node stack traces: a Node build whose server code is compiled or bundled with source
//! maps (`esbuild --sourcemap`, `tsup --sourcemap`, `rollup --sourcemap`, `swc --source-maps`, `tsc`
//! with `sourceMap` in its tsconfig) runs with `--enable-source-maps` joined into `NODE_OPTIONS` at
//! start (`Plan::source_maps`; apps-control joins it, as it does the OpenTelemetry agent's), so
//! a stack, and so an error group's fingerprint and frames, names the source's files and lines
//! rather than the bundle's. Without maps it stays off: Node pays for it on every stack it formats.

use crate::Tree;
use serde_json::Value;

/// Why the build emits source maps, if it does.
pub fn emitted(tree: &dyn Tree, pkg: &Value) -> Option<String> {
    let scripts = &pkg["scripts"];
    // The build script, and the ones it runs by name (`npm run build:server`).
    let mut texts: Vec<&str> = scripts["build"].as_str().into_iter().collect();
    if let Some(b) = scripts["build"].as_str() {
        for word in b.split(|c: char| c.is_whitespace() || c == '&' || c == ';') {
            if let Some(s) = scripts[word].as_str() {
                texts.push(s);
            }
        }
    }
    for t in &texts {
        let words: Vec<&str> = t.split_whitespace().collect();
        if words.iter().any(|w| ["--sourcemap", "--source-map", "--source-maps", "--sourcemaps"].contains(w) || w.starts_with("--sourcemap=") || w.starts_with("--source-maps=")) {
            return Some(format!("its build ({t}) writes source maps"));
        }
        if words.contains(&"tsc") || words.iter().any(|w| w.ends_with("/tsc")) {
            let config = words.windows(2).find(|w| w[0] == "-p" || w[0] == "--project").map_or("tsconfig.json", |w| w[1]);
            let config = if config.ends_with(".json") { config.to_string() } else { format!("{}/tsconfig.json", config.trim_end_matches('/')) };
            if tsconfig_source_map(tree, &config) {
                return Some(format!("{config} sets sourceMap"));
            }
        }
    }
    None
}

/// `compilerOptions.sourceMap` in a tsconfig (comments and trailing commas allowed, as tsc's
/// own reader allows them; `extends` not followed).
fn tsconfig_source_map(tree: &dyn Tree, path: &str) -> bool {
    let Some(text) = tree.read(path).and_then(|b| String::from_utf8(b).ok()) else { return false };
    let mut out = String::with_capacity(text.len());
    let (mut chars, mut in_str) = (text.chars().peekable(), false);
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_str = !in_str;
                out.push(c);
            }
            '\\' if in_str => {
                out.push(c);
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '/' if !in_str && chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if !in_str && chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for n in chars.by_ref() {
                    if last == '*' && n == '/' {
                        break;
                    }
                    last = n;
                }
            }
            _ => out.push(c),
        }
    }
    let cleaned = out.replace(",}", "}").replace(",\n}", "\n}").replace(",]", "]");
    let cleaned = regex_lite_trailing_commas(&cleaned);
    serde_json::from_str::<Value>(&cleaned).is_ok_and(|v| v["compilerOptions"]["sourceMap"] == true || v["compilerOptions"]["inlineSourceMap"] == true)
}

/// A trailing comma before a closing brace or bracket, whatever space is between.
fn regex_lite_trailing_commas(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        if c == ',' {
            let next = b[i + 1..].iter().find(|x| !x.is_ascii_whitespace());
            if matches!(next, Some(b'}') | Some(b']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::tests::tree as files;
    use serde_json::json;

    #[test]
    fn a_build_with_source_maps_says_so() {
        let none = files(&[]);
        assert!(super::emitted(&none, &json!({"scripts": {"build": "esbuild src/index.ts --bundle --sourcemap --outfile=dist/index.js"}})).is_some());
        assert!(super::emitted(&none, &json!({"scripts": {"build": "npm run build:server", "build:server": "tsup src --sourcemap"}})).is_some());
        assert!(super::emitted(&none, &json!({"scripts": {"build": "esbuild src/index.ts --bundle"}})).is_none());
        let ts = files(&[("tsconfig.json", "{\n  // the server\n  \"compilerOptions\": { \"outDir\": \"dist\", \"sourceMap\": true, },\n}\n")]);
        assert!(super::emitted(&ts, &json!({"scripts": {"build": "tsc"}})).is_some_and(|w| w.contains("tsconfig.json")));
        assert!(super::emitted(&ts, &json!({"scripts": {"build": "vite build"}})).is_none(), "tsc's tsconfig only for tsc");
        let off = files(&[("tsconfig.json", "{\"compilerOptions\": {\"outDir\": \"dist\"}}")]);
        assert!(super::emitted(&off, &json!({"scripts": {"build": "tsc"}})).is_none());
    }
}
