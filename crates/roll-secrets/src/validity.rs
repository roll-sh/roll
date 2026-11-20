//! Validity checks (docs/secrets.md, "Is it live?"), as GitGuardian's: whether a credential found
//! in code (roll code's `secret_unstored` notices, `roll secrets scan`) still works, asked of the
//! provider that issued it. Off unless a project's admin turns `validity_checks` on, because it
//! contacts the provider.
//!
//! - **Nothing trips a canary.** An AWS key is never tried by default: `GetCallerIdentity` is what
//!   canary tokens (canarytokens.org's, GitGuardian's) alert on. Its account is read from the key
//!   id offline (`roll_auth::credentials::aws_account`) and the answer is `unknown` with that
//!   account. Only an operator turns live AWS checks on ([`Settings::aws_live`]), and even then a
//!   key whose account is a known canary's ([`Canaries`]: a bundled list plus the operator's) is
//!   never tried. The operator's list can also name any credential by fingerprint.
//! - **roll's own tokens aren't tried either.** An `rlt_` or `rlo_` is looked up by its hash in
//!   roll-access ([`crate::Workloads::token_status`]), which says live or revoked without using it.
//!   A decoy (docs/secrets.md, "Decoys") is never reported: finding one is the decoy working, so its
//!   answer is the one any roll secret roll can't look up gets, and nothing is kept or audited.
//! - **Fixed hosts.** The other kinds have one request each to one host of their provider, built
//!   here from constants: GitHub's `GET /user`, Stripe's `GET /v1/balance`, Slack's `auth.test`,
//!   OpenAI's and Anthropic's models lists (read-only, free), and with live AWS checks on, STS's
//!   `GetCallerIdentity` signed with the found pair. Nothing chooses the URL; requests go through
//!   rolld's guarded client ([`Deliver::request`]: HTTPS to public addresses, no redirects, a
//!   timeout, the answer bounded). The request carries the credential and the headers the API
//!   needs to read it, nothing else.
//! - **Once per credential.** Results are kept per (project, fingerprint) for a day (an `unknown`
//!   for an hour), and a project gets at most [`PER_HOUR`] checks an hour. A check is claimed in a
//!   write first (`claimed_at`), so two callers or two nodes never ask twice; the request goes out
//!   after that write and its answer is stored, with its `secrets.validity` audit row (kind,
//!   fingerprint, result), in another. A roll token's lookup is kept and audited the same way (an
//!   hour, outside the rate limit); an AWS account read offline asks nobody and isn't kept.
//! - **Never the value.** A credential is a [`Value`] (redacted, zeroized) from the request to the
//!   provider; it isn't stored, logged, audited or put in an error. Only its fingerprint
//!   (`roll_auth::credentials::fingerprint`, what the notice and the scan already show) and the
//!   provider's account id (an AWS account, a GitHub login, a Slack team) are kept.

use crate::access::{Action, Caller, Scope};
use crate::sinks::Deliver;
use crate::{Error, Result, Secrets, Value, audit};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS validity (
    project_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    kind TEXT NOT NULL,
    result TEXT,
    account TEXT,
    checked_at INTEGER,
    why TEXT,
    claimed_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, fingerprint)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS validity_claims ON validity (project_id, claimed_at);
";

/// How long a `live` or `invalid` answer is kept, and an `unknown` one.
pub const TTL_MS: i64 = 24 * 3_600_000;
pub const UNKNOWN_TTL_MS: i64 = 3_600_000;
/// A claim older than this is someone's that never finished: it may be claimed again.
const CLAIM_MS: i64 = 60_000;
/// The most checks a project's credentials get an hour.
pub const PER_HOUR: i64 = 100;
/// The most credentials one call asks about.
pub const MAX_PROBES: usize = 200;
/// The most of a provider's answer read.
const ANSWER_MAX: usize = 64 * 1024;
/// How long a caller waits for another's check of the same credential.
const WAIT_MS: u64 = 20_000;

/// The operator's validity settings (rolld: `ROLL_SECRETS_VALIDITY_AWS_LIVE`,
/// `ROLL_SECRETS_VALIDITY_CANARIES`).
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Try AWS keys with STS (`GetCallerIdentity`). Off: an AWS key's account is read from its id
    /// and nothing is sent, since that call is what AWS canary tokens alert on.
    pub aws_live: bool,
    /// Credentials never tried, beyond the bundled AWS canary accounts.
    pub canaries: Canaries,
}

/// AWS accounts canarytokens.org's AWS tokens and their look-alikes are issued from, as public
/// secret scanners list them: a key of one of these is a canary, and trying it would set it off.
const AWS_CANARY_ACCOUNTS: [&str; 26] = [
    "052310077262", "171436882533", "534261010715", "595918472158", "717712589309", "819147034852", "992382622183", "730335385048", "266735846894", "893192397702",
    "044858866125", "251535659677", "344043088457", "351906852752", "390477818340", "426127672474", "427150556519", "439872796651", "445142720921", "465867158099",
    "637958123769", "693412236332", "732624840810", "735421457923", "959235150393", "982842642351",
];

/// Credentials that are someone's canary: never tried. Any provider's accounts
/// (`aws:123456789012`, `github:<login>`; checked wherever the account is known before asking) and
/// any credential by its fingerprint (`sha256:…`, as the notice and the scan show it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Canaries {
    pub accounts: std::collections::BTreeSet<(String, String)>,
    pub fingerprints: std::collections::BTreeSet<String>,
}

impl Canaries {
    /// `aws:123456789012, sha256:3f7c0a9e1b2d4c6e8f0a1b2c, …` (commas or whitespace between).
    pub fn parse(spec: &str) -> std::result::Result<Canaries, String> {
        let mut c = Canaries::default();
        for entry in spec.split(|ch: char| ch == ',' || ch.is_whitespace()).filter(|e| !e.is_empty()) {
            match entry.split_once(':') {
                Some(("sha256", hex)) if hex.len() == 24 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                    c.fingerprints.insert(format!("sha256:{}", hex.to_ascii_lowercase()));
                }
                Some((provider, account)) if !provider.is_empty() && !account.is_empty() && provider != "sha256" && provider.bytes().all(|b| b.is_ascii_lowercase()) => {
                    c.accounts.insert((provider.to_string(), account.to_string()));
                }
                _ => return Err(format!("{entry}: a canary is provider:account (aws:123456789012) or a fingerprint (sha256: and 24 hex digits)")),
            }
        }
        Ok(c)
    }

    /// Whether `provider`'s `account` is a canary's (the bundled AWS list, or the operator's).
    pub fn account(&self, provider: &str, account: &str) -> bool {
        (provider == "aws" && AWS_CANARY_ACCOUNTS.contains(&account)) || self.accounts.contains(&(provider.to_string(), account.to_string()))
    }
}

/// What a roll token found in code is, as roll-access says without using it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenStatus {
    Live,
    /// Revoked, expired, or never roll's.
    Gone,
    /// One of roll's decoys: never reported.
    Decoy,
}

/// What the provider said.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Validity {
    /// It works: the provider answered as its owner.
    Live,
    /// The provider refused it: revoked, deleted or never real.
    Invalid,
    /// Not known: not checked (off, rate limited, a kind roll can't check, no pair), or the
    /// provider's answer said neither.
    Unknown,
}

impl Validity {
    pub fn as_str(self) -> &'static str {
        match self {
            Validity::Live => "live",
            Validity::Invalid => "invalid",
            Validity::Unknown => "unknown",
        }
    }

    fn parse(s: &str) -> Validity {
        match s {
            "live" => Validity::Live,
            "invalid" => Validity::Invalid,
            _ => Validity::Unknown,
        }
    }
}

/// A credential to check: the found value and, for an AWS access key id, the secret access key
/// next to it (`roll_auth::credentials::aws_secret_near`). Redacted and zeroized.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub value: Value,
    #[serde(default)]
    pub pair: Option<Value>,
}

/// A credential's validity: never its value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Checked {
    pub fingerprint: String,
    /// What the detectors call it (`a GitHub token`), or empty when they don't recognise it.
    pub kind: String,
    pub result: Validity,
    /// The provider's account it belongs to, when the answer names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// When the provider answered (ms); none when it wasn't asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    /// Why it's `unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// A live credential's kind has a roll rotator to replace it with (`aws-iam`: roll rotates an
    /// IAM user's keys; `github-app`: short-lived tokens from the project's GitHub App).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotator: Option<&'static str>,
}

impl Checked {
    fn unknown(fingerprint: String, kind: &str, why: &str) -> Checked {
        Checked { fingerprint, kind: kind.into(), result: Validity::Unknown, account: None, checked_at: None, why: Some(why.into()), rotator: None }
    }

    fn answered(fingerprint: String, kind: &str, result: Validity, account: Option<String>, at: i64, why: Option<String>) -> Checked {
        let rotator = if result == Validity::Live { Checker::of(kind).and_then(Checker::rotator) } else { None };
        Checked { fingerprint, kind: kind.into(), result, account, checked_at: Some(at), why, rotator }
    }
}

/// The kinds roll can check, each with its one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checker {
    Aws,
    GitHub,
    Stripe,
    Slack,
    OpenAi,
    Anthropic,
    /// roll's own tokens: looked up, never used.
    Roll,
}

impl Checker {
    /// By the detectors' kind (`roll_auth::credentials`).
    fn of(kind: &str) -> Option<Checker> {
        Some(match kind {
            "an AWS access key" => Checker::Aws,
            "a GitHub token" => Checker::GitHub,
            "a Stripe live key" => Checker::Stripe,
            "a Slack token" => Checker::Slack,
            "an OpenAI API key" => Checker::OpenAi,
            "an Anthropic API key" => Checker::Anthropic,
            "a roll token" => Checker::Roll,
            _ => return None,
        })
    }

    fn rotator(self) -> Option<&'static str> {
        match self {
            Checker::Aws => Some("aws-iam"),
            Checker::GitHub => Some("github-app"),
            _ => None,
        }
    }
}

/// One request to a provider: method, URL, headers, and a body with its content type.
type Call = (&'static str, &'static str, Vec<(&'static str, Value)>, Option<(&'static str, Value)>);

/// The provider's answer: the result, its account, and why when it's unknown.
type Answer = (Validity, Option<String>, Option<String>);

/// An account id from an answer, kept only when it's plainly one (no markup, bounded).
fn account(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    (!s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.@:".contains(&b))).then(|| s.to_string())
}

fn unknown(why: &str) -> Answer {
    (Validity::Unknown, None, Some(why.into()))
}

/// Ask the provider. `secret` is the found value; `pair`, an AWS key id's secret access key.
fn ask(deliver: &dyn Deliver, checker: Checker, secret: &Value, pair: Option<&Value>, now: time::OffsetDateTime) -> Answer {
    let bearer = Value::new(format!("Bearer {}", secret.expose()));
    let json_accept = Value::new("application/json");
    let (method, url, headers, body): Call = match checker {
        Checker::Roll => return unknown("roll's own tokens are looked up, never tried"),
        Checker::Aws => {
            let Some(pair) = pair else { return unknown("no secret access key next to it to sign with") };
            let (auth, date) = sigv4_get_caller_identity(secret.expose(), pair.expose(), now);
            ("POST", STS_URL, vec![("authorization", auth), ("x-amz-date", Value::new(date))], Some((STS_CONTENT_TYPE, Value::new(crate::aws_identity::BODY))))
        }
        Checker::GitHub => ("GET", "https://api.github.com/user", vec![("authorization", bearer), ("accept", Value::new("application/vnd.github+json"))], None),
        Checker::Stripe => ("GET", "https://api.stripe.com/v1/balance", vec![("authorization", bearer)], None),
        Checker::Slack => ("POST", "https://slack.com/api/auth.test", vec![("authorization", bearer), ("accept", json_accept)], None),
        Checker::OpenAi => ("GET", "https://api.openai.com/v1/models", vec![("authorization", bearer)], None),
        Checker::Anthropic => ("GET", "https://api.anthropic.com/v1/models", vec![("x-api-key", secret.clone()), ("anthropic-version", Value::new("2023-06-01"))], None),
    };
    let headers: Vec<(&str, &Value)> = headers.iter().map(|(k, v)| (*k, v)).collect();
    let (status, answer) = match deliver.request(method, url, &headers, body.as_ref().map(|(t, b)| (*t, b)), ANSWER_MAX) {
        Ok(a) => a,
        // The error names the host only.
        Err(e) => return unknown(&format!("the provider didn't answer: {e}")),
    };
    if (300..400).contains(&status) {
        return unknown("the provider answered with a redirect, which isn't followed");
    }
    let json = || serde_json::from_slice::<serde_json::Value>(&answer).unwrap_or_default();
    match (checker, status) {
        (Checker::Aws, 200) => {
            let doc = String::from_utf8_lossy(&answer);
            (Validity::Live, account(crate::aws_identity::element(&doc, "Account")), None)
        }
        (Checker::Aws, 403) => {
            let doc = String::from_utf8_lossy(&answer);
            match crate::aws_identity::element(&doc, "Code") {
                Some("InvalidClientTokenId") => (Validity::Invalid, None, None),
                // The key id is AWS's, but the secret next to it isn't its own.
                Some("SignatureDoesNotMatch") => unknown("AWS knows the key id, but the secret next to it doesn't sign for it"),
                _ => unknown("AWS refused it for another reason"),
            }
        }
        (Checker::GitHub, 200) => (Validity::Live, account(json()["login"].as_str()), None),
        (Checker::Slack, 200) => {
            let v = json();
            match (v["ok"].as_bool(), v["error"].as_str()) {
                (Some(true), _) => (Validity::Live, account(v["team_id"].as_str()), None),
                (Some(false), Some("invalid_auth" | "account_inactive" | "token_revoked" | "token_expired" | "not_authed")) => (Validity::Invalid, None, None),
                _ => unknown("Slack's answer said neither"),
            }
        }
        // A restricted key without the balance permission still authenticated.
        (Checker::Stripe, 200 | 403) | (Checker::OpenAi | Checker::Anthropic, 200) => (Validity::Live, None, None),
        (_, 401) => (Validity::Invalid, None, None),
        (_, s) => unknown(&format!("the provider answered {s}")),
    }
}

const STS_URL: &str = "https://sts.amazonaws.com/";
const STS_HOST: &str = "sts.amazonaws.com";
const STS_CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";

/// SigV4 for `GetCallerIdentity` to STS's global endpoint (us-east-1): the `Authorization`
/// header and `X-Amz-Date`, signing content-type, host and the date.
fn sigv4_get_caller_identity(key_id: &str, secret: &str, now: time::OffsetDateTime) -> (Value, String) {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let hmac = |k: &[u8], m: &str| {
        let mut h = <Hmac<Sha256> as Mac>::new_from_slice(k).expect("any key length");
        h.update(m.as_bytes());
        h.finalize().into_bytes().to_vec()
    };
    let date = format!("{:04}{:02}{:02}", now.year(), now.month() as u8, now.day());
    let amz = format!("{date}T{:02}{:02}{:02}Z", now.hour(), now.minute(), now.second());
    let signed = "content-type;host;x-amz-date";
    let canonical = format!("POST\n/\n\ncontent-type:{STS_CONTENT_TYPE}\nhost:{STS_HOST}\nx-amz-date:{amz}\n\n{signed}\n{}", hex(&Sha256::digest(crate::aws_identity::BODY.as_bytes())));
    let scope = format!("{date}/us-east-1/sts/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
    let k = zeroize::Zeroizing::new(format!("AWS4{secret}"));
    let k = hmac(&hmac(&hmac(&hmac(k.as_bytes(), &date), "us-east-1"), "sts"), "aws4_request");
    let sig = hex(&hmac(&k, &to_sign));
    (Value::new(format!("AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed}, Signature={sig}")), amz)
}

/// Why a roll secret has no answer: the same words for a decoy and for what roll-access can't look
/// up without using it, so the answer never says which keys are decoys.
const NOT_LOOKED_UP: &str = "roll doesn't look this roll secret up";

/// How long an answer counts: a day, an hour for `unknown` ones and for roll's own tokens (a
/// revoke there should show soon).
fn ttl(kind: &str, result: Validity) -> i64 {
    if result == Validity::Unknown || kind == "a roll token" { UNKNOWN_TTL_MS } else { TTL_MS }
}

/// What to do about one credential, decided before anything is claimed.
enum Plan {
    /// Answered now and not kept: nobody was asked (not checkable, an AWS account read offline, a
    /// canary, a decoy).
    Now(Checked),
    /// Answered here without asking a provider (a roll token's lookup): kept and audited.
    Here(Answer),
    /// Asked of its provider, once claimed.
    Ask,
}

/// What a claim found for one credential.
enum Claim {
    /// A fresh answer is kept.
    Kept(Checked),
    /// Another caller is asking now.
    Asking,
    /// The project's checks this hour are spent.
    Limited,
    /// This caller asks.
    Mine,
}

impl Secrets {
    /// Whether the project's credentials found in code are checked with their providers.
    pub fn validity_checks(&self, project: &str) -> Result<bool> {
        Ok(self.db.read(|c| c.query_row("SELECT validity_checks FROM projects WHERE project_id = ?", [project], |r| r.get::<_, Option<i64>>(0)).optional())?.flatten() == Some(1))
    }

    /// Turn validity checks on or off (an admin's; audited as `validity.checks`).
    pub fn set_validity_checks(&self, project: &str, on: bool, caller: &Caller) -> Result<bool> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("UPDATE projects SET validity_checks = ? WHERE project_id = ?", params![on as i64, project])?;
            let summary = if on { "validity checks on: credentials found in code are checked with their providers" } else { "validity checks off" };
            audit::record(c, self.keys()?, project, caller, "validity.checks", None, None, &[], summary, now)
        })?;
        Ok(on)
    }

    /// The kept answer for a credential by its fingerprint (`GET …/unstored/{fingerprint}`).
    pub fn validity_of(&self, project: &str, fingerprint: &str, caller: &Caller) -> Result<Checked> {
        caller.need(Action::Read, &Scope::project(project))?;
        let row = self.db.read(|c| {
            c.query_row("SELECT kind, result, account, checked_at, why FROM validity WHERE project_id = ? AND fingerprint = ? AND result IS NOT NULL", params![project, fingerprint], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<String>>(4)?))
            })
            .optional()
        })?;
        let (kind, result, account, at, why) = row.ok_or_else(|| Error::NotFound(format!("{fingerprint} hasn't been checked")))?;
        Ok(Checked::answered(fingerprint.to_string(), &kind, Validity::parse(&result), account, at, why))
    }

    /// Check `probes` with their providers (see the module doc), in order: `None` when the
    /// project's checks are off (nothing is asked). Waits for the answers, its own and other
    /// callers' (up to 20 s for theirs).
    pub fn check_validity(&self, project: &str, probes: Vec<Probe>, caller: &Caller) -> Result<Option<Vec<Checked>>> {
        caller.need(Action::Write, &Scope::project(project))?;
        if probes.len() > MAX_PROBES {
            return Err(Error::BadRequest(format!("at most {MAX_PROBES} credentials a call")));
        }
        self.project(project)?;
        if !self.validity_checks(project)? {
            return Ok(None);
        }
        // What each one is, by the detectors (never the caller's word): one credential, whole.
        let found: Vec<(String, &'static str)> = probes
            .iter()
            .map(|p| {
                let v = p.value.expose();
                let m = roll_auth::credentials::find_all(v);
                let kind = match m.as_slice() {
                    [m] if m.start == 0 && m.end == v.len() && !roll_auth::credentials::placeholder(v) => m.kind,
                    _ => "",
                };
                (roll_auth::credentials::fingerprint(v), kind)
            })
            .collect();
        let now = self.db.now_ms()?;
        let mut out: Vec<Option<Checked>> = vec![None; probes.len()];
        // The ones to claim, once per fingerprint, with what was found here for them.
        let mut claim_for: Vec<(usize, Option<Answer>)> = Vec::new();
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for (i, (fp, kind)) in found.iter().enumerate() {
            if seen.contains_key(fp.as_str()) {
                continue;
            }
            seen.insert(fp, i);
            match self.plan(fp, kind, &probes[i], now) {
                Plan::Now(c) => out[i] = Some(c),
                Plan::Here(a) => claim_for.push((i, Some(a))),
                Plan::Ask => claim_for.push((i, None)),
            }
        }
        let claims = self.claim(project, &claim_for.iter().map(|(i, here)| (found[*i].0.as_str(), found[*i].1, here.is_none())).collect::<Vec<_>>(), now)?;
        let mut waiting = Vec::new();
        for ((i, here), claim) in claim_for.into_iter().zip(claims) {
            let (fp, kind) = (&found[i].0, found[i].1);
            out[i] = match claim {
                Claim::Kept(c) => Some(c),
                Claim::Limited => Some(Checked::unknown(fp.clone(), kind, "this project's checks for the hour are spent")),
                Claim::Asking => {
                    waiting.push(i);
                    None
                }
                Claim::Mine => {
                    let asked = here.is_none();
                    let (result, account, why) = match here {
                        Some(a) => a,
                        None => {
                            let deliver = self.deliver.as_deref().expect("planned with one");
                            let checker = Checker::of(kind).expect("planned for a kind roll checks");
                            // Outside any transaction: the claim's write is done.
                            ask(deliver, checker, &probes[i].value, probes[i].pair.as_ref(), time::OffsetDateTime::now_utc())
                        }
                    };
                    let at = self.db.now_ms()?;
                    let how = if asked { "with its provider" } else { "without using it" };
                    let summary = format!("checked {kind} {fp} {how}: {}{}", result.as_str(), account.as_ref().map(|a| format!(" (account {a})")).unwrap_or_default());
                    self.db.write(|c| {
                        c.execute("UPDATE validity SET result = ?, account = ?, checked_at = ?, why = ? WHERE project_id = ? AND fingerprint = ?", params![result.as_str(), account, at, why, project, fp])?;
                        audit::record(c, self.keys()?, project, caller, "secrets.validity", None, None, &[], &summary, at)
                    })?;
                    Some(Checked::answered(fp.clone(), kind, result, account, at, why))
                }
            };
        }
        // Another caller's check: its answer, once it's there.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(WAIT_MS);
        while !waiting.is_empty() {
            waiting.retain(|&i| match self.validity_of(project, &found[i].0, &Caller::operator()) {
                Ok(c) => {
                    out[i] = Some(c);
                    false
                }
                Err(_) => true,
            });
            if waiting.is_empty() || std::time::Instant::now() >= until {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        for i in waiting {
            out[i] = Some(Checked::unknown(found[i].0.clone(), found[i].1, "another check of it hasn't answered yet"));
        }
        // Repeats take the first one's answer.
        let firsts: Vec<Option<Checked>> = found.iter().map(|(fp, _)| seen.get(fp.as_str()).and_then(|&j| out[j].clone())).collect();
        Ok(Some(out.into_iter().zip(firsts).map(|(c, first)| c.or(first).expect("every credential answered")).collect()))
    }

    /// What to do about one credential (see [`Plan`]): only reads, no provider is asked.
    fn plan(&self, fp: &str, kind: &str, probe: &Probe, now: i64) -> Plan {
        let settings = self.validity.read().unwrap_or_else(|e| e.into_inner()).clone();
        let now_unknown = |account: Option<String>, why: &str| Plan::Now(Checked::answered(fp.to_string(), kind, Validity::Unknown, account, now, Some(why.into())));
        let Some(checker) = Checker::of(kind) else { return Plan::Now(Checked::unknown(fp.to_string(), kind, "roll can't check this kind of credential")) };
        if settings.canaries.fingerprints.contains(fp) {
            return now_unknown(None, "a canary the operator listed: never tried");
        }
        match checker {
            Checker::Roll => {
                let status = self.workloads.as_ref().map(|w| w.token_status(&probe.value));
                match status {
                    Some(Ok(Some(TokenStatus::Live))) => Plan::Here((Validity::Live, None, None)),
                    Some(Ok(Some(TokenStatus::Gone))) => Plan::Here((Validity::Invalid, None, None)),
                    // A decoy reads as any roll secret roll can't look up: nothing says it's one.
                    Some(Ok(Some(TokenStatus::Decoy) | None)) => now_unknown(None, NOT_LOOKED_UP),
                    Some(Err(_)) => now_unknown(None, "roll-access didn't answer"),
                    None => now_unknown(None, NOT_LOOKED_UP),
                }
            }
            Checker::Aws => {
                let id = probe.value.expose();
                let account = roll_auth::credentials::aws_account(id);
                if account.as_deref().is_some_and(|a| settings.canaries.account("aws", a)) {
                    now_unknown(account, "a known canary token's account: never tried")
                } else if !settings.aws_live {
                    now_unknown(account, "AWS keys aren't tried (that call is what AWS canary tokens alert on): the account is read from the key id")
                } else if id.starts_with("ASIA") {
                    now_unknown(account, "a session's key needs its session token")
                } else if probe.pair.is_none() {
                    now_unknown(account, "no secret access key next to it to sign with")
                } else if self.deliver.is_none() {
                    now_unknown(account, "calling out isn't on this server")
                } else {
                    Plan::Ask
                }
            }
            _ if self.deliver.is_none() => now_unknown(None, "calling out isn't on this server"),
            _ => Plan::Ask,
        }
    }

    /// The operator's validity settings: whether AWS keys are tried, and the canaries.
    pub fn set_validity_settings(&self, s: Settings) {
        *self.validity.write().unwrap_or_else(|e| e.into_inner()) = s;
    }

    /// Claim each credential's check in one write: a fresh answer is kept, a recent claim is
    /// another caller's, and past [`PER_HOUR`] claims this hour nothing more is asked. Only touches
    /// the database (the closure may run again).
    fn claim(&self, project: &str, items: &[(&str, &str, bool)], now: i64) -> Result<Vec<Claim>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        self.db.write(|c| {
            let mut used: i64 = c.query_row("SELECT COUNT(*) FROM validity WHERE project_id = ? AND claimed_at > ?", params![project, now - 3_600_000], |r| r.get(0))?;
            let mut out = Vec::with_capacity(items.len());
            for &(fp, kind, counts) in items {
                let row = c
                    .query_row("SELECT result, account, checked_at, claimed_at, why FROM validity WHERE project_id = ? AND fingerprint = ?", params![project, fp], |r| {
                        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<String>>(4)?))
                    })
                    .optional()?;
                if let Some((result, account, checked_at, claimed_at, why)) = row {
                    if let (Some(result), Some(at)) = (result.as_deref().map(Validity::parse), checked_at) {
                        if now - at < ttl(kind, result) {
                            out.push(Claim::Kept(Checked::answered(fp.to_string(), kind, result, account, at, why)));
                            continue;
                        }
                    } else if now - claimed_at < CLAIM_MS {
                        out.push(Claim::Asking);
                        continue;
                    }
                }
                // A lookup here (counts: false) asks no provider: outside the limit, and its claim
                // (claimed_at 0) isn't counted.
                if counts && used >= PER_HOUR {
                    out.push(Claim::Limited);
                    continue;
                }
                used += counts as i64;
                c.execute(
                    "INSERT INTO validity (project_id, fingerprint, kind, result, account, checked_at, claimed_at) VALUES (?, ?, ?, NULL, NULL, NULL, ?)
                     ON CONFLICT (project_id, fingerprint) DO UPDATE SET kind = excluded.kind, result = NULL, account = NULL, checked_at = NULL, why = NULL, claimed_at = excluded.claimed_at",
                    params![project, fp, kind, if counts { now } else { 0 }],
                )?;
                out.push(Claim::Mine);
            }
            Ok::<_, Error>(out)
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sigv4_matches_awss_own_example_shape() {
        // AWS's documented example pair, at 2026-01-02 03:04:05 UTC.
        let at = time::OffsetDateTime::from_unix_timestamp(1_767_323_045).unwrap();
        let (auth, date) = super::sigv4_get_caller_identity("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", at);
        assert_eq!(date, "20260102T030405Z");
        let a = auth.expose();
        assert!(a.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260102/us-east-1/sts/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature="), "{a}");
        assert_eq!(a.rsplit('=').next().unwrap().len(), 64);
        assert!(!a.contains("wJalr"), "never the secret");
    }
}
