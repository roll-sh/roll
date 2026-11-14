//! Reference variables (Railway's): a service variable's value may hold `${{ VAR }}`, another
//! of the environment's variables as it resolves (roll's, a link's, a project secret, another
//! of its own), or `${{ service.VAR }}`, one of another service of the project in the same
//! environment (its own variables, `PORT`, `ROLL_PRIVATE_HOST`, `ROLL_PUBLIC_URL`). They're
//! filled in at each deploy, one level deep (a referenced value's own references stay as
//! written); one that names nothing is empty, and the deploy's log says so.

/// One `${{ … }}` in a value: the service it names (none: this environment's own) and the
/// variable.
#[derive(Debug, PartialEq)]
pub struct Ref<'a> {
    pub service: Option<&'a str>,
    pub var: &'a str,
}

fn var_name(s: &str) -> bool {
    let mut b = s.bytes();
    b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn service_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The references in `value`, each with where it is (`start..end` of the whole `${{ … }}`).
/// Anything else between `${{` and `}}` is left as text.
pub fn refs(value: &str) -> Vec<(Ref<'_>, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(open) = value[at..].find("${{").map(|i| at + i) {
        let Some(close) = value[open + 3..].find("}}").map(|i| open + 3 + i) else { break };
        let inner = value[open + 3..close].trim();
        let r = match inner.split_once('.') {
            Some((svc, var)) if service_name(svc) && var_name(var) => Some(Ref { service: Some(svc), var }),
            None if var_name(inner) => Some(Ref { service: None, var: inner }),
            _ => None,
        };
        if let Some(r) = r {
            out.push((r, open..close + 2));
        }
        at = close + 2;
    }
    out
}

/// `value` with its references filled in by `lookup`, and the ones it couldn't (as written,
/// `api.KEY` or `KEY`), which are empty.
pub fn fill(value: &str, mut lookup: impl FnMut(&Ref<'_>) -> Option<String>) -> (String, Vec<String>) {
    let mut out = String::with_capacity(value.len());
    let mut missing = Vec::new();
    let mut last = 0;
    for (r, span) in refs(value) {
        out.push_str(&value[last..span.start]);
        match lookup(&r) {
            Some(v) => out.push_str(&v),
            None => missing.push(r.service.map_or_else(|| r.var.to_string(), |s| format!("{s}.{}", r.var))),
        }
        last = span.end;
    }
    out.push_str(&value[last..]);
    (out, missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_filled_and_the_rest_is_text() {
        let v = "postgres://${{ db.USER }}:${{db.PASS}}@${{ ROLL_PRIVATE_HOST }}/x ${{ not a ref }} ${{ Bad.X }} $${{";
        let names: Vec<(Option<&str>, &str)> = refs(v).into_iter().map(|(r, _)| (r.service, r.var)).collect();
        assert_eq!(names, [(Some("db"), "USER"), (Some("db"), "PASS"), (None, "ROLL_PRIVATE_HOST")]);
        let (filled, missing) = fill(v, |r| match (r.service, r.var) {
            (Some("db"), "USER") => Some("app".into()),
            (None, "ROLL_PRIVATE_HOST") => Some("api.production.internal".into()),
            _ => None,
        });
        assert_eq!(filled, "postgres://app:@api.production.internal/x ${{ not a ref }} ${{ Bad.X }} $${{");
        assert_eq!(missing, ["db.PASS"]);
    }
}
