//! AWS Signature Version 2 (legacy, HMAC-SHA1): `Authorization: AWS <key>:<signature>`,
//! presigned URLs (`?AWSAccessKeyId=&Expires=&Signature=`) and POST policy forms
//! (`AWSAccessKeyId`, `signature`). Old tools (s3cmd's default, boto 2, many backup tools)
//! still sign this way.
//!
//! ```text
//! StringToSign = VERB \n Content-MD5 \n Content-Type \n Date-or-Expires \n
//!                CanonicalizedAmzHeaders CanonicalizedResource
//! ```
//!
//! - `CanonicalizedAmzHeaders`: every `x-amz-*` header, names lowercase and sorted, repeated
//!   values joined with `,`, each `name:value\n`.
//! - `CanonicalizedResource`: `/<bucket>` when the bucket came from the `Host` header, then the
//!   path as sent (still encoded), then the subresources (sorted, values decoded).
//! - Clients disagree on a few details, so the server accepts each of the variants real clients
//!   produce (see [`verify`]): with `x-amz-date`, the date line is empty (the documented rule,
//!   s3cmd), the `Date` header (botocore) or the `x-amz-date` value (AWS's own example); and the
//!   subresource list is botocore's or MinIO's.

use crate::error::{Code, S3Error};
use crate::sigv4::{MAX_SKEW_S, Payload, Request, Verified, constant_time_eq};
use crate::time;
use hmac::{Hmac, Mac};
use sha1::Sha1;

/// Subresources that are signed, as botocore (and so AWS) has them.
pub const SUBRESOURCES: &[&str] = &[
    "accelerate",
    "acl",
    "analytics",
    "cors",
    "defaultObjectAcl",
    "delete",
    "inventory",
    "lifecycle",
    "location",
    "logging",
    "metrics",
    "notification",
    "object-lock",
    "partNumber",
    "policy",
    "replication",
    "requestPayment",
    "response-cache-control",
    "response-content-disposition",
    "response-content-encoding",
    "response-content-language",
    "response-content-type",
    "response-expires",
    "restore",
    "select",
    "select-type",
    "storageClass",
    "tagging",
    "torrent",
    "uploadId",
    "uploads",
    "versionId",
    "versioning",
    "versions",
    "website",
];

/// MinIO's list (what MinIO-compatible clients sign): differs in a few newer subresources.
const SUBRESOURCES_MINIO: &[&str] = &[
    "acl",
    "cors",
    "delete",
    "encryption",
    "legal-hold",
    "lifecycle",
    "location",
    "logging",
    "notification",
    "partNumber",
    "policy",
    "requestPayment",
    "response-cache-control",
    "response-content-disposition",
    "response-content-encoding",
    "response-content-language",
    "response-content-type",
    "response-expires",
    "retention",
    "select",
    "select-type",
    "tagging",
    "torrent",
    "uploadId",
    "uploads",
    "versionId",
    "versioning",
    "versions",
    "website",
];

/// A SigV2-signed request, parsed (not yet verified).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedV2 {
    pub access_key: String,
    /// Base64, as sent.
    pub signature: String,
    /// Presigned URLs: `Expires` (Unix seconds).
    pub expires: Option<i64>,
    /// Header-signed requests: the request time (Unix seconds).
    pub timestamp: i64,
}

fn denied(why: &str) -> S3Error {
    S3Error::msg(Code::AccessDenied, why)
}

/// An HTTP date as SigV2 clients send it: RFC 1123 with `GMT` or a numeric zone (`+0000`, as in
/// AWS's examples and s3cmd), RFC 850, asctime, or SigV4's `YYYYMMDDTHHMMSSZ`.
pub fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some(t) = time::parse_http_date(s).or_else(|| time::parse_amz_date(s)) {
        return Some(t);
    }
    let rest = s.split_once(',').map_or(s, |(_, r)| r);
    let parts: Vec<&str> = rest.split_whitespace().collect();
    let [day, mon, year, hms, zone] = parts.as_slice() else { return None };
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let m = MONTHS.iter().position(|x| x.eq_ignore_ascii_case(mon))? as u32 + 1;
    let num = |v: &str| -> Option<i64> { (!v.is_empty() && v.bytes().all(|c| c.is_ascii_digit())).then(|| v.parse().ok()).flatten() };
    let (d, y) = (num(day)?, num(year)?);
    let mut t = hms.split(':');
    let (h, mi, sec) = (num(t.next()?)?, num(t.next()?)?, num(t.next()?)?);
    if t.next().is_some() || !(1..=31).contains(&d) || !(1970..=9999).contains(&y) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let offset = match *zone {
        z if ["GMT", "UTC", "UT", "Z"].iter().any(|g| g.eq_ignore_ascii_case(z)) => 0,
        z if z.len() == 5 && (z.starts_with('+') || z.starts_with('-')) => {
            let v = num(&z[1..])?;
            let secs = (v / 100) * 3600 + (v % 100) * 60;
            if z.starts_with('-') { -secs } else { secs }
        }
        _ => return None,
    };
    Some(time::days_from_civil(y, m, d as u32) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// Parse a SigV2 request: `None` if it isn't one. Doesn't check the clock or the signature.
pub fn parse(req: &Request) -> Result<Option<SignedV2>, S3Error> {
    if let Some(auth) = req.header("authorization") {
        let Some(rest) = auth.trim().strip_prefix("AWS ") else { return Ok(None) };
        let (ak, sig) = rest.trim().split_once(':').filter(|(a, s)| !a.trim().is_empty() && !s.trim().is_empty()).ok_or_else(|| {
            S3Error::msg(Code::InvalidArgument, "AWS authorization header is invalid.  Expected AwsAccessKeyId:signature").with("ArgumentName", "Authorization")
        })?;
        let date = req.header("x-amz-date").or_else(|| req.header("date")).ok_or_else(|| denied("AWS authentication requires a valid Date or x-amz-date header"))?;
        let timestamp = parse_date(date).ok_or_else(|| denied("AWS authentication requires a valid Date or x-amz-date header"))?;
        return Ok(Some(SignedV2 { access_key: ak.trim().to_string(), signature: sig.trim().to_string(), expires: None, timestamp }));
    }
    let query = crate::http::parse_query(req.query);
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if q("AWSAccessKeyId").is_none() && q("Signature").is_none() {
        return Ok(None);
    }
    let missing = || denied("Query-string authentication requires the Signature, Expires and AWSAccessKeyId parameters");
    let (Some(ak), Some(sig), Some(exp)) = (q("AWSAccessKeyId"), q("Signature"), q("Expires")) else { return Err(missing()) };
    if ak.is_empty() || sig.is_empty() {
        return Err(missing());
    }
    let expires: i64 = exp.trim().parse().map_err(|_| denied("Invalid date (should be seconds since epoch): ").with("Expires", exp.to_string()))?;
    Ok(Some(SignedV2 { access_key: ak.to_string(), signature: sig.to_string(), expires: Some(expires), timestamp: expires }))
}

/// Header-signed requests within 15 minutes of `now`; presigned URLs not past `Expires`.
pub fn check_time(s: &SignedV2, now: i64) -> Result<(), S3Error> {
    match s.expires {
        Some(e) if now > e => Err(denied("Request has expired").with("Expires", time::iso8601_ms(e.max(0).saturating_mul(1000))).with("ServerTime", time::iso8601_ms(now * 1000))),
        Some(_) => Ok(()),
        None if s.timestamp.saturating_sub(now).saturating_abs() > MAX_SKEW_S => Err(S3Error::new(Code::RequestTimeTooSkewed)
            .with("RequestTime", time::http_date(s.timestamp.saturating_mul(1000)))
            .with("ServerTime", time::iso8601_ms(now * 1000))
            .with("MaxAllowedSkewMilliseconds", (MAX_SKEW_S * 1000).to_string())),
        None => Ok(()),
    }
}

/// `CanonicalizedAmzHeaders`; `skip_amz_date` leaves `x-amz-date` out.
pub fn canonical_amz_headers(req: &Request, skip_amz_date: bool) -> String {
    let mut names: Vec<String> = req.headers.iter().map(|(k, _)| k.to_ascii_lowercase()).filter(|k| k.starts_with("x-amz-") && !(skip_amz_date && k == "x-amz-date")).collect();
    names.sort();
    names.dedup();
    let mut out = String::new();
    for n in names {
        let values: Vec<String> = req.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(&n)).map(|(_, v)| v.trim().to_string()).collect();
        out.push_str(&n);
        out.push(':');
        out.push_str(&values.join(","));
        out.push('\n');
    }
    out
}

/// `CanonicalizedResource`. `vhost_bucket`: the bucket when it came from the `Host` header.
/// `bare_empty`: a subresource sent as `name=` is signed as `name` (MinIO) instead of `name=`
/// (botocore).
pub fn canonical_resource(req: &Request, vhost_bucket: Option<&str>, subresources: &[&str], bare_empty: bool) -> String {
    let mut out = String::new();
    if let Some(b) = vhost_bucket {
        out.push('/');
        out.push_str(b);
    }
    out.push_str(if req.path.is_empty() { "/" } else { req.path });
    let mut subs: Vec<(String, Option<String>)> = req
        .query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = match p.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (p, None),
            };
            let k = String::from_utf8_lossy(&crate::http::percent_decode(k)).into_owned();
            subresources.contains(&k.as_str()).then(|| (k, v.map(|v| String::from_utf8_lossy(&crate::http::percent_decode(v)).into_owned())))
        })
        .collect();
    subs.sort_by(|a, b| a.0.cmp(&b.0));
    for (i, (k, v)) in subs.iter().enumerate() {
        out.push(if i == 0 { '?' } else { '&' });
        out.push_str(k);
        match v {
            Some(v) if !(bare_empty && v.is_empty()) => {
                out.push('=');
                out.push_str(v);
            }
            _ => {}
        }
    }
    out
}

/// The string to sign, from its parts.
pub fn string_to_sign(req: &Request, date_line: &str, amz_headers: &str, resource: &str) -> String {
    let h = |n: &str| req.header(n).map(str::trim).unwrap_or_default();
    format!("{}\n{}\n{}\n{date_line}\n{amz_headers}{resource}", req.method, h("content-md5"), h("content-type"))
}

/// Every string to sign a client could have used for this request, most standard first.
pub fn candidates(req: &Request, s: &SignedV2, vhost_bucket: Option<&str>) -> Vec<String> {
    let mut dates: Vec<(String, bool)> = Vec::new();
    match (s.expires, req.header("x-amz-date")) {
        (Some(e), _) => dates.push((e.to_string(), false)),
        (None, Some(amz)) => {
            dates.push((String::new(), false));
            if let Some(d) = req.header("date") {
                dates.push((d.trim().to_string(), false));
            }
            dates.push((amz.trim().to_string(), true));
        }
        (None, None) => dates.push((req.header("date").unwrap_or_default().trim().to_string(), false)),
    }
    let mut resources = vec![canonical_resource(req, vhost_bucket, SUBRESOURCES, false), canonical_resource(req, vhost_bucket, SUBRESOURCES_MINIO, true)];
    // botocore signs a path-style bucket request (`/bucket`) as `/bucket/`.
    let bare_bucket = vhost_bucket.is_none() && req.path.len() > 1 && !req.path[1..].contains('/');
    if bare_bucket {
        let slashed = Request { path: &format!("{}/", req.path), ..*req };
        resources.push(canonical_resource(&slashed, None, SUBRESOURCES, false));
    }
    let mut out: Vec<String> = Vec::new();
    for (date, skip) in &dates {
        let amz = canonical_amz_headers(req, *skip);
        for r in &resources {
            let sts = string_to_sign(req, date, &amz, r);
            if !out.contains(&sts) {
                out.push(sts);
            }
        }
    }
    out
}

/// Base64 HMAC-SHA1 of `data` with the secret.
pub fn sign(secret: &str, data: &str) -> String {
    let mut m = Hmac::<Sha1>::new_from_slice(secret.as_bytes()).expect("hmac takes any key length");
    m.update(data.as_bytes());
    crate::checksum::b64(&m.finalize().into_bytes())
}

/// Check the signature against each variant of the string to sign.
pub fn verify(req: &Request, s: &SignedV2, secret: &str, vhost_bucket: Option<&str>) -> Result<Verified, S3Error> {
    let all = candidates(req, s, vhost_bucket);
    if !all.iter().any(|sts| constant_time_eq(&sign(secret, sts), &s.signature)) {
        return Err(S3Error::new(Code::SignatureDoesNotMatch)
            .with("AWSAccessKeyId", s.access_key.clone())
            .with("StringToSign", all.first().cloned().unwrap_or_default())
            .with("SignatureProvided", s.signature.clone()));
    }
    Ok(Verified { access_key: s.access_key.clone(), payload: Payload::Unsigned, stream: None })
}

/// Everything in one call: `Ok(None)` if the request isn't SigV2-signed.
pub fn authenticate(req: &Request, now: i64, vhost_bucket: Option<&str>, lookup: impl FnOnce(&str) -> Option<String>) -> Result<Option<Verified>, S3Error> {
    let Some(s) = parse(req)? else { return Ok(None) };
    let secret = lookup(&s.access_key).ok_or_else(|| S3Error::new(Code::InvalidAccessKeyId).with("AWSAccessKeyId", s.access_key.clone()))?;
    check_time(&s, now)?;
    verify(req, &s, &secret, vhost_bucket).map(Some)
}

/// Sign a request the documented way (clients and tests): the `Authorization` value.
pub fn authorization(req: &Request, access_key: &str, secret: &str, vhost_bucket: Option<&str>) -> String {
    let date = if req.header("x-amz-date").is_some() { "" } else { req.header("date").unwrap_or_default().trim() };
    let sts = string_to_sign(req, date, &canonical_amz_headers(req, false), &canonical_resource(req, vhost_bucket, SUBRESOURCES, false));
    format!("AWS {access_key}:{}", sign(secret, &sts))
}

/// The query parameters of a presigned URL (clients and tests), to append to `req.query`.
pub fn presign_query(req: &Request, access_key: &str, secret: &str, expires: i64, vhost_bucket: Option<&str>) -> String {
    let sts = string_to_sign(req, &expires.to_string(), &canonical_amz_headers(req, false), &canonical_resource(req, vhost_bucket, SUBRESOURCES, false));
    let sig = sign(secret, &sts);
    format!("AWSAccessKeyId={}&Expires={expires}&Signature={}", crate::http::uri_encode(access_key, true), crate::http::uri_encode(&sig, true))
}

/// The SigV2 fields of a POST form: `(AWSAccessKeyId, policy, signature)`, `None` without an
/// `AWSAccessKeyId`.
pub fn parse_post_auth(fields: &[(String, String)]) -> Result<Option<(String, String, String)>, S3Error> {
    let field = |n: &str| fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
    let Some(ak) = field("AWSAccessKeyId") else { return Ok(None) };
    let bad = |what: &str| S3Error::msg(Code::InvalidArgument, format!("Bucket POST must contain a field named '{what}'.  If it is specified, please check the order of the fields.")).with("ArgumentName", what.to_string());
    let policy = field("policy").ok_or_else(|| bad("policy"))?;
    let signature = field("signature").ok_or_else(|| bad("signature"))?;
    Ok(Some((ak, policy, signature)))
}

/// Check a SigV2 POST form's signature (base64 HMAC-SHA1 of the base64 policy).
pub fn verify_post_signature(access_key: &str, policy: &str, signature: &str, secret: &str) -> Result<(), S3Error> {
    if constant_time_eq(&sign(secret, policy), signature.trim()) {
        Ok(())
    } else {
        Err(S3Error::new(Code::SignatureDoesNotMatch).with("AWSAccessKeyId", access_key.to_string()).with("StringToSign", policy.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    /// Verify a request signed with `Authorization: AWS AK:<sig>` at its own time.
    fn check(method: &str, path: &str, query: &str, headers: &[(&str, &str)], vhost: Option<&str>, sig: &str, sts: &str) {
        let auth = format!("AWS {AK}:{sig}");
        let mut h = headers.to_vec();
        h.push(("Authorization", &auth));
        let req = Request { method, path, query, headers: &h };
        let s = parse(&req).unwrap().unwrap();
        assert_eq!((s.access_key.as_str(), s.signature.as_str()), (AK, sig));
        assert_eq!(candidates(&req, &s, vhost)[0], sts);
        assert_eq!(sign(SK, sts), sig, "the documented signature of the documented string");
        verify(&req, &s, SK, vhost).unwrap();
        authenticate(&req, s.timestamp + 60, vhost, |k| (k == AK).then(|| SK.to_string())).unwrap().unwrap();
        assert_eq!(authenticate(&req, s.timestamp, vhost, |_| Some("wrong".into())).unwrap_err().code, Code::SignatureDoesNotMatch);
        assert_eq!(authenticate(&req, s.timestamp + 3600, vhost, |_| Some(SK.into())).unwrap_err().code, Code::RequestTimeTooSkewed);
        assert_eq!(authenticate(&req, s.timestamp, vhost, |_| None).unwrap_err().code, Code::InvalidAccessKeyId);
        assert_eq!(verify(&Request { method: if method == "GET" { "HEAD" } else { "GET" }, ..req }, &s, SK, vhost).unwrap_err().code, Code::SignatureDoesNotMatch);
    }

    // AWS's "Signing and authenticating REST requests" (Signature Version 2) examples.
    #[test]
    fn aws_example_object_get() {
        let h = [("Host", "awsexamplebucket1.us-west-1.s3.amazonaws.com"), ("Date", "Tue, 27 Mar 2007 19:36:42 +0000")];
        check("GET", "/photos/puppy.jpg", "", &h, Some("awsexamplebucket1"), "qgk2+6Sv9/oM7G3qLEjTH1a1l1g=", "GET\n\n\nTue, 27 Mar 2007 19:36:42 +0000\n/awsexamplebucket1/photos/puppy.jpg");
        // The same object path-style: the same resource.
        check("GET", "/awsexamplebucket1/photos/puppy.jpg", "", &[("Host", "s3.us-west-1.amazonaws.com"), h[1]], None, "qgk2+6Sv9/oM7G3qLEjTH1a1l1g=", "GET\n\n\nTue, 27 Mar 2007 19:36:42 +0000\n/awsexamplebucket1/photos/puppy.jpg");
    }

    #[test]
    fn aws_example_object_put() {
        let h = [("Content-Type", "image/jpeg"), ("Content-Length", "94328"), ("Host", "awsexamplebucket1.s3.us-west-1.amazonaws.com"), ("Date", "Tue, 27 Mar 2007 21:15:45 +0000")];
        check("PUT", "/photos/puppy.jpg", "", &h, Some("awsexamplebucket1"), "iqRzw+ileNPu1fhspnRs8nOjjIA=", "PUT\n\nimage/jpeg\nTue, 27 Mar 2007 21:15:45 +0000\n/awsexamplebucket1/photos/puppy.jpg");
    }

    #[test]
    fn aws_example_list_and_acl() {
        let h = [("User-Agent", "Mozilla/5.0"), ("Host", "awsexamplebucket1.s3.us-west-1.amazonaws.com"), ("Date", "Tue, 27 Mar 2007 19:42:41 +0000")];
        check("GET", "/", "prefix=photos&max-keys=50&marker=puppy", &h, Some("awsexamplebucket1"), "m0WP8eCtspQl5Ahe6L1SozdX9YA=", "GET\n\n\nTue, 27 Mar 2007 19:42:41 +0000\n/awsexamplebucket1/");
        let h = [("Host", "awsexamplebucket1.s3.us-west-1.amazonaws.com"), ("Date", "Tue, 27 Mar 2007 19:44:46 +0000")];
        check("GET", "/", "acl", &h, Some("awsexamplebucket1"), "82ZHiFIjc+WbcwFKGUVEQspPn+0=", "GET\n\n\nTue, 27 Mar 2007 19:44:46 +0000\n/awsexamplebucket1/?acl");
    }

    #[test]
    fn aws_example_delete_with_x_amz_date() {
        // AWS's example signs the x-amz-date value in the date position (no amz header line).
        let h = [("User-Agent", "dotnet"), ("Host", "s3.us-west-1.amazonaws.com"), ("Date", "Tue, 27 Mar 2007 21:20:27 +0000"), ("x-amz-date", "Tue, 27 Mar 2007 21:20:26 +0000")];
        let auth = format!("AWS {AK}:XbyTlbQdu9Xw5o8P4iMwPktxQd8=");
        let mut with = h.to_vec();
        with.push(("Authorization", &auth));
        let req = Request { method: "DELETE", path: "/awsexamplebucket1/photos/puppy.jpg", query: "", headers: &with };
        let s = parse(&req).unwrap().unwrap();
        assert_eq!(s.timestamp, parse_date("Tue, 27 Mar 2007 21:20:26 +0000").unwrap(), "x-amz-date wins over Date");
        let all = candidates(&req, &s, None);
        assert_eq!(all[0], "DELETE\n\n\n\nx-amz-date:Tue, 27 Mar 2007 21:20:26 +0000\n/awsexamplebucket1/photos/puppy.jpg", "the documented rule first");
        assert!(all.contains(&"DELETE\n\n\nTue, 27 Mar 2007 21:20:26 +0000\n/awsexamplebucket1/photos/puppy.jpg".to_string()));
        verify(&req, &s, SK, None).unwrap();
        // The documented rule (empty date line), as s3cmd signs.
        let doc = authorization(&Request { headers: &h, ..req }, AK, SK, None);
        let mut with = h.to_vec();
        with.push(("Authorization", &doc));
        verify(&Request { headers: &with, ..req }, &parse(&Request { headers: &with, ..req }).unwrap().unwrap(), SK, None).unwrap();
    }

    #[test]
    fn aws_example_upload_with_amz_headers() {
        let h = [
            ("User-Agent", "curl/7.15.5"),
            ("Host", "static.awsexamplebucket1.net:8080"),
            ("Date", "Tue, 27 Mar 2007 21:06:08 +0000"),
            ("x-amz-acl", "public-read"),
            ("content-type", "application/x-download"),
            ("Content-MD5", "4gJE4saaMU4BqNR0kLY+lw=="),
            ("X-Amz-Meta-ReviewedBy", "joe@awsexamplebucket1.net"),
            ("X-Amz-Meta-ReviewedBy", "jane@awsexamplebucket1.net"),
            ("X-Amz-Meta-FileChecksum", "0x02661779"),
            ("X-Amz-Meta-ChecksumAlgorithm", "crc32"),
            ("Content-Disposition", "attachment; filename=database.dat"),
            ("Content-Encoding", "gzip"),
            ("Content-Length", "5913339"),
        ];
        check(
            "PUT",
            "/db-backup.dat.gz",
            "",
            &h,
            Some("static.awsexamplebucket1.net"),
            "dKZcB+bz2EPXgSdXZp9ozGeOM4I=",
            "PUT\n4gJE4saaMU4BqNR0kLY+lw==\napplication/x-download\nTue, 27 Mar 2007 21:06:08 +0000\nx-amz-acl:public-read\nx-amz-meta-checksumalgorithm:crc32\nx-amz-meta-filechecksum:0x02661779\nx-amz-meta-reviewedby:joe@awsexamplebucket1.net,jane@awsexamplebucket1.net\n/static.awsexamplebucket1.net/db-backup.dat.gz",
        );
    }

    #[test]
    fn aws_example_service_and_unicode() {
        check("GET", "/", "", &[("Host", "s3.us-west-1.amazonaws.com"), ("Date", "Wed, 28 Mar 2007 01:29:59 +0000")], None, "qGdzdERIC03wnaRNKh6OqZehG9s=", "GET\n\n\nWed, 28 Mar 2007 01:29:59 +0000\n/");
        check(
            "GET",
            "/dictionary/fran%C3%A7ais/pr%c3%a9f%c3%a8re",
            "",
            &[("Host", "s3.us-west-1.amazonaws.com"), ("Date", "Wed, 28 Mar 2007 01:49:49 +0000")],
            None,
            "DNEZGsoieTZ92F3bUfSPQcbGmlM=",
            "GET\n\n\nWed, 28 Mar 2007 01:49:49 +0000\n/dictionary/fran%C3%A7ais/pr%c3%a9f%c3%a8re",
        );
    }

    #[test]
    fn aws_example_presigned() {
        // The original example (bucket johnsmith): Expires replaces the date.
        let headers = [("Host", "johnsmith.s3.amazonaws.com")];
        let query = "AWSAccessKeyId=AKIAIOSFODNN7EXAMPLE&Signature=NpgCjnDzrM%2BWFzoENXmpNDUsSn8%3D&Expires=1175139620";
        let req = Request { method: "GET", path: "/photos/puppy.jpg", query, headers: &headers };
        let s = parse(&req).unwrap().unwrap();
        assert_eq!((s.expires, s.signature.as_str()), (Some(1_175_139_620), "NpgCjnDzrM+WFzoENXmpNDUsSn8="));
        assert_eq!(candidates(&req, &s, Some("johnsmith"))[0], "GET\n\n\n1175139620\n/johnsmith/photos/puppy.jpg");
        authenticate(&req, 1_175_139_620, Some("johnsmith"), |_| Some(SK.into())).unwrap().unwrap();
        assert_eq!(authenticate(&req, 1_175_139_621, Some("johnsmith"), |_| Some(SK.into())).unwrap_err().code, Code::AccessDenied, "expired");
        // Long-lived URLs are fine (SigV2 has no 7-day cap).
        authenticate(&req, 0, Some("johnsmith"), |_| Some(SK.into())).unwrap().unwrap();
        // Round trip with the helper, with a subresource.
        let base = Request { method: "GET", path: "/b/k", query: "response-content-type=text%2Fplain", headers: &[("host", "h")] };
        let q = format!("{}&{}", base.query, presign_query(&base, AK, SK, 2_000_000_000, None));
        let req = Request { query: &q, ..base };
        authenticate(&req, 1_999_999_999, None, |_| Some(SK.into())).unwrap().unwrap();
        let other = q.replace("text%2Fplain", "text%2Fhtml");
        assert_eq!(authenticate(&Request { query: &other, ..req }, 0, None, |_| Some(SK.into())).unwrap_err().code, Code::SignatureDoesNotMatch, "subresources are signed");
    }

    #[test]
    fn resources_and_headers() {
        let r = |path: &str, query: &str, vhost: Option<&str>, list: &[&str], bare: bool| canonical_resource(&Request { method: "GET", path, query, headers: &[] }, vhost, list, bare);
        assert_eq!(r("/b/k", "versionId=1&uploads&prefix=x&acl=", None, SUBRESOURCES, false), "/b/k?acl=&uploads&versionId=1");
        assert_eq!(r("/b/k", "acl=&tagging", None, SUBRESOURCES_MINIO, true), "/b/k?acl&tagging");
        assert_eq!(r("/k", "response-content-disposition=attachment%3B%20x", Some("b"), SUBRESOURCES, false), "/b/k?response-content-disposition=attachment; x");
        assert_eq!(r("", "", Some("b"), SUBRESOURCES, false), "/b/");
        assert_eq!(r("/", "uploadId=abc&partNumber=2", None, SUBRESOURCES, false), "/?partNumber=2&uploadId=abc");
        let h = [("X-Amz-B", " two  words "), ("x-amz-a", "1"), ("X-AMZ-A", "2"), ("x-other", "no")];
        assert_eq!(canonical_amz_headers(&Request { method: "GET", path: "/", query: "", headers: &h }, false), "x-amz-a:1,2\nx-amz-b:two  words\n");
    }

    #[test]
    fn dates_and_parse_errors() {
        assert_eq!(parse_date("Tue, 27 Mar 2007 19:36:42 +0000"), parse_date("Tue, 27 Mar 2007 19:36:42 GMT"));
        assert_eq!(parse_date("Tue, 27 Mar 2007 21:36:42 +0200"), parse_date("Tue, 27 Mar 2007 19:36:42 GMT"));
        assert_eq!(parse_date("Tue, 27 Mar 2007 17:36:42 -0200"), parse_date("Tue, 27 Mar 2007 19:36:42 GMT"));
        assert_eq!(parse_date("20070327T193642Z"), parse_date("Tue, 27 Mar 2007 19:36:42 GMT"));
        for bad in ["", "yesterday", "Tue, 27 Foo 2007 19:36:42 GMT", "Tue, 27 Mar 2007 19:36 GMT", "Tue, 27 Mar 2007 19:36:42 +00", "Tue, 32 Mar 2007 19:36:42 GMT"] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
        let p = |h: &[(&str, &str)], q: &str| parse(&Request { method: "GET", path: "/", query: q, headers: h }).map(|o| o.is_some()).map_err(|e| e.code);
        assert_eq!(p(&[("authorization", "AWS nocolon"), ("date", "Tue, 27 Mar 2007 19:36:42 GMT")], ""), Err(Code::InvalidArgument));
        assert_eq!(p(&[("authorization", "AWS :sig"), ("date", "Tue, 27 Mar 2007 19:36:42 GMT")], ""), Err(Code::InvalidArgument));
        assert_eq!(p(&[("authorization", "AWS ak:sig")], ""), Err(Code::AccessDenied), "no date");
        assert_eq!(p(&[("authorization", "AWS ak:sig"), ("date", "soon")], ""), Err(Code::AccessDenied));
        assert_eq!(p(&[("authorization", "AWS4-HMAC-SHA256 Credential=x")], ""), Ok(false), "not SigV2");
        assert_eq!(p(&[], "AWSAccessKeyId=a&Signature=b"), Err(Code::AccessDenied), "no Expires");
        assert_eq!(p(&[], "AWSAccessKeyId=a&Expires=1"), Err(Code::AccessDenied));
        assert_eq!(p(&[], "AWSAccessKeyId=a&Signature=b&Expires=later"), Err(Code::AccessDenied));
        assert_eq!(p(&[], "Signature=b&Expires=1"), Err(Code::AccessDenied));
        assert_eq!(p(&[], "prefix=a"), Ok(false));
        assert_eq!(parse_date("Tue, 27 Mar 99999999999 19:36:42 +0000"), None, "years beyond 9999");
    }

    #[test]
    fn botocore_signs_path_style_buckets_with_a_slash() {
        let headers = [("date", "Thu, 22 Oct 2026 10:34:12 GMT")];
        let signed_as = Request { method: "PUT", path: "/v2bucket/", query: "", headers: &headers };
        let auth = authorization(&signed_as, "AK", "secret", None);
        let sent = Request { method: "PUT", path: "/v2bucket", query: "", headers: &headers };
        let s = SignedV2 { access_key: "AK".into(), signature: auth.rsplit(':').next().unwrap().into(), expires: None, timestamp: 0 };
        assert!(verify(&sent, &s, "secret", None).is_ok());
        // Only a bare bucket: an object's path is signed as sent.
        let obj = Request { method: "PUT", path: "/v2bucket/key/", query: "", headers: &headers };
        let s = SignedV2 { signature: authorization(&obj, "AK", "secret", None).rsplit(':').next().unwrap().into(), ..s };
        assert!(verify(&Request { path: "/v2bucket/key", ..obj }, &s, "secret", None).is_err());
    }

    #[test]
    fn extreme_times_are_errors_not_panics() {
        let at = |timestamp: i64, expires: Option<i64>| check_time(&SignedV2 { access_key: "a".into(), signature: "s".into(), expires, timestamp }, 1_700_000_000).map_err(|e| e.code);
        // A `Date` before the epoch (s3-tests sends 1950) isn't a valid date: AccessDenied.
        assert_eq!(parse_date("Tue, 07 Jul 1950 21:41:00 GMT"), None);
        let p = |date: &str| parse(&Request { method: "PUT", path: "/b", query: "", headers: &[("authorization", "AWS ak:sig"), ("x-amz-date", date)] }).map(|_| ()).map_err(|e| e.code);
        assert_eq!(p("Tue, 07 Jul 1950 21:53:04 GMT"), Err(Code::AccessDenied));
        assert_eq!(p("Tue, 07 Jul 1970 21:53:04 GMT"), Ok(()));
        assert_eq!(at(-1_000_000_000, None), Err(Code::RequestTimeTooSkewed));
        assert_eq!(at(i64::MIN, None), Err(Code::RequestTimeTooSkewed));
        assert_eq!(at(i64::MAX, None), Err(Code::RequestTimeTooSkewed));
        assert_eq!(at(i64::MIN, Some(i64::MIN)), Err(Code::AccessDenied));
        assert_eq!(at(0, Some(i64::MAX)), Ok(()));
    }

    #[test]
    fn post_policy_v2() {
        let policy = crate::checksum::b64(br#"{"expiration":"2030-01-01T00:00:00Z","conditions":[{"bucket":"b"},["starts-with","$key",""]]}"#);
        let sig = sign(SK, &policy);
        let fields: Vec<(String, String)> = [("key", "k"), ("AWSAccessKeyId", AK), ("policy", policy.as_str()), ("signature", sig.as_str())].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let (ak, p, s) = parse_post_auth(&fields).unwrap().unwrap();
        verify_post_signature(&ak, &p, &s, SK).unwrap();
        assert_eq!(verify_post_signature(&ak, &p, &s, "wrong").unwrap_err().code, Code::SignatureDoesNotMatch);
        // The SigV2 fields need no policy condition of their own.
        crate::sigv4::check_post_policy(&policy, &fields, "b", 0).unwrap();
        assert_eq!(parse_post_auth(&fields[..1]).unwrap(), None);
        assert_eq!(parse_post_auth(&fields[..2]).unwrap_err().code, Code::InvalidArgument);
    }
}
