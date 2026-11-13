//! OpenSearch 3.9's Security plugin on roll's security model (the opensearch dialect): internal
//! users, roles, role mappings and action groups (`_plugins/_security/api/*`, [`api`]), the
//! account and authinfo APIs, and the authentication and authorization of an opensearch-dialect
//! cluster's requests.
//!
//! **Storage.** The plugin's four configuration types are families of the cluster's security
//! state ([`crate::xpack::store`], home index `.opendistro_security`), one object per entry, in
//! the plugin's own shape. The plugin's defaults ([`config.json`](config.json), captured from
//! `GET` on OpenSearch 3.9.0) are the static and reserved roles and action groups, the owner
//! `admin` and the mapping of its backend role to `all_access`; a stored entry overrides a
//! default of its name and `{"_deleted": true}` hides one. The demo users of OpenSearch's
//! distribution (with their published passwords) are left out.
//!
//! **Authentication.** A Basic user is an internal user whose password checks against its hash
//! (roll's PBKDF2 for passwords set through the API, bcrypt for `hash`es given by clients, which
//! is OpenSearch's format). `admin` is the cluster's owner: the node authenticates it with the
//! cluster's own credentials, so it has no password here.
//!
//! **Authorization** goes through the one evaluator ([`super::Effective`]): a user's roles are
//! its `opendistro_security_roles` and the roles whose mapping names the user or one of its
//! backend roles ([`mapped_roles`]), and each role becomes the role descriptor
//! [`super::privileges::Permission`] reads ([`descriptor`]): action groups expanded to the
//! action patterns they stand for, `index_patterns` and `dls` with the user's variables
//! substituted, `fls` as `field_security`, `masked_fields` as they are. What differs from
//! Elasticsearch is the plugin's: its 403 message, the actions it checks as cluster actions
//! ([`is_cluster_action`]), no narrowing of wildcards to the permitted indices
//! (`do_not_fail_on_forbidden` is off by default), and the REST API's own access check
//! ([`api`]).

pub mod api;
mod password;
mod patch;

use super::privileges::Permission;
use super::{AuthKind, Principal};
use crate::cluster::Cluster;
use crate::error::EsError;
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

/// The plugin's configuration index.
pub const INDEX: &str = ".opendistro_security";
/// The cluster's owner (the plugin's `admin`).
pub const OWNER: &str = "admin";
/// The key of a stored entry that hides a default of its name.
const DELETED: &str = "_deleted";
/// `plugins.security.compliance.salt`'s default (masked fields' BLAKE2b salt).
const DEFAULT_SALT: &str = "e1ukloTsQlOgPquJ";

/// The plugin's configuration types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Users,
    Roles,
    Mappings,
    Groups,
}

impl Kind {
    /// The security state family the entries are stored in.
    pub fn family(self) -> &'static str {
        match self {
            Kind::Users => "opendistro_security.internalusers",
            Kind::Roles => "opendistro_security.roles",
            Kind::Mappings => "opendistro_security.rolesmapping",
            Kind::Groups => "opendistro_security.actiongroups",
        }
    }

    /// How the plugin names an entry in messages (`user 'x' not found.`).
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Users => "user",
            Kind::Roles => "role",
            Kind::Mappings => "rolesmapping",
            Kind::Groups => "actiongroup",
        }
    }

    /// The `restapi:admin/…` permission that opens the type's API.
    pub fn endpoint(self) -> &'static str {
        match self {
            Kind::Users => "internalusers",
            Kind::Roles => "roles",
            Kind::Mappings => "rolesmapping",
            Kind::Groups => "actiongroups",
        }
    }

    fn defaults(self) -> &'static BTreeMap<String, Value> {
        static C: OnceLock<BTreeMap<String, BTreeMap<String, Value>>> = OnceLock::new();
        let all = C.get_or_init(|| {
            let Ok(Some(Value::Obj(m))) = crate::json::parse(include_bytes!("config.json")) else { return BTreeMap::new() };
            m.into_iter().map(|(k, v)| (k, v.as_object().map(|o| o.iter().map(|(n, e)| (n.clone(), e.clone())).collect()).unwrap_or_default())).collect()
        });
        static EMPTY: BTreeMap<String, Value> = BTreeMap::new();
        all.get(self.endpoint()).unwrap_or(&EMPTY)
    }
}

/// An entry by name: the stored one, else the default (none when a stored tombstone hides it).
pub fn entry(st: &ClusterState, kind: Kind, name: &str) -> Option<Value> {
    match st.metadata.xpack.get(kind.family(), name) {
        Some(v) if v.get(DELETED).is_some() => None,
        Some(v) => Some(v.clone()),
        None => kind.defaults().get(name).cloned(),
    }
}

/// Every entry: the defaults not overridden, then the stored ones.
pub fn entries(st: &ClusterState, kind: Kind) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = kind.defaults().iter().filter(|(n, _)| st.metadata.xpack.get(kind.family(), n).is_none()).map(|(n, v)| (n.clone(), v.clone())).collect();
    out.extend(st.metadata.xpack.iter(kind.family()).filter(|(_, v)| v.get(DELETED).is_none()).map(|(n, v)| (n.clone(), v.clone())));
    out
}

/// Whether a default entry of this name exists (deleting it leaves a tombstone).
fn has_default(kind: Kind, name: &str) -> bool {
    kind.defaults().contains_key(name)
}

/// The tombstone that hides a default entry.
fn tombstone() -> Value {
    let mut m = Map::new();
    m.insert(DELETED.into(), true.into());
    Value::Obj(m)
}

/// An entry's `reserved`, `static` or `hidden` flag.
pub fn flag(v: &Value, name: &str) -> bool {
    v.get(name).and_then(Value::as_bool).unwrap_or(false)
}

pub(crate) fn strs(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
        _ => Vec::new(),
    }
}

/// What an OpenSearch user is besides its name and roles.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Identity {
    pub backend_roles: Vec<String>,
    pub attributes: Map,
    /// The cluster's owner: skips authorization, like the plugin's super admin.
    pub owner: bool,
    /// A reserved internal user (`is_reserved` of the account API).
    pub reserved: bool,
}

/// The principal of an internal user.
fn principal_of(st: &ClusterState, name: &str, user: &Value, owner: bool) -> Principal {
    let backend_roles = strs(user.get("backend_roles"));
    let roles = mapped_roles(st, name, &backend_roles, &strs(user.get("opendistro_security_roles")));
    Principal {
        username: name.to_string(),
        roles,
        full_name: Value::Null,
        email: Value::Null,
        metadata: Value::object(),
        realm_name: "internal",
        realm_type: "internal",
        kind: AuthKind::OpenSearch(Box::new(Identity {
            backend_roles,
            attributes: user.get("attributes").and_then(Value::as_object).cloned().unwrap_or_default(),
            owner,
            reserved: flag(user, "reserved"),
        })),
    }
}

/// The cluster's owner, `admin`.
pub fn owner(st: &ClusterState) -> Principal {
    let user = entry(st, Kind::Users, OWNER).or_else(|| Kind::Users.defaults().get(OWNER).cloned()).unwrap_or_else(Value::object);
    principal_of(st, OWNER, &user, true)
}

/// The request's principal: the authenticated user, the owner when nobody authenticated, and
/// a user without roles for any other principal (roll's per-index scope of the owner).
pub fn principal(ctx: &crate::rest::Ctx) -> Principal {
    match ctx.principal.get() {
        None => owner(&ctx.state()),
        Some(p) if p.os().is_some() => p.clone(),
        Some(p) => Principal { roles: Vec::new(), kind: AuthKind::OpenSearch(Box::default()), ..p.clone() },
    }
}

/// Authenticates `user:password`. `admin` is the owner, whom the node authenticates with the
/// cluster's credentials: here (no node in front: dev mode) any password, and never for a node
/// asking about credentials it couldn't verify (`native`).
pub fn authenticate_password(cluster: &Cluster, user: &str, password: &str, native: bool) -> Option<Principal> {
    let st = cluster.state();
    if user == OWNER {
        return (!native).then(|| owner(&st));
    }
    let u = entry(&st, Kind::Users, user)?;
    let hash = u.get("hash").and_then(Value::as_str).filter(|h| !h.is_empty())?;
    super::users::check_password_with(cluster, user, password, hash, verify_hash).then(|| principal_of(&st, user, &u, false))
}

/// The internal user `name` without its credentials.
pub fn lookup_user(cluster: &Cluster, name: &str) -> Option<Principal> {
    let st = cluster.state();
    if name == OWNER {
        return Some(owner(&st));
    }
    entry(&st, Kind::Users, name).map(|u| principal_of(&st, name, &u, false))
}

/// bcrypt costs above this never verify (a hash a client gave can't make a login take minutes).
const MAX_BCRYPT_COST: u32 = 14;

/// Checks a password against a stored hash: roll's `{PBKDF2}`, or bcrypt (`$2a$`, `$2b$`, `$2y$`).
fn verify_hash(password: &str, stored: &str) -> bool {
    if stored.starts_with("{PBKDF2}") {
        return super::verify_password(password, stored);
    }
    let cost = stored.strip_prefix("$2").and_then(|r| r.get(2..4)).and_then(|c| c.parse::<u32>().ok());
    match cost {
        Some(c) if c <= MAX_BCRYPT_COST => bcrypt::verify(password, stored).unwrap_or(false),
        _ => false,
    }
}

/// The plugin's wildcard matching of names in mappings: `*`, `?`, or a `/regex/`.
fn wildcard(pattern: &str, s: &str) -> bool {
    if pattern.len() > 1 && pattern.starts_with('/') && pattern.ends_with('/') {
        return regex::Regex::new(&format!("^(?:{})$", &pattern[1..pattern.len() - 1])).is_ok_and(|r| r.is_match(s));
    }
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => (0..=s.len()).any(|i| go(&p[1..], &s[i..])),
            Some(b'?') => !s.is_empty() && go(&p[1..], &s[1..]),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    if pattern.matches('*').count() > 8 {
        return false;
    }
    go(pattern.as_bytes(), s.as_bytes())
}

/// A user's roles (`ConfigModel.mapSecurityRoles`): its own `opendistro_security_roles`, and every
/// role whose mapping names the user, one of its backend roles, or all of its `and_backend_roles`.
/// Mappings by host never match (roll doesn't see the client's address). Sorted.
pub fn mapped_roles(st: &ClusterState, user: &str, backend: &[String], direct: &[String]) -> Vec<String> {
    let mut out: BTreeSet<String> = direct.iter().cloned().collect();
    for (role, m) in entries(st, Kind::Mappings) {
        let any = |key: &str, values: &[String]| strs(m.get(key)).iter().any(|p| values.iter().any(|v| wildcard(p, v)));
        let and = strs(m.get("and_backend_roles"));
        let all = !and.is_empty() && and.iter().all(|p| backend.iter().any(|b| wildcard(p, b)));
        if any("users", &[user.to_string()]) || any("backend_roles", backend) || all {
            out.insert(role);
        }
    }
    out.into_iter().collect()
}

/// The action patterns `names` stand for: action groups expanded (recursively, each once), `*` as
/// a pattern over every action, other patterns as they are. A name that is neither a group nor
/// an action pattern grants nothing (and never one of Elasticsearch's named privileges).
pub fn expand(st: &ClusterState, names: &[String]) -> Vec<String> {
    fn go(st: &ClusterState, names: &[String], seen: &mut BTreeSet<String>, out: &mut Vec<String>) {
        for n in names {
            if let Some(g) = entry(st, Kind::Groups, n) {
                if seen.insert(n.clone()) {
                    go(st, &strs(g.get("allowed_actions")), seen, out);
                }
            } else if n == "*" {
                out.push("*:*".into());
            } else if n.contains(':') && !out.contains(n) {
                out.push(n.clone());
            }
        }
    }
    let mut out = Vec::new();
    go(st, names, &mut BTreeSet::new(), &mut out);
    out
}

/// The user variables of index patterns and DLS queries (`${user.name}`, `${user_name}`,
/// `${user.roles}`: backend roles, `${user.securityRoles}`, `${attr.internal.<name>}`).
fn substitute(s: &str, p: &Principal, id: &Identity) -> String {
    if !s.contains("${") {
        return s.to_string();
    }
    let mut out = s.replace("${user.name}", &p.username).replace("${user_name}", &p.username);
    out = out.replace("${user.roles}", &id.backend_roles.join(",")).replace("${user.securityRoles}", &p.roles.join(","));
    for (k, v) in &id.attributes {
        out = out.replace(&format!("${{attr.internal.{k}}}"), &v.to_es_string());
    }
    out
}

/// The role descriptor (Elasticsearch's shape, as [`Permission::of_descriptor`] reads it) an
/// OpenSearch role stands for, for principal `p`.
pub fn descriptor(st: &ClusterState, role: &Value, p: &Principal, id: &Identity) -> Value {
    let arr = |v: Vec<String>| Value::Arr(v.into_iter().map(Value::from).collect());
    let mut indices = Vec::new();
    for e in role.get("index_permissions").and_then(Value::as_array).into_iter().flatten() {
        let names: Vec<String> = strs(e.get("index_patterns")).iter().map(|n| substitute(n, p, id)).collect();
        let privileges = expand(st, &strs(e.get("allowed_actions")));
        if names.is_empty() || privileges.is_empty() {
            continue;
        }
        let mut g = Map::new();
        g.insert("names".into(), arr(names));
        g.insert("privileges".into(), arr(privileges));
        g.insert("allow_restricted_indices".into(), false.into());
        if let Some(q) = e.get("dls").and_then(Value::as_str).filter(|q| !q.trim().is_empty()) {
            g.insert("query".into(), Value::from(substitute(q, p, id)));
        }
        let fls = strs(e.get("fls"));
        if !fls.is_empty() {
            let (except, grant): (Vec<String>, Vec<String>) = fls.into_iter().partition(|f| f.starts_with('~'));
            let except: Vec<String> = except.into_iter().map(|f| f[1..].to_string()).collect();
            let grant = if grant.is_empty() { vec!["*".to_string()] } else { grant };
            let mut fs = Map::new();
            fs.insert("grant".into(), arr(grant));
            fs.insert("except".into(), arr(except));
            g.insert("field_security".into(), Value::Obj(fs));
        }
        let masked = strs(e.get("masked_fields"));
        if !masked.is_empty() {
            g.insert("masked_fields".into(), arr(masked));
        }
        indices.push(Value::Obj(g));
    }
    let mut d = Map::new();
    d.insert("cluster".into(), arr(expand(st, &strs(role.get("cluster_permissions")))));
    d.insert("indices".into(), Value::Arr(indices));
    Value::Obj(d)
}

/// The permissions of an OpenSearch principal's roles (roles that don't exist grant nothing).
pub fn permissions(st: &ClusterState, p: &Principal, id: &Identity) -> Vec<Permission> {
    p.roles.iter().filter_map(|r| entry(st, Kind::Roles, r)).map(|r| Permission::of_descriptor(&descriptor(st, &r, p, id))).collect()
}

/// Actions the plugin checks against cluster permissions (`PrivilegesEvaluator.isClusterPerm`)
/// besides `cluster:*` ones: the composite requests (their items are then checked on their
/// indices), scrolls, reindex and template rendering.
pub fn is_cluster_action(action: &str) -> bool {
    action.starts_with("cluster:")
        || action.starts_with("indices:admin/template/")
        || action.starts_with("indices:admin/index_template/")
        || action.starts_with("indices:data/read/scroll")
        || action.starts_with("indices:data/read/msearch")
        || matches!(action, "indices:data/write/bulk" | "indices:data/read/mget" | "indices:data/read/mtv" | "indices:data/write/reindex" | "indices:data/read/search/template/render")
}

/// How the plugin names a user in messages.
pub fn user_string(p: &Principal, id: &Identity) -> String {
    format!("User [name={}, backend_roles=[{}], requestedTenant=null]", p.username, id.backend_roles.join(", "))
}

/// The plugin's 403 for an action no role permits.
pub fn denied(p: &Principal, id: &Identity, action: &str) -> EsError {
    EsError::security(403, format!("no permissions for [{action}] and {}", user_string(p, id)))
}

/// The plugin's 401: a plain `Unauthorized` with its Basic challenge.
pub fn unauthorized() -> EsError {
    EsError::new(401, "security_exception", "Unauthorized")
        .with_hidden("plain_body", Value::from("Unauthorized"))
        .with_header("WWW-Authenticate", "Basic realm=\"OpenSearch Security\"")
}

/// The salt of masked fields' hashes (`plugins.security.compliance.salt`, its first 16 bytes).
pub fn masking_salt(cluster: &Cluster) -> [u8; 16] {
    let s = cluster.config.node_setting("plugins.security.compliance.salt").unwrap_or(DEFAULT_SALT);
    let mut out = [0u8; 16];
    for (o, b) in out.iter_mut().zip(s.as_bytes()) {
        *o = *b;
    }
    out
}

/// Masks a value of a masked field (`MaskedField`): `field` hashes it (BLAKE2b-256 with the
/// salt, hex), `field::/regex/::replacement[::/regex/::replacement…]` rewrites it, and
/// `field::SHA-256` / `field::SHA-512` hash it with that digest.
pub fn mask(rule: &str, salt: &[u8; 16], value: &str) -> String {
    use blake2::digest::core_api::{Buffer, OutputSizeUser, UpdateCore, VariableOutputCore};
    let parts: Vec<&str> = rule.split("::").collect();
    match parts.len() {
        1 => {
            // Unkeyed BLAKE2b with a salt (the `Mac` wrapper with an empty key isn't that).
            let mut core = blake2::Blake2bVarCore::new_with_params(salt, &[], 0, 32);
            let mut buf = Buffer::<blake2::Blake2bVarCore>::default();
            buf.digest_blocks(value.as_bytes(), |blocks| core.update_blocks(blocks));
            let mut out = blake2::digest::generic_array::GenericArray::<u8, <blake2::Blake2bVarCore as OutputSizeUser>::OutputSize>::default();
            core.finalize_variable_core(&mut buf, &mut out);
            hex(&out[..32])
        }
        2 => {
            use sha2::Digest;
            match parts[1].to_ascii_uppercase().as_str() {
                "SHA-512" => hex(&sha2::Sha512::digest(value.as_bytes())),
                "SHA-384" => hex(&sha2::Sha384::digest(value.as_bytes())),
                _ => hex(&sha2::Sha256::digest(value.as_bytes())),
            }
        }
        _ => {
            let mut out = value.to_string();
            for pair in parts[1..].chunks(2) {
                let [re, with] = pair else { break };
                let re = re.strip_prefix('/').and_then(|r| r.strip_suffix('/')).unwrap_or(re);
                if let Ok(r) = regex::Regex::new(re) {
                    out = r.replace_all(&out, regex::NoExpand(with)).into_owned();
                }
            }
            out
        }
    }
}

/// The field pattern of a masked field rule.
pub fn mask_field(rule: &str) -> &str {
    rule.split("::").next().unwrap_or(rule)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_like_the_plugin() {
        // Captured from OpenSearch 3.9.0 with `plugins.security.compliance.salt: AZ#MSCQo*@XNyrSc`.
        let mut salt = [0u8; 16];
        salt.copy_from_slice(b"AZ#MSCQo*@XNyrSc");
        assert_eq!(mask("mask", &salt, "m1"), "b2b87666b901e048518156988ae0b8cacae0aa1a24db57ee7d2e634be375dc93");
        assert_eq!(mask("f::/[0-9]/::X", &salt, "a1b2"), "aXbX");
        assert_eq!(mask_field("f::/[0-9]/::X"), "f");
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("esm-*", "esm-u1") && wildcard("b?", "b1") && !wildcard("b?", "b12"));
        assert!(wildcard("/b[0-9]+/", "b12") && !wildcard("/b[0-9]+/", "c1"));
    }

    #[test]
    fn groups_expand_to_actions() {
        let st = ClusterState::default();
        let read = expand(&st, &["read".into()]);
        assert!(read.contains(&"indices:data/read*".to_string()));
        let crud = expand(&st, &["crud".into(), "monitor".into(), "*".into()]);
        assert!(crud.contains(&"indices:data/write*".to_string()) && crud.contains(&"*:*".to_string()));
        assert!(!crud.contains(&"monitor".to_string()), "an unknown name grants nothing");
        assert!(super::super::privileges::index_grants("*:*", "indices:data/read/search"));
    }

    #[test]
    fn bcrypt_hashes_verify() {
        let h = bcrypt::hash("s3krit-password", 4).unwrap();
        assert!(verify_hash("s3krit-password", &h) && !verify_hash("other", &h));
        assert!(!verify_hash("x", "$2y$31$abcdefghijklmnopqrstuuabcdefghijklmnopqrstuvwxyz01234"));
    }
}
