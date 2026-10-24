//! A validator for JMX object names (`javax.management.ObjectName`), which Cassandra applies to
//! `MBEAN` / `MBEANS` resources while parsing. Only validity and "is a pattern" matter here.

/// Parses `domain:key=value,...`. `Some(is_pattern)` when valid.
pub(crate) fn parse_object_name(s: &str) -> Option<bool> {
    if s.contains('\n') {
        return None;
    }
    let colon = s.find(':')?;
    let (domain, props) = (&s[..colon], &s[colon + 1..]);
    let mut pattern = domain.contains(['*', '?']);
    if props.is_empty() {
        return None;
    }
    if props == "*" {
        return Some(true);
    }
    let mut keys: Vec<&str> = Vec::new();
    let mut rest = props;
    loop {
        if rest == "*" {
            pattern = true;
            break;
        }
        let eq = rest.find('=')?;
        let key = &rest[..eq];
        if key.is_empty() || key.contains([',', ':', '*', '?', '"']) || keys.contains(&key) {
            return None;
        }
        keys.push(key);
        rest = &rest[eq + 1..];
        let value_end;
        if let Some(quoted) = rest.strip_prefix('"') {
            // Quoted value: backslash escapes, ends at the closing quote.
            let mut i = 0;
            let b = quoted.as_bytes();
            loop {
                match b.get(i) {
                    None => return None,
                    Some(b'\\') => {
                        match b.get(i + 1) {
                            Some(b'"' | b'\\' | b'n' | b'*' | b'?') => {
                                if matches!(b[i + 1], b'*' | b'?') {
                                    pattern = true;
                                }
                            }
                            _ => return None,
                        }
                        i += 2;
                    }
                    Some(b'"') => break,
                    Some(b'*' | b'?') => {
                        pattern = true;
                        i += 1;
                    }
                    Some(_) => i += 1,
                }
            }
            value_end = 1 + i + 1;
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let value = &rest[..end];
            if value.is_empty() || value.contains(['=', ':', '"']) {
                return None;
            }
            if value.contains(['*', '?']) {
                pattern = true;
            }
            value_end = end;
        }
        rest = &rest[value_end..];
        if rest.is_empty() {
            break;
        }
        rest = rest.strip_prefix(',')?;
        if rest.is_empty() {
            return None;
        }
    }
    Some(pattern)
}

#[cfg(test)]
mod tests {
    use super::parse_object_name as p;

    #[test]
    fn object_names() {
        assert_eq!(p("org.apache:type=Foo"), Some(false));
        assert_eq!(p("org.apache:type=Foo,name=Bar"), Some(false));
        assert_eq!(p("org.apache:type=*"), Some(true));
        assert_eq!(p("org.*:type=Foo"), Some(true));
        assert_eq!(p("org.apache:type=Foo,*"), Some(true));
        assert_eq!(p("org.apache:*"), Some(true));
        assert_eq!(p("org.apache:type=\"a,b\""), Some(false));
        assert_eq!(p("nocolon"), None);
        assert_eq!(p("d:"), None);
        assert_eq!(p("d:type"), None);
        assert_eq!(p("d:type=a,type=b"), None);
        assert_eq!(p("d:type=a,"), None);
    }
}
