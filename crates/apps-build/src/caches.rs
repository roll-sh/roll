//! BuildKit's cache mounts (`RUN --mount=type=cache,…`) belong to the builder and are keyed by
//! their `id`, which is the target path when none is given. On a node that builds many services,
//! two services' `/root/.npm` would be one directory, and one build could leave files there that
//! another reads. Every cache mount of a Dockerfile, the generated one or the tree's own, gets
//! `,id=<namespace>-<hash>` added at its end, where a later `id` wins over any the mount names:
//! the namespace is the service's secret, and the hash keeps its different mounts apart.
//!
//! The layer cache is shared the same way: a `RUN` step's cache key holds its command, its
//! parents and its environment, but not what its cache mounts held, so a build that filled its
//! own mount with bad files could hand the step's output to another service whose step matches.
//! Every stage therefore declares [`LAYERS_ARG`] (`ARG ROLL_LAYERS` after each `FROM`), and the
//! builder passes it a secret per service, which puts it in every `RUN` step's key.
//!
//! Only `RUN` instructions' flags are read, as BuildKit's Dockerfile parser reads them: quotes,
//! the escape character (`# escape=`), line continuations and the comment lines inside them; a
//! heredoc's body is skipped. A custom frontend (`# syntax=`) makes its own mounts and can name
//! any id but a namespaced one, which it can't know.

/// The build argument every stage declares, whose value keeps a service's `RUN` steps its own.
pub const LAYERS_ARG: &str = "ROLL_LAYERS";

/// `dockerfile` with every cache mount of its `RUN` instructions under `ns`, and every stage
/// declaring [`LAYERS_ARG`].
pub fn isolate_caches(dockerfile: &[u8], ns: &str) -> Vec<u8> {
    let escape = escape_char(dockerfile);
    let mut inserts: Vec<(usize, String)> = Vec::new();
    let d = dockerfile;
    let mut i = 0;
    while i < d.len() {
        // The start of a line: blank, a comment, or an instruction.
        i = skip_blanks(d, i);
        if i >= d.len() {
            break;
        }
        if d[i] == b'\n' {
            i += 1;
            continue;
        }
        if d[i] == b'#' {
            i = line_end(d, i);
            continue;
        }
        let word_end = (i..d.len()).find(|&j| !d[j].is_ascii_alphabetic()).unwrap_or(d.len());
        let word = &d[i..word_end];
        let run = word.eq_ignore_ascii_case(b"RUN");
        let from = word.eq_ignore_ascii_case(b"FROM");
        // Only these take heredocs.
        let heredocs = run || word.eq_ignore_ascii_case(b"COPY") || word.eq_ignore_ascii_case(b"ADD");
        let mut at = word_end;
        if run {
            // Its flags, each `--…` up to unquoted whitespace.
            loop {
                at = skip_space(d, at, escape);
                // A flag is a word that starts with `--` once its quotes are out.
                let (end, value) = token(d, at, escape);
                if !value.starts_with(b"--") {
                    break;
                }
                if is_cache_mount(&value) {
                    inserts.push((end, format!(",id={ns}-{:016x}", fnv(&value))));
                }
                at = end;
            }
        }
        i = instruction_end(d, at, escape, heredocs);
        if from {
            let nl = if i > 0 && d[i - 1] != b'\n' { "\n" } else { "" };
            inserts.push((i, format!("{nl}ARG {LAYERS_ARG}\n")));
        }
    }
    let mut out = Vec::with_capacity(d.len() + inserts.len() * 40);
    let mut last = 0;
    for (at, s) in inserts {
        out.extend_from_slice(&d[last..at]);
        out.extend_from_slice(s.as_bytes());
        last = at;
    }
    out.extend_from_slice(&d[last..]);
    out
}

/// The escape character a leading `# escape=` directive sets (`\` by default).
fn escape_char(d: &[u8]) -> u8 {
    for line in d.split(|&b| b == b'\n') {
        let l = String::from_utf8_lossy(line);
        let Some(rest) = l.trim().strip_prefix('#') else { break };
        let Some((k, v)) = rest.split_once('=') else { break };
        if k.trim().eq_ignore_ascii_case("escape") && v.trim() == "`" {
            return b'`';
        }
    }
    b'\\'
}

fn skip_blanks(d: &[u8], mut i: usize) -> usize {
    while i < d.len() && matches!(d[i], b' ' | b'\t' | b'\r') {
        i += 1;
    }
    i
}

fn line_end(d: &[u8], i: usize) -> usize {
    (i..d.len()).find(|&j| d[j] == b'\n').map_or(d.len(), |j| j + 1)
}

/// Whether `i` is a line continuation (the escape character, then only blanks to the newline):
/// where the next line starts.
fn continuation(d: &[u8], i: usize, escape: u8) -> Option<usize> {
    if d.get(i) != Some(&escape) {
        return None;
    }
    let j = skip_blanks(d, i + 1);
    match d.get(j) {
        Some(b'\n') => Some(skip_comment_lines(d, j + 1)),
        None => Some(j),
        _ => None,
    }
}

/// Inside a continued instruction, comment lines and empty lines are left out.
fn skip_comment_lines(d: &[u8], mut i: usize) -> usize {
    loop {
        let j = skip_blanks(d, i);
        match d.get(j) {
            Some(b'#') => i = line_end(d, j),
            Some(b'\n') => i = j + 1,
            _ => return i,
        }
    }
}

/// Past whitespace and line continuations.
fn skip_space(d: &[u8], mut i: usize, escape: u8) -> usize {
    loop {
        i = skip_blanks(d, i);
        match continuation(d, i, escape) {
            Some(j) => i = j,
            None => return i,
        }
    }
}

/// One flag from `i`: where it ends, and its value with the quotes and escapes taken out.
fn token(d: &[u8], mut i: usize, escape: u8) -> (usize, Vec<u8>) {
    let mut value = Vec::new();
    let mut quote: Option<u8> = None;
    while i < d.len() {
        let c = d[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(b'"') if c == escape && i + 1 < d.len() && d[i + 1] != b'\n' => {
                i += 1;
                value.push(d[i]);
            }
            Some(_) if c == b'\n' => break,
            Some(_) => value.push(c),
            None => {
                if let Some(j) = continuation(d, i, escape) {
                    // A continuation inside a flag joins the next line to it.
                    if matches!(d.get(j), Some(b' ' | b'\t') | None) {
                        break;
                    }
                    i = j;
                    continue;
                }
                match c {
                    b' ' | b'\t' | b'\r' | b'\n' => break,
                    b'"' | b'\'' => quote = Some(c),
                    _ if c == escape && i + 1 < d.len() => {
                        i += 1;
                        value.push(d[i]);
                    }
                    _ => value.push(c),
                }
            }
        }
        i += 1;
    }
    (i, value)
}

fn is_cache_mount(flag: &[u8]) -> bool {
    let Some(v) = flag.strip_prefix(b"--mount=") else { return false };
    v.split(|&b| b == b',').any(|f| {
        let f = String::from_utf8_lossy(f);
        f.split_once('=').is_some_and(|(k, v)| k.trim().eq_ignore_ascii_case("type") && v.trim().eq_ignore_ascii_case("cache"))
    })
}

/// Where the instruction from `i` ends: its continuations, and (`heredocs`: `RUN`, `COPY`, `ADD`)
/// the body of each heredoc it opens after it. A heredoc is a word of its own, outside quotes,
/// that starts `<<` (`<<EOF`, `<<-"EOF"`), as BuildKit reads them; one whose end word never comes
/// skips nothing (BuildKit refuses the file anyway).
fn instruction_end(d: &[u8], mut i: usize, escape: u8, heredocs: bool) -> usize {
    let start = i;
    while i < d.len() {
        if let Some(j) = continuation(d, i, escape) {
            i = j;
            continue;
        }
        if d[i] == b'\n' {
            i += 1;
            break;
        }
        i += 1;
    }
    if !heredocs {
        return i;
    }
    // Its words, outside quotes.
    let mut words: Vec<(String, bool)> = Vec::new();
    let mut at = start;
    while at < i {
        at = skip_space(d, at, escape);
        if at >= i || d[at] == b'\n' {
            break;
        }
        let first = d[at];
        let (end, value) = token(d, at, escape);
        if first == b'<' && value.starts_with(b"<<") {
            let mut w = &value[2..];
            let dash = w.first() == Some(&b'-');
            if dash {
                w = &w[1..];
            }
            if !w.is_empty() && w.iter().all(|b| b.is_ascii_graphic() && *b != b'<') {
                words.push((String::from_utf8_lossy(w).into_owned(), dash));
            }
        }
        at = end.max(at + 1);
    }
    for (word, dash) in words {
        let mut j = i;
        let mut ended = None;
        while j < d.len() {
            let end = line_end(d, j);
            let line = String::from_utf8_lossy(&d[j..end]);
            let line = line.trim_end_matches(['\n', '\r']);
            j = end;
            if (if dash { line.trim_start_matches('\t') } else { line }) == word {
                ended = Some(j);
                break;
            }
        }
        match ended {
            Some(e) => i = e,
            None => break,
        }
    }
    i
}

/// FNV-1a: tells a service's mounts apart (not a secret; the namespace is).
fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &c| (h ^ c as u64).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso(df: &str) -> String {
        String::from_utf8(isolate_caches(df.as_bytes(), "NS")).unwrap()
    }

    fn count(df: &str) -> usize {
        iso(df).matches(",id=NS-").count()
    }

    #[test]
    fn every_cache_mount_of_a_run_gets_the_namespace_last() {
        let df = "FROM node:22\nRUN --mount=type=cache,target=/root/.npm npm ci\n";
        let out = String::from_utf8(isolate_caches(df.as_bytes(), "NS")).unwrap();
        assert!(out.starts_with("FROM node:22\nARG ROLL_LAYERS\nRUN --mount=type=cache,target=/root/.npm,id=NS-"), "{out}");
        assert_eq!(iso("FROM a AS b\nRUN x\nFROM \\\n  c\nRUN y"), "FROM a AS b\nARG ROLL_LAYERS\nRUN x\nFROM \\\n  c\nARG ROLL_LAYERS\nRUN y", "every stage");
        assert!(out.ends_with(" npm ci\n"), "{out}");
        // Quoted values, a quoted flag, an id of its own, a second flag, other mounts untouched.
        let out = iso("RUN --mount=type=cache,target=\"/a b\" --mount=type=secret,id=T \"--mount=type=cache,dst=/x\" --mount=type=cache,id=mine,target=/y make\n");
        assert_eq!(out.matches(",id=NS-").count(), 3, "{out}");
        for want in ["target=\"/a b\",id=NS-", "\"--mount=type=cache,dst=/x\",id=NS-", "id=mine,target=/y,id=NS-", "--mount=type=secret,id=T \"", " make\n"] {
            assert!(out.contains(want), "{want}: {out}");
        }
        // Tricks that are still cache mounts to BuildKit.
        assert_eq!(count("RUN --mount=type=ca\"che\",i\\d=shared,target=/x true\n"), 1);
        assert_eq!(count("RUN --mount=type=cache,\\\nid=shared,target=/x true\n"), 1, "a continuation inside the flag");
        assert_eq!(count("RUN \\\n  # a comment\n  --mount=type=cache,target=/x true\n"), 1, "a flag after a continuation and a comment");
        assert_eq!(count("# escape=`\nRUN `\n  --mount=type=cache,target=/x true\n"), 1);
        assert_eq!(count("run --mount=TYPE=Cache,target=/x true\n"), 1);
        // Words that only mention one: a command, a heredoc's body, another instruction.
        assert!(count("RUN echo '--mount=type=cache,target=/y'\n") == 0);
        assert!(count("RUN <<EOF\nRUN --mount=type=cache,target=/z x\nEOF\nCOPY --mount=type=cache,target=/w . .\n") == 0);
        assert_eq!(count("RUN <<-EOT\n\tx\n\tEOT\nRUN --mount=type=cache,target=/after true\n"), 1, "after a heredoc, the next instruction");
        // `<<` that isn't a heredoc: a shift, inside quotes, another instruction; an end word with `-`.
        for not_one in ["RUN echo $((1<<20))\n", "RUN python -c \"print(1<<3)\"\n", "LABEL x=\"a<<b\"\n", "RUN cat <<EOF\n"] {
            assert_eq!(count(&format!("{not_one}RUN --mount=type=cache,target=/next true\n")), 1, "{not_one}");
        }
        assert_eq!(count("RUN <<END-OF\nRUN --mount=type=cache,target=/in x\nEND-OF\nRUN --mount=type=cache,target=/out y\n"), 1, "an end word with a dash");
        // Bytes that aren't UTF-8 stay as they are.
        let latin1 = b"# caf\xe9\nRUN --mount=type=cache,target=/x echo \xe9\n";
        let out = isolate_caches(latin1, "NS");
        assert!(out.starts_with(b"# caf\xe9\nRUN --mount=type=cache,target=/x,id=NS-") && out.ends_with(b" echo \xe9\n"));
        // The same mount gets the same id each build; another mount another.
        assert_eq!(isolate_caches(df.as_bytes(), "NS"), isolate_caches(df.as_bytes(), "NS"));
        let two = iso("RUN --mount=type=cache,target=/a --mount=type=cache,target=/b x\n");
        let hashes: Vec<&str> = two.split(",id=NS-").skip(1).map(|r| &r[..16]).collect();
        assert!(hashes.len() == 2 && hashes[0] != hashes[1], "{two}");
    }
}
