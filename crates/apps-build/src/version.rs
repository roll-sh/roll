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
/// When a runtime's version stops getting security fixes, by its project's own schedule (Node's
/// majors; Python's, Ruby's and PHP's minors), as a year, month and day.
pub(crate) fn end_of_life(provider: crate::Provider, version: &str) -> Option<(i32, u32, u32)> {
    use crate::Provider::*;
    Some(match (provider, version) {
        (Node, "14") => (2023, 4, 30),
        (Node, "16") => (2023, 9, 11),
        (Node, "18") => (2025, 4, 30),
        (Node, "20") => (2026, 4, 30),
        (Node, "22") => (2027, 4, 30),
        (Node, "24") => (2028, 4, 30),
        (Python, "3.7") => (2023, 6, 27),
        (Python, "3.8") => (2024, 10, 7),
        (Python, "3.9") => (2025, 10, 31),
        (Python, "3.10") => (2026, 10, 31),
        (Python, "3.11") => (2027, 10, 31),
        (Python, "3.12") => (2028, 10, 31),
        (Python, "3.13") => (2029, 10, 31),
        (Ruby, "3.0") => (2024, 4, 23),
        (Ruby, "3.1") => (2025, 3, 31),
        (Ruby, "3.2") => (2026, 3, 31),
        (Ruby, "3.3") => (2027, 3, 31),
        (Php, "8.0") => (2023, 11, 26),
        (Php, "8.1") => (2025, 12, 31),
        (Php, "8.2") => (2026, 12, 31),
        (Php, "8.3") => (2027, 12, 31),
        _ => return None,
    })
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's `days_from_civil`).
pub(crate) fn days(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// What a plan says of its runtime's end of life on day `today` (days since 1970): past it, or
/// within 90 days of it; nothing otherwise.
pub(crate) fn end_of_life_note(provider: crate::Provider, version: &str, today: i64) -> Option<String> {
    let (y, m, d) = end_of_life(provider, version)?;
    let at = days(y, m, d);
    let name = match provider {
        crate::Provider::Node => "Node",
        crate::Provider::Python => "Python",
        crate::Provider::Ruby => "Ruby",
        crate::Provider::Php => "PHP",
        _ => return None,
    };
    let how = match provider {
        crate::Provider::Node => "engines.node in package.json, or .nvmrc",
        crate::Provider::Python => ".python-version",
        crate::Provider::Ruby => ".ruby-version, or the Gemfile's ruby",
        _ => "composer.json's require.php",
    };
    if today >= at {
        Some(format!("{name} {version} reached its end of life on {y}-{m:02}-{d:02}: it gets no security fixes; set a newer one ({how})"))
    } else if at - today <= 90 {
        Some(format!("{name} {version} reaches its end of life on {y}-{m:02}-{d:02}: set a newer one soon ({how})"))
    } else {
        None
    }
}

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

#[cfg(test)]
mod end_of_life_tests {
    use super::*;

    #[test]
    fn a_runtime_past_or_near_its_end_of_life_is_said() {
        let on = |y, m, d| days(y, m, d);
        assert_eq!(days(1970, 1, 1), 0);
        assert!(end_of_life_note(crate::Provider::Node, "18", on(2026, 11, 17)).is_some_and(|n| n.starts_with("Node 18 reached its end of life on 2025-04-30")));
        assert!(end_of_life_note(crate::Provider::Python, "3.12", on(2028, 9, 1)).is_some_and(|n| n.contains("reaches its end of life on 2028-10-31")));
        assert_eq!(end_of_life_note(crate::Provider::Node, "22", on(2026, 11, 17)), None, "months away");
        assert_eq!(end_of_life_note(crate::Provider::Go, "1.22", on(2026, 11, 17)), None, "not in the table");
    }
}
