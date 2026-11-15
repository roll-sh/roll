//! Log filters by a line's fields (Railway's `@attr:value`, Cloud Run's `jsonPayload.x`):
//! `level:error user_id:42` keeps the lines whose fields say so, all of them. A line's fields are
//! its JSON object's (a dotted key reaches into nested objects: `req.method:POST`) or its logfmt
//! pairs (`user_id=42 level=error`). `key:*` asks only that the field is there, `-key:value`
//! that it isn't that; a quoted value may hold spaces (`msg:"cache miss"`). Values compare as
//! text, in any case, so `ok:true` and `status:500` match JSON's booleans and numbers.

use serde_json::Value;

/// At most this many terms.
const MAX_TERMS: usize = 10;

#[derive(Clone, Debug, PartialEq)]
pub struct Where(Vec<Term>);

#[derive(Clone, Debug, PartialEq)]
struct Term {
    key: Vec<String>,
    /// None: the field is there.
    value: Option<String>,
    not: bool,
}

impl Where {
    pub fn parse(s: &str) -> Result<Where, String> {
        let mut terms = Vec::new();
        for token in tokens(s)? {
            let (not, t) = match token.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, token.as_str()),
            };
            let t = t.strip_prefix('@').unwrap_or(t);
            let Some((key, value)) = t.split_once(':') else { return Err(format!("{token:?} isn't key:value")) };
            if !key_ok(key) {
                return Err(format!("{key:?} isn't a field name (letters, digits, _ - and . between parts)"));
            }
            let value = unquote(value);
            if value.chars().count() > 200 {
                return Err("a value is at most 200 characters".into());
            }
            terms.push(Term { key: key.split('.').map(str::to_string).collect(), value: (value != "*").then(|| value.to_lowercase()), not });
        }
        if terms.len() > MAX_TERMS {
            return Err(format!("where is at most {MAX_TERMS} terms"));
        }
        Ok(Where(terms))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `text`'s fields say what every term asks.
    pub fn matches(&self, text: &str) -> bool {
        let fields = Fields::of(text);
        self.0.iter().all(|t| {
            let got = fields.get(&t.key);
            let hit = match (&t.value, &got) {
                (None, g) => g.is_some(),
                (Some(want), Some(g)) => g.to_lowercase() == *want,
                (Some(_), None) => false,
            };
            hit != t.not
        })
    }
}

fn key_ok(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
}

fn unquote(v: &str) -> &str {
    v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v)
}

/// Words split on spaces outside double quotes.
fn tokens(s: &str) -> Result<Vec<String>, String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if quoted {
        return Err("a quote isn't closed".into());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

enum Fields {
    Json(Value),
    Logfmt(Vec<(String, String)>),
}

impl Fields {
    fn of(text: &str) -> Fields {
        let t = text.trim();
        if t.starts_with('{')
            && let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(t)
        {
            return Fields::Json(v);
        }
        Fields::Logfmt(logfmt(t))
    }

    fn get(&self, key: &[String]) -> Option<String> {
        match self {
            Fields::Json(v) => {
                let mut at = v;
                for k in key {
                    at = at.get(k)?;
                }
                match at {
                    Value::String(s) => Some(s.clone()),
                    Value::Null => Some("null".into()),
                    Value::Bool(_) | Value::Number(_) => Some(at.to_string()),
                    // An object or a list is there, but isn't a value to compare.
                    _ => Some(String::new()),
                }
            }
            Fields::Logfmt(pairs) => {
                let k = key.join(".");
                pairs.iter().find(|(name, _)| *name == k).map(|(_, v)| v.clone())
            }
        }
    }
}

/// `k=v` and `k="v w"` pairs; anything else is a word, not a field.
fn logfmt(t: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for w in tokens(t).unwrap_or_default() {
        if let Some((k, v)) = w.split_once('=')
            && key_ok(k)
        {
            out.push((k.to_string(), unquote(v).to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::Where;

    #[test]
    fn fields_of_json_and_logfmt_lines() {
        let json = r#"{"level":"error","user_id":42,"ok":false,"req":{"method":"POST"},"msg":"cache miss"}"#;
        let w = |s: &str| Where::parse(s).unwrap();
        assert!(w("level:error user_id:42").matches(json));
        assert!(!w("user_id:4").matches(json), "4 isn't 42");
        assert!(w("req.method:post ok:false").matches(json) && w(r#"msg:"cache miss""#).matches(json));
        assert!(w("user_id:* -level:info").matches(json) && !w("missing:*").matches(json));
        assert!(w("@level:ERROR").matches(json), "Railway's @ and any case");
        let logfmt = r#"ts=1 level=warn user_id=42 msg="slow query" done"#;
        assert!(w("level:warn user_id:42").matches(logfmt) && w(r#"msg:"slow query""#).matches(logfmt));
        assert!(!w("level:warn").matches("level warn, plain text"));
        assert!(Where::parse("no-colon").is_err() && Where::parse(r#"msg:"open"#).is_err() && Where::parse("a..b:x").is_err());
    }
}
