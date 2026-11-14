//! The `patterns` command's `brain` method: the plugin's `BrainLogParser` (an implementation
//! of the Brain log-parsing algorithm), reproduced step for step so the same logs give the
//! same patterns.
//!
//! Each message is preprocessed (IPs, dates, UUIDs and number-like words replaced by
//! variable markers, `,` and `+` turned into spaces, split on whitespace). A histogram counts
//! every `(position, token)`; each message's representative frequency is the most common
//! frequency among its tokens (above 30% of its highest). Within a group (same length and
//! representative), a token is a variable when it is more frequent than the representative
//! but not the only token at its position, or less frequent and its position holds at least
//! `variable_count_threshold` different tokens.
//!
//! The rule patterns use Java regex lookarounds, which the `regex` crate lacks: the one that
//! needs them (a number between non-alphanumerics) is matched by hand, and the negative
//! lookahead of the number rule is implied by the rest of its alternative.

use std::collections::{HashMap, HashSet};

const VAR: &str = "<*>";

struct Rules {
    ip: regex::Regex,
    datetime: regex::Regex,
    uuid: regex::Regex,
    number: regex::Regex,
}

fn rules() -> &'static Rules {
    static R: std::sync::OnceLock<Rules> = std::sync::OnceLock::new();
    R.get_or_init(|| Rules {
        ip: regex::Regex::new(r"(/|)([0-9]+\.){3}[0-9]+(:[0-9]+|)(:|)").expect("ip"),
        datetime: regex::Regex::new(r"(\d{4}-\d{2}-\d{2})[T ]?(\d{2}:\d{2}:\d{2})(\.\d{3})?(Z|([+-]\d{2}:?\d{2}))?").expect("datetime"),
        uuid: regex::Regex::new(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b").expect("uuid"),
        // `(?!\d{3}$)` before `\d{4,}` can never fail, so it is left out.
        number: regex::Regex::new(r"((0x|0X)[0-9a-fA-F]+)|[a-zA-Z]+\d+|([+-]?\d{4,}(\.\d*)?|\.\d+)").expect("number"),
    })
}

/// `(?<=[^A-Za-z0-9 ])(-?\+?\d+)(?=[^A-Za-z0-9])` replaced by `<*>`, left to right.
fn replace_bounded_numbers(s: &str) -> String {
    let b = s.as_bytes();
    let alnum = |c: u8| c.is_ascii_alphanumeric();
    let mut out = String::with_capacity(s.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        // A match starts at i if the character before it is neither alphanumeric nor a space.
        let starts = i > 0 && !alnum(b[i - 1]) && b[i - 1] != b' ';
        if starts {
            let mut j = i;
            if j < b.len() && b[j] == b'-' {
                j += 1;
            }
            if j < b.len() && b[j] == b'+' {
                j += 1;
            }
            let digits = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits && j < b.len() && !alnum(b[j]) {
                out.push_str(&s[copied..i]);
                out.push_str(VAR);
                copied = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[copied..]);
    out
}

fn preprocess(msg: &str) -> Vec<String> {
    let r = rules();
    let s = r.ip.replace_all(msg, "<*IP*>");
    let s = r.datetime.replace_all(&s, "<*DATETIME*>");
    let s = r.uuid.replace_all(&s, "<*UUID*>");
    let s = r.number.replace_all(&s, VAR);
    let s = replace_bounded_numbers(&s).replace([',', '+'], " ");
    let t = s.trim();
    if t.is_empty() {
        return vec![String::new()];
    }
    t.split(|c: char| c.is_whitespace()).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

/// `<*><*>...` runs inside a token collapse to one `<*>`.
fn collapse(part: &str) -> String {
    if part.len() < 6 {
        return part.to_string();
    }
    let mut out = String::with_capacity(part.len());
    let mut rest = part;
    while let Some(j) = rest.find(VAR) {
        out.push_str(&rest[..j]);
        out.push_str(VAR);
        rest = &rest[j + VAR.len()..];
        while let Some(r) = rest.strip_prefix(VAR) {
            rest = r;
        }
    }
    out.push_str(rest);
    out
}

/// The pattern of every message (`None` messages give an empty pattern).
pub fn parse_all(messages: &[Option<String>], variable_count_threshold: usize, threshold: f64) -> Vec<String> {
    let logs: Vec<Vec<String>> = messages.iter().map(|m| m.as_deref().map_or_else(|| vec![String::new()], preprocess)).collect();
    let mut freq: HashMap<(usize, &str), u64> = HashMap::new();
    for tokens in &logs {
        for (i, t) in tokens.iter().enumerate() {
            *freq.entry((i, t.as_str())).or_insert(0) += 1;
        }
    }
    // Each message's group candidate: (representative frequency, how many tokens have it).
    let mut candidates: Vec<(u64, usize)> = Vec::with_capacity(logs.len());
    // The tokens seen at each (message length, group candidate, position).
    type GroupKey = (usize, (u64, usize), usize);
    let mut group_tokens: HashMap<GroupKey, HashSet<&str>> = HashMap::new();
    for tokens in &logs {
        let mut occ: HashMap<u64, usize> = HashMap::new();
        for (i, t) in tokens.iter().enumerate() {
            *occ.entry(freq[&(i, t.as_str())]).or_insert(0) += 1;
        }
        let mut combos: Vec<(u64, usize)> = occ.into_iter().collect();
        // By same-frequency count, then frequency, both descending.
        combos.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
        let max = combos.iter().map(|c| c.0).max().unwrap_or(0);
        let limit = max as f32 * threshold as f32;
        let cand = combos.iter().find(|c| c.0 as f32 > limit).or(combos.first()).copied().unwrap_or((0, 0));
        candidates.push(cand);
        for (i, t) in tokens.iter().enumerate() {
            group_tokens.entry((tokens.len(), cand, i)).or_default().insert(t.as_str());
        }
    }
    logs.iter()
        .zip(&candidates)
        .map(|(tokens, cand)| {
            let parts: Vec<String> = tokens
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let f = freq[&(i, t.as_str())];
                    let set = &group_tokens[&(tokens.len(), *cand, i)];
                    let variable = (f > cand.0 && set.len() != 1) || (f < cand.0 && set.len() >= variable_count_threshold);
                    if variable { VAR.to_string() } else { collapse(t) }
                })
                .collect();
            parts.join(" ")
        })
        .collect()
}

/// A pattern's wildcards (`<*>`, `<*IP*>`, ...) numbered `<token1>`, `<token2>`, ..., and the
/// values each takes in `original` (`PatternUtils.parsePattern` / `extractVariables`).
pub fn number_tokens(pattern: &str, original: Option<&str>) -> (String, Vec<(String, String)>) {
    static WILD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let wild = WILD.get_or_init(|| regex::Regex::new(r"<\*[^>]*>").expect("wildcard"));
    let mut parts: Vec<(String, bool)> = Vec::new();
    let mut last = 0;
    for m in wild.find_iter(pattern) {
        if m.start() > last {
            parts.push((pattern[last..m.start()].to_string(), false));
        }
        parts.push((m.as_str().to_string(), true));
        last = m.end();
    }
    if last < pattern.len() {
        parts.push((pattern[last..].to_string(), false));
    }
    let mut out = String::new();
    let mut names = Vec::new();
    for (p, tok) in &parts {
        if *tok {
            let n = format!("<token{}>", names.len() + 1);
            out.push_str(&n);
            names.push(n);
        } else {
            out.push_str(p);
        }
    }
    let mut values = Vec::new();
    if let Some(orig) = original {
        let (mut pos, mut k) = (0usize, 0usize);
        let mut i = 0;
        while i < parts.len() {
            let (p, tok) = &parts[i];
            if *tok {
                let name = names[k].clone();
                k += 1;
                if i == parts.len() - 1 {
                    values.push((name, orig[pos..].to_string()));
                    break;
                }
                let next = &parts[i + 1].0;
                let Some(idx) = orig[pos..].find(next.as_str()).map(|x| x + pos) else { break };
                values.push((name, orig[pos..idx].to_string()));
                pos = idx;
            } else if orig[pos..].starts_with(p.as_str()) {
                pos += p.len();
            } else {
                break;
            }
            i += 1;
        }
    }
    (out, values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocessing_masks_variables() {
        assert_eq!(preprocess("blk_-7017553867379051457 size 67108864"), vec!["blk_<*>", "size", "<*>"]);
        assert_eq!(preprocess("from 10.0.0.1:50010 to x"), vec!["from", "<*IP*>", "to", "x"]);
        assert_eq!(replace_bounded_numbers("a_-12/x 3"), "a_<*>/x 3");
    }

    #[test]
    fn variables_are_numbered_and_extracted() {
        let (p, v) = number_tokens("added to blk_<*> size <*>", Some("added to blk_-70 size 6710"));
        assert_eq!(p, "added to blk_<token1> size <token2>");
        assert_eq!(v, vec![("<token1>".to_string(), "-70".to_string()), ("<token2>".to_string(), "6710".to_string())]);
    }

    #[test]
    fn brain_groups_logs_like_the_plugin() {
        // The SQL plugin's `issues/3570` HDFS sample: its expected brain patterns.
        let logs: Vec<Option<String>> = [
            "BLOCK* NameSystem.addStoredBlock: blockMap updated: 10.251.31.85:50010 is added to blk_-7017553867379051457 size 67108864",
            "BLOCK* NameSystem.allocateBlock: /user/root/sortrand/_temporary/_task_200811092030_0002_r_000296_0/part-00296. blk_-6620182933895093708",
            "BLOCK* NameSystem.addStoredBlock: blockMap updated: 10.250.7.244:50010 is added to blk_-3249711809227781266 size 67108864",
            "Verification succeeded for blk_-1547954353065580372",
            "BLOCK* NameSystem.allocateBlock: /user/root/sortrand/_temporary/_task_200811092030_0002_r_000318_0/part-00318. blk_2096692261399680562",
            "Verification succeeded for blk_6996194389878584395",
            "PacketResponder failed for blk_6996194389878584395",
            "PacketResponder failed for blk_-1547954353065580372",
        ]
        .iter()
        .map(|s| Some(s.to_string()))
        .collect();
        let p = parse_all(&logs, 5, 0.3);
        assert_eq!(p[0], "BLOCK* NameSystem.addStoredBlock: blockMap updated: <*IP*> is added to blk_<*> size <*>");
        assert_eq!(p[3], "Verification succeeded <*> blk_<*>");
        assert_eq!(p[3], p[5]);
        assert_eq!(p[6], "PacketResponder failed <*> blk_<*>");
        assert_eq!(number_tokens(&p[1], None).0, "<token1> NameSystem.allocateBlock: /user/root/sortrand/_temporary/_task_<token2>_<token3>_r_<token4>_<token5>/part<token6> blk_<token7>");
    }
}
