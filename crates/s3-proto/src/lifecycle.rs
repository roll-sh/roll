//! `LifecycleConfiguration`: the full S3 rule model, parsed and validated as S3 does, and
//! rendered back in the form it was given (legacy `<Rule><Prefix>`, or a `<Filter>` with one
//! predicate, or `<And>` for several).

use crate::error::{Code, S3Error};
use crate::time::{iso8601_ms, parse_iso8601};
use crate::xml::{Node, W, expect_root, malformed};

/// Storage classes a transition may name.
pub const TRANSITION_CLASSES: [&str; 6] = ["GLACIER", "STANDARD_IA", "ONEZONE_IA", "INTELLIGENT_TIERING", "DEEP_ARCHIVE", "GLACIER_IR"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transition {
    pub days: Option<u32>,
    /// Unix ms (midnight UTC).
    pub date: Option<i64>,
    pub storage_class: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoncurrentTransition {
    pub noncurrent_days: u32,
    pub newer_noncurrent_versions: Option<u32>,
    pub storage_class: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rule {
    pub id: Option<String>,
    pub enabled: bool,
    /// The rule used the legacy `<Rule><Prefix>` form (no `<Filter>`).
    pub legacy_prefix: bool,
    pub prefix: String,
    /// Every tag must match.
    pub tags: Vec<(String, String)>,
    pub size_greater_than: Option<u64>,
    pub size_less_than: Option<u64>,
    pub expire_days: Option<u32>,
    /// Unix ms (midnight UTC).
    pub expire_date: Option<i64>,
    pub expire_delete_markers: bool,
    pub noncurrent_days: Option<u32>,
    pub newer_noncurrent_versions: Option<u32>,
    pub transitions: Vec<Transition>,
    pub noncurrent_transitions: Vec<NoncurrentTransition>,
    pub abort_multipart_days: Option<u32>,
}

fn invalid(msg: impl Into<String>) -> S3Error {
    S3Error::msg(Code::InvalidArgument, msg)
}

fn request(msg: impl Into<String>) -> S3Error {
    S3Error::msg(Code::InvalidRequest, msg)
}

/// A whole number of days; `min` and up.
fn days(n: &Node, name: &str, what: &str, min: u32) -> Result<Option<u32>, S3Error> {
    let Some(t) = n.text_of(name) else { return Ok(None) };
    let d: i64 = t.trim().parse().map_err(|_| malformed())?;
    if d < min as i64 || d > u32::MAX as i64 {
        return Err(invalid(if min == 0 { format!("'{name}' for {what} action must be a nonnegative integer") } else { format!("'{name}' for {what} action must be a positive integer") }));
    }
    Ok(Some(d as u32))
}

fn date(n: &Node, what: &str) -> Result<Option<i64>, S3Error> {
    let Some(t) = n.text_of("Date") else { return Ok(None) };
    let t = t.trim();
    // S3 takes `2030-01-01T00:00:00Z`, with or without fractions, or `+00:00`.
    let ms = parse_iso8601(t).or_else(|| parse_iso8601(&format!("{}Z", t.strip_suffix("+00:00")?))).ok_or_else(|| invalid(format!("'Date' for {what} action is not a valid ISO 8601 date")))?;
    if ms.rem_euclid(86_400_000) != 0 {
        return Err(invalid(format!("'Date' for {what} action must be at midnight GMT")));
    }
    Ok(Some(ms))
}

fn newer_versions(n: &Node) -> Result<Option<u32>, S3Error> {
    let Some(t) = n.text_of("NewerNoncurrentVersions") else { return Ok(None) };
    let v: i64 = t.trim().parse().map_err(|_| malformed())?;
    if !(1..=100).contains(&v) {
        return Err(invalid("NewerNoncurrentVersions must be between 1 and 100"));
    }
    Ok(Some(v as u32))
}

fn size(n: &Node, name: &str) -> Result<Option<u64>, S3Error> {
    n.text_of(name).map(|t| t.trim().parse::<u64>().map_err(|_| malformed())).transpose()
}

fn tag(n: &Node) -> Result<(String, String), S3Error> {
    Ok((n.text_of("Key").ok_or_else(malformed)?.to_string(), n.text_of("Value").unwrap_or_default().to_string()))
}

fn storage_class(n: &Node) -> Result<String, S3Error> {
    let c = n.text_of("StorageClass").map(str::trim).ok_or_else(malformed)?;
    if !TRANSITION_CLASSES.contains(&c) {
        return Err(S3Error::new(Code::InvalidStorageClass).with("StorageClassRequested", c.to_string()));
    }
    Ok(c.to_string())
}

/// The infrequent-access classes need objects at least 30 days old, as on S3.
fn check_ia(class: &str, days: Option<u32>, what: &str) -> Result<(), S3Error> {
    if matches!(class, "STANDARD_IA" | "ONEZONE_IA") && days.is_some_and(|d| d < 30) {
        return Err(invalid(format!("'{what}' in {} action must be greater than or equal to 30 for storageClass '{class}'", if what == "Days" { "Transition" } else { "NoncurrentVersionTransition" })));
    }
    Ok(())
}

fn parse_filter(r: &Node, rule: &mut Rule) -> Result<(), S3Error> {
    let Some(f) = r.child("Filter") else {
        if let Some(p) = r.text_of("Prefix") {
            rule.prefix = p.to_string();
            rule.legacy_prefix = true;
        }
        return Ok(());
    };
    let preds = ["Prefix", "Tag", "And", "ObjectSizeGreaterThan", "ObjectSizeLessThan"];
    if f.children.iter().filter(|c| preds.contains(&c.name.as_str())).count() > 1 {
        return Err(malformed());
    }
    let src = match f.child("And") {
        Some(and) => and,
        None => f,
    };
    rule.prefix = src.text_of("Prefix").unwrap_or_default().to_string();
    rule.tags = src.all("Tag").map(tag).collect::<Result<_, _>>()?;
    if f.child("And").is_none() && rule.tags.len() > 1 {
        return Err(malformed());
    }
    rule.size_greater_than = size(src, "ObjectSizeGreaterThan")?;
    rule.size_less_than = size(src, "ObjectSizeLessThan")?;
    let mut keys: Vec<&str> = rule.tags.iter().map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    if keys.windows(2).any(|w| w[0] == w[1]) {
        return Err(request("Duplicate Tag Keys are not allowed."));
    }
    if !rule.tags.is_empty() {
        crate::http::validate_tags(&rule.tags)?;
    }
    if let (Some(gt), Some(lt)) = (rule.size_greater_than, rule.size_less_than)
        && gt >= lt
    {
        return Err(invalid("ObjectSizeGreaterThan must be less than ObjectSizeLessThan"));
    }
    Ok(())
}

fn parse_rule(r: &Node) -> Result<Rule, S3Error> {
    let mut rule = Rule {
        enabled: match r.text_of("Status").map(str::trim) {
            Some("Enabled") => true,
            Some("Disabled") => false,
            _ => return Err(malformed()),
        },
        id: r.text_of("ID").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        ..Default::default()
    };
    if rule.id.as_ref().is_some_and(|i| i.len() > 255) {
        return Err(invalid("ID length should not exceed allowed limit of 255"));
    }
    parse_filter(r, &mut rule)?;
    if let Some(e) = r.child("Expiration") {
        rule.expire_days = days(e, "Days", "Expiration", 1)?;
        rule.expire_date = date(e, "Expiration")?;
        rule.expire_delete_markers = match e.text_of("ExpiredObjectDeleteMarker").map(str::trim) {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(malformed()),
        };
        let set = usize::from(rule.expire_days.is_some()) + usize::from(rule.expire_date.is_some()) + usize::from(e.child("ExpiredObjectDeleteMarker").is_some());
        if set != 1 {
            return Err(if e.child("ExpiredObjectDeleteMarker").is_some() { invalid("ExpiredObjectDeleteMarker cannot be specified with Days or Date in a Lifecycle Expiration Policy.") } else { malformed() });
        }
        if rule.expire_delete_markers && !rule.tags.is_empty() {
            return Err(request("ExpiredObjectDeleteMarker cannot be specified with tags."));
        }
    }
    if let Some(n) = r.child("NoncurrentVersionExpiration") {
        rule.noncurrent_days = Some(days(n, "NoncurrentDays", "NoncurrentVersionExpiration", 1)?.ok_or_else(malformed)?);
        rule.newer_noncurrent_versions = newer_versions(n)?;
    }
    for t in r.all("Transition") {
        let tr = Transition { days: days(t, "Days", "Transition", 0)?, date: date(t, "Transition")?, storage_class: storage_class(t)? };
        if tr.days.is_some() == tr.date.is_some() {
            return Err(malformed());
        }
        check_ia(&tr.storage_class, tr.days, "Days")?;
        rule.transitions.push(tr);
    }
    for t in r.all("NoncurrentVersionTransition") {
        let tr = NoncurrentTransition {
            noncurrent_days: days(t, "NoncurrentDays", "NoncurrentVersionTransition", 0)?.ok_or_else(malformed)?,
            newer_noncurrent_versions: newer_versions(t)?,
            storage_class: storage_class(t)?,
        };
        check_ia(&tr.storage_class, Some(tr.noncurrent_days), "NoncurrentDays")?;
        rule.noncurrent_transitions.push(tr);
    }
    let mut classes: Vec<&str> = rule.transitions.iter().map(|t| t.storage_class.as_str()).collect();
    classes.sort_unstable();
    let mut nc_classes: Vec<&str> = rule.noncurrent_transitions.iter().map(|t| t.storage_class.as_str()).collect();
    nc_classes.sort_unstable();
    if classes.windows(2).any(|w| w[0] == w[1]) || nc_classes.windows(2).any(|w| w[0] == w[1]) {
        return Err(request("Found duplicate storage class in transitions of a lifecycle rule"));
    }
    if rule.transitions.iter().any(|t| t.days.is_some()) && rule.transitions.iter().any(|t| t.date.is_some()) {
        return Err(request("Found mixed 'Date' and 'Days' based Transition actions in lifecycle rule"));
    }
    if let Some(e) = rule.expire_days
        && rule.transitions.iter().filter_map(|t| t.days).any(|d| d >= e)
    {
        return Err(invalid("'Days' in the Expiration action must be greater than 'Days' in the Transition action"));
    }
    if let Some(e) = rule.noncurrent_days
        && rule.noncurrent_transitions.iter().any(|t| t.noncurrent_days >= e)
    {
        return Err(invalid("'NoncurrentDays' in the NoncurrentVersionExpiration action must be greater than 'NoncurrentDays' in the NoncurrentVersionTransition action"));
    }
    if let Some(a) = r.child("AbortIncompleteMultipartUpload") {
        rule.abort_multipart_days = Some(days(a, "DaysAfterInitiation", "AbortIncompleteMultipartUpload", 1)?.ok_or_else(malformed)?);
        if !rule.tags.is_empty() || rule.size_greater_than.is_some() || rule.size_less_than.is_some() {
            return Err(request("AbortIncompleteMultipartUpload cannot be specified with tags or object size filters."));
        }
    }
    let acts = rule.expire_days.is_some()
        || rule.expire_date.is_some()
        || rule.expire_delete_markers
        || r.child("Expiration").is_some()
        || rule.noncurrent_days.is_some()
        || !rule.transitions.is_empty()
        || !rule.noncurrent_transitions.is_empty()
        || rule.abort_multipart_days.is_some();
    if !acts {
        return Err(malformed());
    }
    Ok(rule)
}

/// A `PutBucketLifecycleConfiguration` body (1–1000 rules, unique IDs).
pub fn parse(body: &[u8]) -> Result<Vec<Rule>, S3Error> {
    let n = expect_root(body, "LifecycleConfiguration")?;
    let rules: Vec<Rule> = n.all("Rule").map(parse_rule).collect::<Result<_, _>>()?;
    if rules.is_empty() || rules.len() > 1000 {
        return Err(malformed());
    }
    let mut ids: Vec<&str> = rules.iter().filter_map(|r| r.id.as_deref()).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(invalid("Rule ID must be unique. Found same ID for more than one rule"));
    }
    Ok(rules)
}

fn filter(w: &mut W, r: &Rule) {
    if r.legacy_prefix {
        w.el("Prefix", &r.prefix);
        return;
    }
    let n = usize::from(!r.prefix.is_empty()) + r.tags.len() + usize::from(r.size_greater_than.is_some()) + usize::from(r.size_less_than.is_some());
    w.open("Filter");
    if n > 1 {
        w.open("And");
    }
    if !r.prefix.is_empty() || n == 0 {
        w.el("Prefix", &r.prefix);
    }
    for (k, v) in &r.tags {
        w.open("Tag");
        w.el("Key", k);
        w.el("Value", v);
        w.close("Tag");
    }
    if let Some(s) = r.size_greater_than {
        w.el("ObjectSizeGreaterThan", &s.to_string());
    }
    if let Some(s) = r.size_less_than {
        w.el("ObjectSizeLessThan", &s.to_string());
    }
    if n > 1 {
        w.close("And");
    }
    w.close("Filter");
}

/// `GetBucketLifecycleConfiguration`.
pub fn render(rules: &[Rule]) -> String {
    let mut w = W::root("LifecycleConfiguration");
    for r in rules {
        w.open("Rule");
        w.opt("ID", r.id.as_deref());
        filter(&mut w, r);
        w.el("Status", if r.enabled { "Enabled" } else { "Disabled" });
        for t in &r.transitions {
            w.open("Transition");
            if let Some(d) = t.days {
                w.el("Days", &d.to_string());
            }
            if let Some(d) = t.date {
                w.el("Date", &iso8601_ms(d));
            }
            w.el("StorageClass", &t.storage_class);
            w.close("Transition");
        }
        if r.expire_days.is_some() || r.expire_date.is_some() || r.expire_delete_markers {
            w.open("Expiration");
            if let Some(d) = r.expire_days {
                w.el("Days", &d.to_string());
            }
            if let Some(d) = r.expire_date {
                w.el("Date", &iso8601_ms(d));
            }
            if r.expire_delete_markers {
                w.el("ExpiredObjectDeleteMarker", "true");
            }
            w.close("Expiration");
        }
        for t in &r.noncurrent_transitions {
            w.open("NoncurrentVersionTransition");
            w.el("NoncurrentDays", &t.noncurrent_days.to_string());
            w.el("StorageClass", &t.storage_class);
            if let Some(n) = t.newer_noncurrent_versions {
                w.el("NewerNoncurrentVersions", &n.to_string());
            }
            w.close("NoncurrentVersionTransition");
        }
        if let Some(d) = r.noncurrent_days {
            w.open("NoncurrentVersionExpiration");
            w.el("NoncurrentDays", &d.to_string());
            if let Some(n) = r.newer_noncurrent_versions {
                w.el("NewerNoncurrentVersions", &n.to_string());
            }
            w.close("NoncurrentVersionExpiration");
        }
        if let Some(d) = r.abort_multipart_days {
            w.open("AbortIncompleteMultipartUpload");
            w.el("DaysAfterInitiation", &d.to_string());
            w.close("AbortIncompleteMultipartUpload");
        }
        w.close("Rule");
    }
    w.finish("LifecycleConfiguration")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lc(rules: &str) -> Result<Vec<Rule>, Code> {
        parse(format!("<LifecycleConfiguration>{rules}</LifecycleConfiguration>").as_bytes()).map_err(|e| e.code)
    }

    fn r(body: &str) -> String {
        format!("<Rule><Status>Enabled</Status>{body}</Rule>")
    }

    #[test]
    fn round_trips() {
        let rules = vec![
            Rule { id: Some("logs".into()), enabled: true, prefix: "logs/".into(), expire_days: Some(30), abort_multipart_days: Some(7), ..Default::default() },
            Rule { enabled: false, abort_multipart_days: Some(1), ..Default::default() },
            Rule { id: Some("legacy".into()), enabled: true, legacy_prefix: true, prefix: "tmp/".into(), expire_days: Some(1), ..Default::default() },
            Rule { enabled: true, tags: vec![("a".into(), "1".into())], expire_date: Some(1_893_456_000_000), ..Default::default() },
            Rule {
                enabled: true,
                prefix: "p/".into(),
                tags: vec![("a".into(), "1".into()), ("b".into(), "".into())],
                size_greater_than: Some(10),
                size_less_than: Some(1000),
                noncurrent_days: Some(40),
                newer_noncurrent_versions: Some(3),
                transitions: vec![Transition { days: Some(30), date: None, storage_class: "STANDARD_IA".into() }, Transition { days: Some(90), date: None, storage_class: "GLACIER".into() }],
                noncurrent_transitions: vec![NoncurrentTransition { noncurrent_days: 30, newer_noncurrent_versions: Some(2), storage_class: "ONEZONE_IA".into() }],
                expire_days: Some(365),
                ..Default::default()
            },
            Rule { enabled: true, size_greater_than: Some(5), expire_delete_markers: true, ..Default::default() },
        ];
        let doc = render(&rules);
        assert_eq!(parse(doc.as_bytes()).unwrap(), rules, "{doc}");
        assert!(doc.contains("<Rule><ID>legacy</ID><Prefix>tmp/</Prefix><Status>"), "the legacy form stays legacy: {doc}");
        assert!(doc.contains("<Filter><Tag><Key>a</Key><Value>1</Value></Tag></Filter>"), "one predicate without And: {doc}");
        assert!(doc.contains("<Filter><And><Prefix>p/</Prefix><Tag>"), "{doc}");
        assert!(doc.contains("<Filter><Prefix></Prefix></Filter>"), "{doc}");
        assert!(doc.contains("<Date>2030-01-01T00:00:00.000Z</Date>"), "{doc}");
    }

    #[test]
    fn s3_forms_parse() {
        let legacy = lc(&r("<ID>x</ID><Prefix>tmp/</Prefix><Expiration><Days>1</Days></Expiration>")).unwrap();
        assert_eq!((legacy[0].prefix.as_str(), legacy[0].legacy_prefix), ("tmp/", true));
        let filtered = lc(&r("<Prefix>old/</Prefix><Filter><Prefix>new/</Prefix></Filter><Expiration><Days>2</Days></Expiration>")).unwrap();
        assert_eq!((filtered[0].prefix.as_str(), filtered[0].legacy_prefix), ("new/", false));
        let nc = lc(&r("<Filter/><NoncurrentVersionExpiration><NoncurrentDays>3</NoncurrentDays></NoncurrentVersionExpiration>")).unwrap();
        assert_eq!(nc[0].noncurrent_days, Some(3));
        let dm = lc(&r("<Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>")).unwrap();
        assert!(dm[0].expire_delete_markers);
        assert!(!lc(&r("<Expiration><ExpiredObjectDeleteMarker>false</ExpiredObjectDeleteMarker></Expiration>")).unwrap()[0].expire_delete_markers, "false is still an action");
        let date = lc(&r("<Expiration><Date>2030-01-01T00:00:00+00:00</Date></Expiration>")).unwrap();
        assert_eq!(date[0].expire_date, Some(1_893_456_000_000));
        let size = lc(&r("<Filter><ObjectSizeLessThan>5</ObjectSizeLessThan></Filter><Expiration><Days>1</Days></Expiration>")).unwrap();
        assert_eq!(size[0].size_less_than, Some(5));
        let tr0 = lc(&r("<Transition><Days>0</Days><StorageClass>INTELLIGENT_TIERING</StorageClass></Transition>")).unwrap();
        assert_eq!(tr0[0].transitions[0].days, Some(0));
        assert_eq!(lc(&r("<Expiration><Days>1</Days></Expiration>").repeat(2)).unwrap().len(), 2, "rules without IDs don't collide");
        assert_eq!(lc(&r(&format!("<ID>{}</ID><Expiration><Days>1</Days></Expiration>", "i".repeat(255)))).unwrap()[0].id.as_ref().unwrap().len(), 255);
    }

    #[test]
    fn s3_validation() {
        let mal = Code::MalformedXML;
        let inv = Code::InvalidArgument;
        let req = Code::InvalidRequest;
        for (body, want) in [
            ("<Expiration><Days>0</Days></Expiration>", inv),
            ("<Expiration><Days>x</Days></Expiration>", mal),
            ("<Expiration><Days>1</Days><Date>2030-01-01T00:00:00Z</Date></Expiration>", mal),
            ("<Expiration><Days>1</Days><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>", inv),
            ("<Expiration><Date>2030-01-01T10:00:00Z</Date></Expiration>", inv),
            ("<Expiration><Date>tomorrow</Date></Expiration>", inv),
            ("<Expiration></Expiration>", mal),
            ("<Filter><Tag><Key>a</Key><Value>b</Value></Tag></Filter><Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>", req),
            ("<Filter><Prefix>a</Prefix><Tag><Key>a</Key><Value>b</Value></Tag></Filter><Expiration><Days>1</Days></Expiration>", mal),
            ("<Filter><And><Tag><Key>a</Key><Value>b</Value></Tag><Tag><Key>a</Key><Value>c</Value></Tag></And></Filter><Expiration><Days>1</Days></Expiration>", req),
            ("<Filter><And><ObjectSizeGreaterThan>9</ObjectSizeGreaterThan><ObjectSizeLessThan>9</ObjectSizeLessThan></And></Filter><Expiration><Days>1</Days></Expiration>", inv),
            ("<Filter><ObjectSizeGreaterThan>-1</ObjectSizeGreaterThan></Filter><Expiration><Days>1</Days></Expiration>", mal),
            ("<NoncurrentVersionExpiration></NoncurrentVersionExpiration>", mal),
            ("<NoncurrentVersionExpiration><NoncurrentDays>0</NoncurrentDays></NoncurrentVersionExpiration>", inv),
            ("<NoncurrentVersionExpiration><NoncurrentDays>1</NoncurrentDays><NewerNoncurrentVersions>101</NewerNoncurrentVersions></NoncurrentVersionExpiration>", inv),
            ("<Transition><Days>30</Days><StorageClass>REDUCED_REDUNDANCY</StorageClass></Transition>", Code::InvalidStorageClass),
            ("<Transition><Days>29</Days><StorageClass>STANDARD_IA</StorageClass></Transition>", inv),
            ("<Transition><StorageClass>GLACIER</StorageClass></Transition>", mal),
            ("<Transition><Days>1</Days><Date>2030-01-01T00:00:00Z</Date><StorageClass>GLACIER</StorageClass></Transition>", mal),
            ("<Transition><Days>1</Days><StorageClass>GLACIER</StorageClass></Transition><Transition><Days>2</Days><StorageClass>GLACIER</StorageClass></Transition>", req),
            ("<Transition><Days>1</Days><StorageClass>GLACIER</StorageClass></Transition><Transition><Date>2030-01-01T00:00:00Z</Date><StorageClass>DEEP_ARCHIVE</StorageClass></Transition>", req),
            ("<Transition><Days>10</Days><StorageClass>GLACIER</StorageClass></Transition><Expiration><Days>10</Days></Expiration>", inv),
            ("<NoncurrentVersionTransition><NoncurrentDays>10</NoncurrentDays><StorageClass>STANDARD_IA</StorageClass></NoncurrentVersionTransition>", inv),
            ("<NoncurrentVersionTransition><NoncurrentDays>10</NoncurrentDays><StorageClass>GLACIER</StorageClass></NoncurrentVersionTransition><NoncurrentVersionExpiration><NoncurrentDays>5</NoncurrentDays></NoncurrentVersionExpiration>", inv),
            ("<AbortIncompleteMultipartUpload></AbortIncompleteMultipartUpload>", mal),
            ("<AbortIncompleteMultipartUpload><DaysAfterInitiation>0</DaysAfterInitiation></AbortIncompleteMultipartUpload>", inv),
            ("<Filter><Tag><Key>a</Key><Value>b</Value></Tag></Filter><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload>", req),
            ("", mal),
        ] {
            assert_eq!(lc(&r(body)).unwrap_err(), want, "{body}");
        }
        assert_eq!(lc("<Rule><Expiration><Days>1</Days></Expiration></Rule>").unwrap_err(), mal, "no Status");
        assert_eq!(lc("<Rule><Status>On</Status><Expiration><Days>1</Days></Expiration></Rule>").unwrap_err(), mal);
        assert_eq!(lc("").unwrap_err(), mal);
        assert_eq!(lc(&r(&format!("<ID>{}</ID><Expiration><Days>1</Days></Expiration>", "i".repeat(256)))).unwrap_err(), inv);
        assert_eq!(lc(&(r("<ID>a</ID><Expiration><Days>1</Days></Expiration>") + &r("<ID>a</ID><Expiration><Days>2</Days></Expiration>"))).unwrap_err(), inv);
        assert_eq!(lc(&r("<ID>x</ID><Expiration><Days>1</Days></Expiration>").repeat(1001)).unwrap_err(), mal);
    }
}
