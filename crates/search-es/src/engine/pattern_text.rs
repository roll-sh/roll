//! `pattern_text` templates (`PatternTextValueProcessor`): a message splits on whitespace and
//! square brackets into tokens; tokens holding a digit are its arguments. The template is the
//! message without them (the delimiters stay), and its id is the first 64 bits of the template's
//! MurmurHash3 (x64, 128-bit, seed 0), little-endian, in unpadded URL-safe base64.

use base64::Engine;

fn is_delimiter(c: char) -> bool {
    // Java's `\s` (`[ \t\n\x0B\f\r]`) and `[`, `]`.
    matches!(c, ' ' | '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | '[' | ']')
}

fn is_arg(token: &str) -> bool {
    token.chars().any(|c| c.is_ascii_digit() || (!c.is_ascii() && c.is_numeric()))
}

/// The message without its arguments.
pub fn template(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut start = 0;
    for (i, c) in message.char_indices() {
        if is_delimiter(c) {
            let token = &message[start..i];
            if !is_arg(token) {
                out.push_str(token);
            }
            out.push(c);
            start = i + c.len_utf8();
        }
    }
    let token = &message[start..];
    if !is_arg(token) {
        out.push_str(token);
    }
    out
}

/// The id of the message's template.
pub fn template_id(message: &str) -> String {
    let (h1, _) = search_aggs::util::murmur3_x64_128(template(message).as_bytes(), 0);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h1.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids Elasticsearch gives these messages (its pattern_text YAML tests).
    #[test]
    fn template_ids_match_elasticsearch() {
        assert_eq!(template("Found 5 errors for service [cheddar1]"), "Found  errors for service []");
        assert_eq!(template_id("Found 5 errors for service [cheddar1]"), "mOVsnxlxdac");
        assert_eq!(template_id("Found 123 errors for service [gorgonzola-24]"), "mOVsnxlxdac");
        assert_eq!(template_id("[2020-08-18T00:58:56] Found 123 errors for service [cheddar1]"), "1l_PtCLQ5xY");
        assert_eq!(template_id("Found some errors for cheddar data service"), "k-2qtjujOCw");
    }
}
