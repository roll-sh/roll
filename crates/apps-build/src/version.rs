//! Language versions from pins (`.nvmrc`, `.python-version`) and constraints (`engines.node`,
//! `requires-python`, composer's `php`, the Gemfile's `ruby`).

/// `major.minor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Ver(pub u32, pub u32);

impl Ver {
    pub(crate) fn minor(self) -> String {
        format!("{}.{}", self.0, self.1)
    }
}

/// The first `major[.minor]` in `s`: "v20.11.0" is 20.11, "ruby-3.2.2" is 3.2.
pub(crate) fn parse(s: &str) -> Option<Ver> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let token = Token::parse(&s[start..])?;
    Some(Ver(token.major, token.minor.unwrap_or(0)))
}

/// The version to build with for `constraint` (">=3.10,<3.13", "^20", "~> 3.2", "20.x",
/// "^8.2|^8.3"): `default` when it fits, else the nearest version that does.
pub(crate) fn pick(constraint: &str, default: Ver) -> Option<Ver> {
    constraint
        .split('|')
        .filter_map(|alt| pick_one(alt, default))
        .max()
}

fn pick_one(alt: &str, default: Ver) -> Option<Ver> {
    let (mut lower, mut upper, mut exact) = (None, None, None);
    let mut s = alt;
    loop {
        s = s.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if s.is_empty() {
            break;
        }
        let op_end = s.find(|c: char| !"<>=!~^".contains(c)).unwrap_or(s.len());
        let op = &s[..op_end];
        let rest = s[op_end..].trim_start();
        let v_end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '*'))
            .unwrap_or(rest.len());
        let token = &rest[..v_end];
        s = &rest[v_end..];
        if op_end == 0 && v_end == 0 {
            let skip = s.chars().next().map_or(0, char::len_utf8);
            s = &s[skip..];
            continue;
        }
        let Some(t) = Token::parse(token.trim_start_matches('v')) else {
            continue;
        };
        let v = Ver(t.major, t.minor.unwrap_or(0));
        match op {
            ">=" | ">" | "~=" => lower = Some(v),
            "^" | "~" => {
                lower = Some(v);
                upper = Some(Ver(t.major + 1, 0));
            }
            "~>" => {
                lower = Some(v);
                upper = Some(if t.patch {
                    Ver(t.major, v.1 + 1)
                } else {
                    Ver(t.major + 1, 0)
                });
            }
            "<" => upper = Some(v),
            "<=" => upper = Some(Ver(v.0, v.1 + 1)),
            "!=" => {}
            _ => match t.minor {
                Some(minor) if !t.wildcard => exact = Some(Ver(t.major, minor)),
                Some(minor) => {
                    lower = Some(Ver(t.major, minor));
                    upper = Some(Ver(t.major, minor + 1));
                }
                None => {
                    lower = Some(Ver(t.major, 0));
                    upper = Some(Ver(t.major + 1, 0));
                }
            },
        }
    }
    if exact.is_some() {
        return exact;
    }
    if lower.is_none() && upper.is_none() {
        return None;
    }
    let mut v = lower.map_or(default, |l| l.max(default));
    if let Some(u) = upper
        && v >= u
    {
        v = if u.1 > 0 {
            Ver(u.0, u.1 - 1)
        } else {
            Ver(u.0.saturating_sub(1), 0)
        };
        if let Some(l) = lower {
            v = v.max(l);
        }
    }
    Some(v)
}

/// `20`, `20.x`, `3.11.*`, `3.2.2`.
struct Token {
    major: u32,
    minor: Option<u32>,
    patch: bool,
    wildcard: bool,
}

impl Token {
    fn parse(s: &str) -> Option<Token> {
        let mut parts = s.split('.');
        let digits = |p: &str| {
            let end = p.find(|c: char| !c.is_ascii_digit()).unwrap_or(p.len());
            p[..end].parse::<u32>().ok()
        };
        let major = digits(parts.next()?)?;
        let minor_part = parts.next();
        let patch_part = parts.next();
        let wild = |p: Option<&str>| p.is_some_and(|p| matches!(p, "x" | "X" | "*"));
        Some(Token {
            major,
            minor: minor_part.and_then(digits),
            patch: patch_part.is_some(),
            wildcard: wild(minor_part) || wild(patch_part),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraints_pick_the_default_or_the_nearest_fit() {
        let node = Ver(22, 0);
        let python = Ver(3, 12);
        for (c, default, want) in [
            (">=18", node, Ver(22, 0)),
            ("^20.1", node, Ver(20, 1)),
            ("20.x", node, Ver(20, 0)),
            ("18 || 20", node, Ver(20, 0)),
            (">= 18 < 21", node, Ver(20, 0)),
            ("v20.11.0", node, Ver(20, 11)),
            (">=3.9", python, Ver(3, 12)),
            (">=3.10,<3.12", python, Ver(3, 11)),
            ("==3.11.*", python, Ver(3, 11)),
            (">=3.13", python, Ver(3, 13)),
            ("^3.11", python, Ver(3, 12)),
            ("~> 3.2.0", Ver(3, 3), Ver(3, 2)),
            ("^8.2|^8.4", Ver(8, 3), Ver(8, 4)),
        ] {
            assert_eq!(pick(c, default), Some(want), "{c}");
        }
        assert_eq!(pick("lts/*", node), None);
        assert_eq!(parse("python-3.11.4"), Some(Ver(3, 11)));
    }
}
