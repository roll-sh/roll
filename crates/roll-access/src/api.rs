//! `/v1/access` (docs/permissions.md, "HTTP API"): grants, approvals, the audit log and OAuth
//! clients, for the dashboard and `roll token`. The gate has already decided each request (the
//! catalogue's `access` rows); handlers only tell the operator from a grant.

use crate::store::{AuditQuery, GrantChange, NewGrant};
use crate::{USER_REACH, Access, Error, catalog};
use axum::extract::{Path, Query, State};
use axum::http::{Extensions, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use roll_auth::GrantKind;
use roll_auth::scope::{Action, Outcome, PRODUCTS, Scope, Target};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type S = State<Arc<Access>>;

pub fn router(access: Arc<Access>) -> Router {
    Router::new()
        .route("/v1/access", get(me))
        .route("/v1/access/catalog", get(catalog_view))
        .route("/v1/access/check", post(check))
        .route("/v1/access/tokens", get(list).post(create))
        .route("/v1/access/tokens/{id}", get(get_one).patch(update).delete(revoke))
        .route("/v1/access/tokens/{id}/rotate", post(rotate))
        .route("/v1/access/tokens/{id}/approve", post(approve_token))
        .route("/v1/access/tokens/{id}/deny", post(deny_token))
        .route("/v1/access/tokens/{id}/tighten", get(tighten).post(apply_tighten))
        .route("/v1/access/tokens/{id}/networks", get(networks))
        .route("/v1/access/checkup", get(checkup))
        .route("/v1/access/tokens/{id}/temporary/{approval}", axum::routing::delete(forget))
        .route("/v1/access/approvals", get(approvals))
        .route("/v1/access/approvals/{id}", get(approval))
        .route("/v1/access/approvals/{id}/approve", post(approve))
        .route("/v1/access/approvals/{id}/deny", post(deny))
        .route("/v1/access/audit", get(audit))
        .route("/v1/access/audit/sinks", get(sinks).post(create_sink))
        .route("/v1/access/audit/sinks/{id}", axum::routing::delete(delete_sink))
        .route("/v1/access/audit/sinks/{id}/retry", post(retry_sink))
        .route("/v1/access/notify", get(notify).put(set_notify))
        .route("/v1/access/policy", get(policy).put(set_policy))
        .route("/v1/access/oauth/clients", get(clients))
        .route("/v1/access/oauth/clients/{id}", get(client))
        .route("/v1/access/oauth/authorize", post(crate::oauth::authorize))
        .with_state(access)
}

async fn blocking<T: Send + 'static>(a: Arc<Access>, f: impl FnOnce(&Access) -> crate::Result<T> + Send + 'static) -> crate::Result<T> {
    tokio::task::spawn_blocking(move || f(&a)).await.map_err(|e| Error::Internal(e.to_string()))?
}

/// Who's calling: the operator (or an admin), a grant, or a signed-in user who isn't an admin
/// (docs/permissions.md, "Users' own tokens": they manage only their own).
pub(crate) enum Who {
    Operator,
    Grant(roll_auth::Grant),
    User(roll_auth::SignedIn),
}

impl Who {
    /// Whose tokens this caller sees and manages: a user's own (theirs, or their token's), or
    /// every token (`None`).
    fn mine(&self) -> Option<&str> {
        match self {
            Who::Operator => None,
            Who::Grant(g) => g.user.as_deref(),
            Who::User(u) => Some(&u.user),
        }
    }
}

pub(crate) fn who(access: &Access, ext: &Extensions) -> crate::Result<Who> {
    if let Some(g) = roll_auth::Grant::on(ext) {
        return Ok(Who::Grant(g.clone()));
    }
    if roll_auth::Operator::on(ext) {
        return Ok(Who::Operator);
    }
    if let Some(u) = ext.get::<roll_auth::SignedIn>().filter(|u| !u.admin) {
        return Ok(Who::User(u.clone()));
    }
    if !access.config.locked {
        return Ok(Who::Operator);
    }
    Err(Error::Unauthorized("this API needs a token".into()))
}

/// A token `who` may see: every one for the operator, else only the user's own.
fn visible(who: &Who, g: &crate::store::Grant) -> bool {
    who.mine().is_none_or(|u| g.user.as_deref() == Some(u))
}

/// The token `id`, when `who` may see it (else 404, as if it weren't there).
async fn visible_grant(a: &Arc<Access>, who: &Who, id: &str) -> crate::Result<crate::store::Grant> {
    let id2 = id.to_string();
    let g = blocking(a.clone(), move |a| a.store.get(&id2)).await?;
    if visible(who, &g) { Ok(g) } else { Err(Error::NotFound(format!("no token {id}"))) }
}

/// Who acted, for the record: the signed-in admin (`usr_…`, docs/accounts.md), else the operator's
/// token ("operator").
pub(crate) fn actor(ext: &Extensions) -> String {
    ext.get::<roll_auth::SignedIn>().map_or_else(|| "operator".to_string(), |u| u.user.clone())
}

fn operator_only(access: &Access, ext: &Extensions, what: &str) -> crate::Result<()> {
    match who(access, ext)? {
        Who::Operator => Ok(()),
        Who::Grant(_) | Who::User(_) => Err(Error::Forbidden(format!("only the operator can {what}"))),
    }
}

async fn me(State(a): S, ext: Extensions) -> crate::Result<Json<Value>> {
    Ok(Json(match who(&a, &ext)? {
        Who::Operator => json!({ "kind": "operator", "locked": a.config.locked }),
        Who::Grant(g) => {
            let id = g.id.to_string();
            let grant = blocking(a.clone(), move |a| a.store.get(&id)).await?;
            let mut out = json!({ "kind": if g.kind == GrantKind::Oauth { "oauth" } else { "token" }, "grant": grant, "locked": a.config.locked });
            // A person's own token reaches what they do, as the gate decides it: say so.
            if let (Some(user), Some(people)) = (g.user.as_deref(), a.people.get())
                && let Ok(Some(p)) = people.person(user).await
                && !p.admin
            {
                out["as_user"] = json!(p.handle);
                out["reach"] = json!(USER_REACH);
            }
            out
        }
        // What a user's tokens can reach: what their session can (docs/accounts.md).
        // `approve_people`: what in their tokens waits for an admin (the builder and consent say so first).
        Who::User(u) => json!({ "kind": "user", "user": { "id": u.user, "handle": u.handle }, "reach": USER_REACH, "approve_people": a.policy_now().approve_people, "locked": a.config.locked }),
    }))
}

/// Products, actions, presets and every classified route: what the builder offers.
async fn catalog_view(State(a): S) -> Json<Value> {
    let rows: Vec<_> = catalog::rows().iter().map(catalog::view).collect();
    Json(json!({
        // What this node runs: a rule on a product that's off grants nothing here. Rules may still
        // name any of `PRODUCTS` (another node, a product turned on later).
        "products": PRODUCTS.iter().filter(|p| a.config.products.is_empty() || matches!(**p, "workspace" | "access") || a.config.products.iter().any(|o| o == **p)).collect::<Vec<_>>(),
        "actions": Action::ALL,
        "presets": presets(),
        "routes": rows,
        "operator_only": catalog::OPERATOR_ONLY.iter().map(|(r, why)| json!({ "route": r, "why": why })).collect::<Vec<_>>(),
        "api_url": a.config.api_url,
    }))
}

/// The builder's starting points (docs/permissions.md, "Presets"). `project` in a rule's
/// resources means "the project the user picks" (every resource when none is picked).
pub fn presets() -> Value {
    let all = |actions: Value| json!({ "product": "*", "resources": "*", "actions": actions });
    json!([
        { "id": "read-only", "name": "Read only", "description": "See everything; change nothing.", "rules": [all(json!({ "read": "allow" }))] },
        {
            "id": "agent", "name": "Agent, asks before destructive",
            "description": "Read and write everywhere. Deleting, revealing credentials and shipping to production wait for your approval.",
            "rules": [all(json!({ "read": "allow", "write": "allow", "delete": "ask", "reveal": "ask", "deploy": "ask" }))],
        },
        {
            "id": "project", "name": "This project only",
            "description": "Read and write one workspace project's resources; destructive actions ask.",
            "rules": [{ "product": "*", "resources": "project", "actions": { "read": "allow", "write": "allow", "delete": "ask", "reveal": "ask", "deploy": "ask" } }],
        },
        {
            "id": "development", "name": "Dev environment only",
            "description": "Everything in development environments; revealing secrets asks. Production is out of reach.",
            "rules": [{ "product": "*", "resources": "project", "environments": ["development"], "actions": { "read": "allow", "write": "allow", "delete": "allow", "reveal": "ask" } }],
        },
        {
            "id": "previews", "name": "Previews only",
            "description": "Preview environments only: create, change and tear them down. Nothing they were branched from.",
            "rules": [{ "product": "*", "resources": "*", "environments": ["preview"], "actions": { "read": "allow", "write": "allow", "delete": "allow", "reveal": "ask" } }],
        },
        { "id": "full", "name": "Full access", "description": "Everything the operator can do, until it expires.", "rules": [all(json!({ "read": "allow", "write": "allow", "delete": "allow", "reveal": "allow", "deploy": "allow", "admin": "allow" }))] },
    ])
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    method: String,
    /// The path with its query, as a request would send it (`/v1/projects/prj_x?days=7`).
    path: String,
    #[serde(default)]
    body: Option<Value>,
    /// Whose decision (the operator only; a grant checks itself).
    #[serde(default)]
    token: Option<String>,
}

/// What the evaluator would decide for a request, without running it: the builder's "test this
/// token", and an agent asking before it tries. Same catalogue and resolvers as a real request;
/// a nested or catch-all route is matched by its path, as the gate matches it.
async fn check(State(a): S, ext: Extensions, Json(c): Json<Check>) -> crate::Result<Json<Value>> {
    let who = who(&a, &ext)?;
    let (scope, ask_otherwise) = match (&who, &c.token) {
        (Who::Grant(g), None) => (g.scope.as_ref().clone(), g.ask_otherwise),
        (Who::Grant(_), Some(_)) => return Err(Error::Forbidden("a token checks only itself".into())),
        (Who::Operator | Who::User(_), Some(id)) => {
            let g = visible_grant(&a, &who, id).await?;
            (g.effective_rules(code_sql::now_ms()), g.ask_otherwise)
        }
        (Who::Operator, None) => return Ok(Json(json!({ "decision": "allow", "why": "the operator may do everything" }))),
        (Who::User(_), None) => return Err(Error::BadRequest("name the token to check (token)".into())),
    };
    let method = c.method.to_ascii_uppercase();
    let uri: axum::http::Uri = c.path.parse().map_err(|_| Error::BadRequest("path isn't a request path".into()))?;
    let Some(found) = catalog::by_path(&method, uri.path()) else {
        return Ok(Json(json!({ "decision": "deny", "why": "no grant can call this route: it's the operator's" })));
    };
    let row = found.row;
    let params = found.params.unwrap_or_default();
    let (a2, q) = (a.clone(), uri.query().map(str::to_string));
    let params2 = params.clone();
    let target = tokio::task::spawn_blocking(move || crate::evaluate::target(&a2, row, &params2, q.as_deref(), &axum::http::HeaderMap::new()))
        .await
        .map_err(|e| Error::Internal(e.to_string()))?;
    // Decided as the gate decides: the copy a body asks for, and "ask for anything else".
    let mut action = row.action(&method, &params, c.body.as_ref());
    let mut outcome = if matches!(row.target, catalog::Tgt::AnyGrant) { Outcome::Allow } else { scope.outcome(&target, action) };
    if let Some((copied, _, _)) = crate::evaluate::copy_outcome(&a, row, &params, &target, (c.body.as_ref(), uri.query()), &scope).await
        && crate::evaluate::rank(copied) < crate::evaluate::rank(outcome)
    {
        (outcome, action) = (copied, Action::Reveal);
    }
    let mut about: (&str, Option<String>) = (row.product, None);
    if let Some(l) = crate::evaluate::lend_outcome(&a, row, &params, c.body.as_ref(), &scope).await
        && crate::evaluate::rank(l.outcome) < crate::evaluate::rank(outcome)
    {
        (outcome, action) = (l.outcome, l.action);
        about = (l.product, Some(l.id));
    }
    if outcome == Outcome::Deny && ask_otherwise && catalog::placement(row, &params).is_none() {
        outcome = Outcome::Ask;
    }
    // What a path resolves to (an endpoint's project, a replica's database) only for what the
    // token reaches.
    // A link that decided is reported as itself: the linked resource's product and id.
    let resource = if outcome == Outcome::Deny { None } else { about.1.or_else(|| crate::evaluate::describe(&target)) };
    Ok(Json(json!({
        "decision": outcome,
        "product": about.0,
        "action": action,
        "resource": resource,
        "route": format!("{} {}", row.method, row.route),
    })))
}

/// Grants, newest first, each with its last day's requests (`activity`: allowed, denied, asked).
/// `GET /v1/access/tokens?product=&resource=`: only the tokens made for one product resource.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListFilter {
    product: Option<String>,
    resource: Option<String>,
    /// `workload`: the workload tokens instead (the latest 500).
    kind: Option<String>,
}

/// Whether a rule of `g`'s is about `product` (by name, not `*`) and, with `resource`, names it
/// (`{id}` or `{project}`): the tokens a product's page lists as its own (roll secrets' project
/// tokens), not every token that happens to reach it.
fn made_for(g: &crate::store::Grant, product: &str, resource: Option<&str>) -> bool {
    use roll_auth::scope::{Resources, Selector};
    g.rules.0.iter().any(|r| {
        r.product == product
            && match (resource, &r.resources) {
                (None, _) => true,
                (Some(_), Resources::All) => false,
                (Some(want), Resources::Some(sel)) => sel.iter().any(|s| match s {
                    Selector::Project { project } => project == want,
                    Selector::Id { id, .. } => id == want,
                }),
            }
    })
}

/// A user sees their own.
async fn list(State(a): S, ext: Extensions, Query(f): Query<ListFilter>) -> crate::Result<Json<Value>> {
    if f.resource.is_some() && f.product.is_none() {
        return Err(Error::BadRequest("resource needs product".into()));
    }
    let who = who(&a, &ext)?;
    let workloads = match f.kind.as_deref() {
        None => false,
        Some("workload") => true,
        Some(k) => return Err(Error::BadRequest(format!("kind {k:?}: only workload"))),
    };
    let mine = who.mine().map(str::to_string);
    let (grants, activity) = blocking(a, move |a| Ok((if workloads { a.store.workloads(500, mine.as_deref())? } else { a.store.list()? }, a.store.activity()?))).await?;
    let out: Vec<Value> = grants
        .into_iter()
        .filter(|g| visible(&who, g))
        .filter(|g| f.product.as_deref().is_none_or(|p| made_for(g, p, f.resource.as_deref())))
        .map(|g| {
            let (allowed, denied, asked) = activity.get(&g.id).copied().unwrap_or_default();
            let mut v = json!(g);
            v["activity"] = json!({ "allowed": allowed, "denied": denied, "asked": asked });
            v
        })
        .collect();
    Ok(Json(json!(out)))
}

async fn get_one(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    let who = who(&a, &ext)?;
    Ok(Json(json!(visible_grant(&a, &who, &id).await?)))
}

/// Tokens checked for this many at most for a narrower tighten proposal (an audit query each).
const CHECKUP_TIGHTEN: usize = 200;

/// The access checkup: the live tokens that need attention, each with its findings and fixes.
async fn checkup(State(a): S, ext: Extensions) -> crate::Result<Json<Value>> {
    // A person's checkup is of their own tokens.
    let who = who(&a, &ext)?;
    let policy = a.policy_now();
    let out = blocking(a, move |a| {
        let now = code_sql::now_ms();
        let counts = a.store.network_counts()?;
        let since = now - crate::checkup::UNUSED_DAYS * 86_400_000;
        let mut checked = 0;
        let mut out = Vec::new();
        for g in a.store.list()?.into_iter().filter(|g| g.live(now) && visible(&who, g)) {
            // Whether its proposal is narrower: for tokens used lately, a bounded number of them.
            let used_lately = g.last_used_at.is_some_and(|t| t >= since);
            let narrower = if used_lately && checked < CHECKUP_TIGHTEN {
                checked += 1;
                let used = a.store.used(&g.id, since)?;
                !used.is_empty() && crate::tighten::tighten(&g.rules, &used) != g.rules
            } else {
                false
            };
            let seen = crate::checkup::Seen { networks: counts.get(&g.id).copied().unwrap_or(0), narrower };
            let mut findings = crate::checkup::findings(&g, &seen, now);
            // A person sees that their own waits elsewhere; deciding it isn't theirs.
            if matches!(who, Who::User(_)) {
                findings.retain(|f| f.kind != "pending");
            } else if g.user.is_some() && !g.pending && g.approved_at.is_none() && g.live(now) && policy.needs_approval(&g.rules, g.ask_otherwise) {
                // A person's token made before the policy asked: it works, unapproved, until an
                // admin decides it.
                findings.insert(0, crate::checkup::Finding { kind: "unapproved", why: "a person's token that reaches what the policy has an admin approve, made before the policy asked: approve it or deny it".into(), fix: "approve" });
            }
            if !findings.is_empty() {
                out.push(json!({
                    "grant": { "id": g.id, "name": g.name, "kind": g.kind, "last_used_at": g.last_used_at, "expires_at": g.expires_at, "created_at": g.created_at },
                    "findings": findings,
                }));
            }
        }
        Ok(out)
    })
    .await?;
    Ok(Json(json!(out)))
}

/// The networks a token has been used from (its /24 or /48), with when first and last.
async fn networks(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    // A person sees only their own tokens'.
    let who = who(&a, &ext)?;
    visible_grant(&a, &who, &id).await?;
    let list = blocking(a, move |a| a.store.networks(&id)).await?;
    Ok(Json(json!(list.into_iter().map(|(network, first_at, last_at)| json!({ "network": network, "first_at": first_at, "last_at": last_at })).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct TightenQuery {
    /// The days of activity it's narrowed to (1-90; default 30).
    days: Option<i64>,
}

/// Its rules narrowed to what its requests used ([`crate::tighten`]): a proposal, never wider,
/// with the rules it was made `from` (what applying it checks).
async fn tighten(State(a): S, ext: Extensions, Path(id): Path<String>, Query(q): Query<TightenQuery>) -> crate::Result<Json<Value>> {
    let who = who(&a, &ext)?;
    let (grant, days, since, used, rules) = proposal(&a, &who, id, q.days).await?;
    Ok(Json(json!({ "since": since, "days": days, "used": used, "rules": rules, "from": grant.rules, "narrower": rules != grant.rules })))
}

/// A person sees only their own tokens' activity.
async fn proposal(a: &Arc<Access>, who: &Who, id: String, days: Option<i64>) -> crate::Result<(crate::store::Grant, i64, i64, Vec<crate::tighten::Used>, roll_auth::scope::Scope)> {
    let days = days.unwrap_or(30).clamp(1, 90);
    let since = code_sql::now_ms() - days * 86_400_000;
    let grant = visible_grant(a, who, &id).await?;
    let used = blocking(a.clone(), move |a| a.store.used(&id, since)).await?;
    let rules = crate::tighten::tighten(&grant.rules, &used);
    // A proposal the rules' own checks would refuse (too many rules, too long) isn't one.
    let rules = if rules.problems().is_empty() { rules } else { grant.rules.clone() };
    Ok((grant, days, since, used, rules))
}

#[derive(Deserialize)]
struct ApplyTighten {
    days: Option<i64>,
    /// The rules the proposal was made from: applied only if they're still the token's.
    from: roll_auth::scope::Scope,
}

/// Apply the proposal, made again now, if the token's rules are still the ones it was shown from
/// (another change in between: 409). The operator's, like editing rules.
async fn apply_tighten(State(a): S, ext: Extensions, Path(id): Path<String>, Json(b): Json<ApplyTighten>) -> crate::Result<Json<Value>> {
    // The operator, or a person for their own token (narrowing never widens). A token never.
    let who = who(&a, &ext)?;
    if matches!(who, Who::Grant(_)) {
        return Err(Error::Forbidden("a token doesn't tighten tokens: the operator or its owner does".into()));
    }
    let (grant, _, _, _, rules) = proposal(&a, &who, id.clone(), b.days).await?;
    if rules == grant.rules {
        return Ok(Json(json!(grant)));
    }
    Ok(Json(json!(blocking(a, move |a| a.store.swap_rules(&id, &b.from, &rules)).await?)))
}

async fn create(State(a): S, ext: Extensions, body: axum::body::Bytes) -> crate::Result<Response> {
    let new: NewGrant = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))?;
    let (grant, token) = a.create_for(&ext, new).await?;
    Ok((StatusCode::CREATED, Json(json!({ "grant": grant, "token": token }))).into_response())
}

impl Access {
    /// Make a token for the request `ext` belongs to, as `POST /v1/access/tokens` does (it's this
    /// function): its owner is the caller, a grant makes only what it covers, then the token
    /// policy and the caps. For a product making one of its own tokens for its caller (roll
    /// secrets' project tokens), never for someone else, so `ext` must name one (the operator, a
    /// signed-in person or a grant): extensions without a caller are refused, even on an open
    /// daemon, where the route itself treats them as the operator. The secret is returned once.
    pub async fn create_grant(self: &Arc<Self>, ext: &Extensions, new: NewGrant) -> crate::Result<(crate::store::Grant, String)> {
        let named = roll_auth::Operator::on(ext) || ext.get::<roll_auth::SignedIn>().is_some() || ext.get::<roll_auth::Grant>().is_some();
        if !named {
            return Err(Error::Forbidden("create_grant needs the caller's request: these extensions name no one".into()));
        }
        self.create_for(ext, new).await
    }

    async fn create_for(self: &Arc<Self>, ext: &Extensions, mut new: NewGrant) -> crate::Result<(crate::store::Grant, String)> {
        // A user's token is theirs, and so is one their token makes: it acts as them.
        let who = who(self, ext)?;
        // A person's own token past what the policy has an admin approve waits for one (one their
        // token makes is within that token, which was approved).
        let asker = match &who {
            Who::User(u) if self.policy_now().needs_approval(&new.rules, new.ask_otherwise) => Some(u.handle.clone()),
            _ => None,
        };
        new.pending = asker.is_some();
        let (by, user) = match who {
            // An admin's token is theirs too: it acts as the operator while they're an admin, and
            // stops (or narrows to their own reach) when they aren't.
            Who::Operator => (actor(ext), ext.get::<roll_auth::SignedIn>().map(|u| u.user.clone())),
            Who::User(u) => (u.user.clone(), Some(u.user)),
            Who::Grant(g) => {
                check_within(self, &g, &Shape { rules: &new.rules, expires_at: new.expires_at, ip_allow: &new.ip_allow, rate: new.rate_per_minute, ask_otherwise: new.ask_otherwise }).await?;
                (g.id.to_string(), g.user.as_deref().map(str::to_string))
            }
        };
        self.check_policy(new.expires_at)?;
        let made = blocking(self.clone(), move |a| a.store.create(&new, GrantKind::Token, &by, None, user.as_deref())).await?;
        if let Some(handle) = asker {
            self.tell_admins_waiting(&made.0, &handle);
        }
        Ok(made)
    }

    /// Admins hear that a person's token waits for one of them (GitHub mails organization owners):
    /// who asked, what it reaches, and where to decide. Never its secret.
    pub(crate) fn tell_admins_waiting(self: &Arc<Self>, g: &crate::store::Grant, handle: &str) {
        let (name, handle) = (crate::mail_name(&g.name), crate::mail_name(handle));
        let reaches: Vec<String> = g.rules.0.iter().map(|r| format!("  {}: {}", r.product, r.actions.keys().map(|a| a.as_str()).collect::<Vec<_>>().join(", "))).collect();
        let text = format!(
            "{handle} made the token \"{}\", which waits for an admin's approval. It may:\n{}{}\n\nApprove or deny it: {}/dashboard/access/tokens/{}\n",
            name,
            reaches.join("\n"),
            if g.ask_otherwise { "\n  and ask for anything else" } else { "" },
            self.web_url(),
            g.id
        );
        self.tell_admins(&format!("pending:{}", g.id), format!("A roll token waits for your approval: \"{name}\" ({handle})"), text);
    }
}

/// What a grant would be: what a maker must cover.
struct Shape<'a> {
    rules: &'a Scope,
    expires_at: Option<i64>,
    ip_allow: &'a [String],
    rate: Option<u32>,
    ask_otherwise: bool,
}

impl<'a> Shape<'a> {
    fn of(g: &'a crate::store::Grant) -> Shape<'a> {
        Shape { rules: &g.rules, expires_at: g.expires_at, ip_allow: &g.ip_allow, rate: g.rate_per_minute, ask_otherwise: g.ask_otherwise }
    }
}

/// A grant may hand out only what it has: rules it covers, for no longer than it lives, from no
/// more addresses than it may be used from, at no more requests a minute than it may make.
async fn check_within(a: &Arc<Access>, g: &roll_auth::Grant, shape: &Shape<'_>) -> crate::Result<()> {
    let Shape { rules, expires_at, ip_allow, rate, ask_otherwise } = *shape;
    let id = g.id.to_string();
    let maker = blocking(a.clone(), move |a| a.store.get(&id)).await?;
    // A workload's token is a job's for minutes: what it makes would be a credential nobody set
    // up, so it makes and changes none.
    if maker.kind == "workload" {
        return Err(Error::Forbidden("a workload token can't make or change tokens".into()));
    }
    // Its own rules, not what it's allowed for a while: a while never becomes for good.
    if !maker.rules.covers(rules) {
        return Err(Error::Forbidden("a token can only make tokens it covers: every rule must be within one of its own".into()));
    }
    if let Some(end) = maker.expires_at
        && expires_at.is_none_or(|e| e > end)
    {
        return Err(Error::Forbidden("a token can't make one that outlives it: set expires_at on or before its own".into()));
    }
    if !maker.ip_allow.is_empty() {
        let mine = crate::store::parse_ip_allow(&maker.ip_allow)?;
        let theirs = crate::store::parse_ip_allow(ip_allow)?;
        let inside = !theirs.is_empty() && theirs.iter().all(|t| mine.iter().any(|m| m.contains(t)));
        if !inside {
            return Err(Error::Forbidden(format!("a token limited to {} can only make tokens limited within those addresses", maker.ip_allow.join(", "))));
        }
    }
    if ask_otherwise && !maker.ask_otherwise {
        return Err(Error::Forbidden("only a token that asks for anything else can make one that does".into()));
    }
    if let Some(mine) = maker.rate_per_minute
        && rate.is_none_or(|r| r > mine)
    {
        return Err(Error::Forbidden(format!("a token limited to {mine} requests a minute can only make tokens limited to that or less")));
    }
    Ok(())
}

/// The target as `g` may touch it: itself (not its addresses), or a token it covers entirely.
async fn may_touch(a: &Arc<Access>, g: &roll_auth::Grant, target: &crate::store::Grant) -> crate::Result<()> {
    if *g.id == *target.id {
        return Ok(());
    }
    check_within(a, g, &Shape::of(target)).await
}

async fn update(State(a): S, ext: Extensions, Path(id): Path<String>, body: axum::body::Bytes) -> crate::Result<Json<Value>> {
    let change: GrantChange = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))?;
    let who = who(&a, &ext)?;
    let target = visible_grant(&a, &who, &id).await?;
    // What an admin is deciding doesn't change under them: a waiting token's reach is fixed until
    // they do (an approval would otherwise take rules they never saw).
    if target.pending && (change.rules.is_some() || change.ask_otherwise.is_some()) {
        return Err(Error::Conflict("it waits for an admin's approval: its rules can't change until they decide (revoke it and make another)".into()));
    }
    let person = matches!(who, Who::User(_));
    let who_handle = match &who {
        Who::User(u) => Some(u.handle.clone()),
        _ => None,
    };
    if let Who::Grant(g) = who {
        // What the token is now, and what it would become, must both be within the caller's; a
        // token never widens its own addresses.
        may_touch(&a, &g, &target).await?;
        if *g.id == *target.id && change.ip_allow.as_ref().is_some_and(|ip| *ip != target.ip_allow) {
            return Err(Error::Forbidden("a token can't change the addresses it may be used from".into()));
        }
        let rules = change.rules.clone().unwrap_or(target.rules.clone());
        let expires = change.expires_at.unwrap_or(target.expires_at);
        let ips = change.ip_allow.clone().unwrap_or(target.ip_allow.clone());
        let rate = change.rate_per_minute.unwrap_or(target.rate_per_minute);
        let ask_otherwise = change.ask_otherwise.unwrap_or(target.ask_otherwise);
        check_within(&a, &g, &Shape { rules: &rules, expires_at: expires, ip_allow: &ips, rate, ask_otherwise }).await?;
    }
    // A token whose rules, lifetime or addresses change meets the policy as it is now.
    if change.rules.is_some() || change.expires_at.is_some() || change.ip_allow.is_some() {
        a.check_policy(change.expires_at.unwrap_or(target.expires_at))?;
    }
    // A person widening their own token past what the policy has an admin approve: it waits again.
    let asker = match &who_handle {
        Some(h) => h.clone(),
        None => String::new(),
    };
    let again = person
        && (change.rules.is_some() || change.ask_otherwise.is_some())
        && a.policy_now().needs_approval(change.rules.as_ref().unwrap_or(&target.rules), change.ask_otherwise.unwrap_or(target.ask_otherwise));
    let mut change = change;
    if again {
        change.pending = Some(true);
    }
    let updated = blocking(a.clone(), move |a| a.store.update(&id, &change)).await?;
    if again {
        a.tell_admins_waiting(&updated, &asker);
    }
    Ok(Json(json!(updated)))
}

/// `POST /v1/access/tokens/{id}/approve`: an admin lets a person's pending token work (GitHub's
/// organization approval of fine-grained tokens). An admin's call, never a token's.
async fn approve_token(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    decide_token(a, &ext, id, true).await
}

/// `POST /v1/access/tokens/{id}/deny`: it's revoked, saying an admin denied it.
async fn deny_token(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    decide_token(a, &ext, id, false).await
}

async fn decide_token(a: Arc<Access>, ext: &Extensions, id: String, approve: bool) -> crate::Result<Json<Value>> {
    if !matches!(who(&a, ext)?, Who::Operator) {
        return Err(Error::Forbidden("only an admin approves people's tokens".into()));
    }
    let g = blocking(a.clone(), move |a| a.store.decide_pending(&id, approve)).await?;
    if let Some(user) = g.user.as_deref() {
        let name = crate::mail_name(&g.name);
        let (subject, text) = if approve {
            (format!("Your roll token \"{name}\" was approved"), "An admin approved it: it works now.\n".to_string())
        } else {
            (format!("Your roll token \"{name}\" was denied"), "An admin denied it, so it's revoked. Make one that reaches less, or ask them.\n".to_string())
        };
        a.tell(Some(user), None, subject, text);
    }
    Ok(Json(json!(g)))
}

async fn revoke(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    let who = who(&a, &ext)?;
    let target = visible_grant(&a, &who, &id).await?;
    if let Who::Grant(g) = who {
        // A grant revokes itself, or tokens it covers.
        may_touch(&a, &g, &target).await?;
    }
    Ok(Json(json!(blocking(a, move |a| a.store.revoke(&id)).await?)))
}

/// Take back what an approval allowed a token for a while. The operator decides approvals, so
/// the operator takes them back.
async fn forget(State(a): S, ext: Extensions, Path((id, approval)): Path<(String, String)>) -> crate::Result<Json<Value>> {
    // Whoever may decide the approval takes it back: the operator, or a person for their own token.
    may_decide(&a, &ext, &approval).await?;
    // The approval is this token's, and the token is one the caller sees.
    let who = who(&a, &ext)?;
    visible_grant(&a, &who, &id).await?;
    let (a2, apr) = (a.clone(), approval.clone());
    if blocking(a2, move |a| a.store.approval(&apr)).await?.grant_id != id {
        return Err(Error::NotFound(format!("{approval} isn't this token's")));
    }
    Ok(Json(json!(blocking(a, move |a| a.store.forget(&id, &approval)).await?)))
}

/// A new secret, the same rules: shown once. A grant may rotate itself, or tokens it covers.
/// `POST /v1/access/tokens/{id}/rotate`, `{keep_old_for_s?}`: the old secret stops now, or works
/// that much longer (at most a week) while deployments move to the new one.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RotateBody {
    keep_old_for_s: Option<i64>,
}

async fn rotate(State(a): S, ext: Extensions, Path(id): Path<String>, body: axum::body::Bytes) -> crate::Result<Json<Value>> {
    let body: RotateBody = if body.iter().all(u8::is_ascii_whitespace) { RotateBody::default() } else { serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))? };
    if body.keep_old_for_s.is_some_and(|s| s < 0 || s * 1000 > crate::store::MAX_KEEP_OLD_MS) {
        return Err(Error::BadRequest("keep_old_for_s is 0 to 604800 (a week)".into()));
    }
    let who = who(&a, &ext)?;
    let target = visible_grant(&a, &who, &id).await?;
    if let Who::Grant(g) = who {
        may_touch(&a, &g, &target).await?;
    }
    // A new secret is a new token: it meets the policy (set an expiry first if it must expire).
    a.check_policy(target.expires_at)?;
    let keep = body.keep_old_for_s.map(|s| s * 1000);
    let (grant, token) = blocking(a, move |a| a.store.rotate(&id, keep)).await?;
    Ok(Json(json!({ "grant": grant, "token": token })))
}

#[derive(Deserialize)]
struct ApprovalsQuery {
    state: Option<String>,
    /// One token's approvals (its page).
    grant: Option<String>,
    limit: Option<usize>,
}

/// The ids of the grants `user` owns.
async fn grants_of(a: &Arc<Access>, user: &str) -> crate::Result<std::collections::HashSet<String>> {
    let user = user.to_string();
    blocking(a.clone(), move |a| a.store.grant_ids_of(&user)).await
}

/// A person sees their own tokens' approvals only: filtered in the query, so the limit counts theirs.
async fn approvals(State(a): S, ext: Extensions, Query(q): Query<ApprovalsQuery>) -> crate::Result<Json<Value>> {
    let who = who(&a, &ext)?;
    let grants: Option<Vec<String>> = match (who.mine(), q.grant) {
        (None, g) => g.map(|g| vec![g]),
        (Some(user), g) => {
            let mine = grants_of(&a, user).await?;
            Some(match g {
                Some(g) => mine.contains(&g).then_some(g).into_iter().collect(),
                None => mine.into_iter().collect(),
            })
        }
    };
    let list = blocking(a, move |a| a.store.approvals(q.state.as_deref(), grants.as_deref(), q.limit.unwrap_or(100))).await?;
    Ok(Json(json!(list)))
}

/// Who may decide an approval: the operator (or an admin), and a user for their own tokens'. A
/// token never decides one, not even its own.
async fn may_decide(a: &Arc<Access>, ext: &Extensions, id: &str) -> crate::Result<String> {
    match who(a, ext)? {
        Who::Operator => Ok(actor(ext)),
        Who::Grant(_) => Err(Error::Forbidden("a token never decides a request".into())),
        Who::User(u) => {
            let id2 = id.to_string();
            let found = blocking(a.clone(), move |a| a.store.approval(&id2)).await?;
            if grants_of(a, &u.user).await?.contains(&found.grant_id) { Ok(u.user) } else { Err(Error::NotFound(format!("no approval {id}"))) }
        }
    }
}

#[derive(Deserialize)]
struct WaitQuery {
    /// Seconds to wait for a decision (at most 55).
    wait: Option<u64>,
}

async fn approval(State(a): S, ext: Extensions, Path(id): Path<String>, Query(q): Query<WaitQuery>) -> crate::Result<Json<Value>> {
    let who = who(&a, &ext)?;
    let (a2, id2) = (a.clone(), id.clone());
    let found = blocking(a2, move |a| a.store.approval(&id2)).await?;
    let hidden = match &who {
        Who::Operator => false,
        Who::Grant(g) => *g.id != *found.grant_id && g.scope.outcome(&Target::Product { product: "access".into() }, Action::Read) != Outcome::Allow,
        Who::User(_) => false,
    };
    let not_mine = match who.mine() {
        Some(user) => !grants_of(&a, user).await?.contains(&found.grant_id),
        None => false,
    };
    if hidden || not_mine {
        return Err(Error::NotFound(format!("no approval {id}")));
    }
    let found = match q.wait.filter(|w| *w > 0) {
        Some(w) => crate::evaluate::wait(&a, &id, std::time::Duration::from_secs(w.min(55))).await?,
        None => found,
    };
    Ok(Json(json!(found)))
}

#[derive(Deserialize, Default)]
struct Decision {
    reason: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Approve {
    /// Also allow the same action on the same resource from now on: `1h`, `8h`, `1d` or `always`.
    remember: Option<String>,
}

/// Approve a request, once, or also for what's asked again (docs/permissions.md, "Remembered
/// approvals"): the same action on the same resource for a while, or for good.
async fn approve(State(a): S, ext: Extensions, Path(id): Path<String>, body: axum::body::Bytes) -> crate::Result<Json<Value>> {
    let by = may_decide(&a, &ext, &id).await?;
    let ap: Approve = if body.iter().all(u8::is_ascii_whitespace) { Approve::default() } else { serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))? };
    let remembered = remembered(&a, &id, ap.remember.as_deref()).await?;
    let decided = blocking(a.clone(), move |a| a.store.decide(&id, true, &by, None)).await?;
    a.changed(&decided);
    if let Some((grant, rule, until)) = remembered {
        let apr = decided.id.clone();
        blocking(a.clone(), move |a| a.store.remember(&grant, &rule, until, &apr)).await?;
    }
    Ok(Json(json!(decided)))
}

/// What approving `id` with `remember` would also allow: the grant, the rule (the request's action
/// on its one resource) and until when (`None`: for good). Checked before anything is decided.
async fn remembered(a: &Arc<Access>, id: &str, remember: Option<&str>) -> crate::Result<Option<(String, roll_auth::scope::Rule, Option<i64>)>> {
    let hours = |h: i64| Some(Some(code_sql::now_ms() + h * 3_600_000));
    let until: Option<Option<i64>> = match remember {
        None | Some("once") => None,
        Some("1h") => hours(1),
        Some("8h") => hours(8),
        Some("1d") => hours(24),
        Some("always") => Some(None),
        Some(other) => return Err(Error::BadRequest(format!("remember is 1h, 8h, 1d or always, not {other}"))),
    };
    let Some(until) = until else { return Ok(None) };
    let id2 = id.to_string();
    let found = blocking(a.clone(), move |a| a.store.approval(&id2)).await?;
    // About one resource, as a rule would name it: not something new, not several links at once.
    let Some(resource) = found.resource.clone().filter(|r| !r.starts_with("new in ") && !r.contains(", ")) else {
        return Err(Error::BadRequest("this request isn't about one resource: approve it once".into()));
    };
    // A route whose product filters per environment answers about no environment: a rule from it
    // would drop the environments the token is limited to.
    if catalog::find(&found.method, &found.route, &found.path).is_some_and(|f| f.row.filters_itself) {
        return Err(Error::BadRequest("this route spans every environment: approve it once".into()));
    }
    // The indices it was about stay part of it (`cl_1/logs-a` is cluster `cl_1`, index `logs-a`);
    // a secrets request names one key, and a name that isn't one would remember as a pattern.
    if found.scope_index.as_deref().is_some_and(|ix| !roll_auth::scope::rememberable_part(&found.product, ix)) {
        return Err(Error::BadRequest("this request names no single encryption key: approve it once".into()));
    }
    let selector = match found.scope_index.clone() {
        Some(ix) => roll_auth::scope::Selector::Id { id: resource.strip_suffix(&format!("/{ix}")).unwrap_or(&resource).to_string(), index: Some(ix) },
        None => roll_auth::scope::Selector::id(resource),
    };
    let action: Action = serde_json::from_value(json!(found.action)).map_err(|_| Error::Internal(format!("approval {id} has action {}", found.action)))?;
    let rule = roll_auth::scope::Rule {
        product: found.product.clone(),
        resources: roll_auth::scope::Resources::Some(vec![selector]),
        // Only where the request was: an approval in development never reaches production.
        environments: found.environment.clone().map(|e| vec![e]),
        paths: found.scope_path.clone().map(|p| vec![p]),
        actions: [(action, roll_auth::scope::Effect::Allow)].into_iter().collect(),
    };
    // What the request named becomes a rule: it meets the rules' own checks (one line, sizes).
    if let Some(p) = roll_auth::scope::Scope(vec![rule.clone()]).problems().first() {
        return Err(Error::BadRequest(format!("this request can't be remembered ({p}): approve it once")));
    }
    // For good is a change to the token: it meets the policy as it is now.
    if until.is_none() {
        let g = found.grant_id.clone();
        let grant = blocking(a.clone(), move |a| a.store.get(&g)).await?;
        a.check_policy(grant.expires_at)?;
    }
    Ok(Some((found.grant_id, rule, until)))
}

async fn deny(State(a): S, ext: Extensions, Path(id): Path<String>, body: axum::body::Bytes) -> crate::Result<Json<Value>> {
    let by = may_decide(&a, &ext, &id).await?;
    let d: Decision = if body.iter().all(u8::is_ascii_whitespace) { Decision::default() } else { serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))? };
    let reason = d.reason.map(|r| r.chars().take(500).collect::<String>());
    let decided = blocking(a.clone(), move |a| a.store.decide(&id, false, &by, reason.as_deref())).await?;
    a.changed(&decided);
    Ok(Json(json!(decided)))
}

/// Where approvals are posted: `{chat: {install, channel, approvers} | null}` (docs/permissions.md,
/// "Approvals in chat"). `approvers` are the chat user ids (Slack `U…`, Discord snowflakes) who
/// may decide there: the operator names them, whoever else sees the channel.
async fn notify(State(a): S, ext: Extensions) -> crate::Result<Json<Value>> {
    operator_only(&a, &ext, "see where approvals go")?;
    let v = blocking(a, |a| a.store.setting("notify")).await?;
    Ok(Json(v.unwrap_or_else(|| json!({ "chat": null }))))
}

async fn set_notify(State(a): S, ext: Extensions, Json(v): Json<Value>) -> crate::Result<Json<Value>> {
    operator_only(&a, &ext, "choose where approvals go")?;
    let chat = &v["chat"];
    let short = |v: &Value| v.as_str().is_some_and(|i| i.len() <= 64 && !i.is_empty());
    let id = |v: &Value| short(v) && v.as_str().is_some_and(|i| i.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)));
    let approvers = chat["approvers"].as_array().filter(|a| !a.is_empty() && a.len() <= 50 && a.iter().all(id));
    let ok = chat.is_null()
        || (short(&chat["install"])
            && short(&chat["channel"])
            && approvers.is_some()
            && chat.as_object().is_some_and(|o| o.keys().all(|k| ["install", "channel", "approvers"].contains(&k.as_str()))));
    if !ok || v.as_object().is_none_or(|o| o.keys().any(|k| k != "chat")) {
        return Err(Error::BadRequest(
            "expected {\"chat\": {\"install\": \"chi_…\", \"channel\": \"C…\", \"approvers\": [\"U…\"]}} (1-50 chat user ids who may decide) or {\"chat\": null}".into(),
        ));
    }
    let v2 = json!({ "chat": chat });
    blocking(a, move |a| a.store.set_setting("notify", &v2)).await?;
    Ok(Json(json!({ "chat": chat })))
}

/// Where the audit log streams to (docs/permissions.md, "Audit sinks").
async fn sinks(State(a): S, ext: Extensions) -> crate::Result<Json<Value>> {
    operator_only(&a, &ext, "see where the audit log goes")?;
    Ok(Json(json!(blocking(a, |a| a.list_sinks()).await?)))
}

async fn create_sink(State(a): S, ext: Extensions, body: axum::body::Bytes) -> crate::Result<Response> {
    operator_only(&a, &ext, "send the audit log somewhere")?;
    let n: crate::sinks::NewSink = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))?;
    let by = actor(&ext);
    let (sink, secret) = blocking(a, move |a| a.create_sink(n, &by)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "sink": sink, "secret": secret }))).into_response())
}

async fn retry_sink(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<StatusCode> {
    operator_only(&a, &ext, "retry an audit sink")?;
    blocking(a, move |a| a.retry_sink(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_sink(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<StatusCode> {
    operator_only(&a, &ext, "stop sending the audit log")?;
    blocking(a, move |a| a.delete_sink(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The operator's rules for every grant: `{max_days, require_expiry}`.
async fn policy(State(a): S) -> crate::Result<Json<Value>> {
    Ok(Json(json!(blocking(a, |a| Ok(a.policy())).await?)))
}

async fn set_policy(State(a): S, ext: Extensions, Json(p): Json<crate::Policy>) -> crate::Result<Json<Value>> {
    operator_only(&a, &ext, "set roll's token policy")?;
    if p.max_days == Some(0) || p.max_idle_days == Some(0) {
        return Err(Error::BadRequest("max_days and max_idle_days are at least 1 (or null for no limit)".into()));
    }
    let v = json!(p);
    blocking(a.clone(), move |a| a.store.set_setting("policy", &v)).await?;
    // This node sees it at once; the others within seconds.
    let _ = blocking(a, |a| Ok(a.policy())).await;
    Ok(Json(json!(p)))
}

/// The audit log; `?format=jsonl`: one event per line, as a download (what a SIEM or `jq` reads).
async fn audit(State(a): S, ext: Extensions, Query(mut q): Query<AuditQuery>) -> crate::Result<Response> {
    let format = q.format.clone();
    if format.as_deref().is_some_and(|f| f != "jsonl" && f != "csv") {
        return Err(Error::BadRequest("format: jsonl or csv".into()));
    }
    let jsonl = format.as_deref() == Some("jsonl");
    // A user reads their own tokens' rows.
    if let Some(user) = who(&a, &ext)?.mine() {
        q.grants = Some(grants_of(&a, user).await?.into_iter().collect());
    }
    let events = blocking(a, move |a| a.store.audit_log(&q)).await?;
    if format.as_deref() == Some("csv") {
        let headers = [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"), (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"roll-audit.csv\"")];
        return Ok((headers, audit_csv(&events)).into_response());
    }
    if jsonl {
        let mut out = String::new();
        for e in &events {
            out.push_str(&serde_json::to_string(e).unwrap_or_default());
            out.push('\n');
        }
        let headers = [(axum::http::header::CONTENT_TYPE, "application/x-ndjson"), (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"roll-audit.jsonl\"")];
        return Ok((headers, out).into_response());
    }
    Ok(Json(json!(events)).into_response())
}

/// The audit log as CSV, for a spreadsheet: one event a row, the time in UTC. Token and client
/// names and resources are anyone's text, so a cell that would start a formula (`=`, `+`, `-`, `@`,
/// a tab or a carriage return) is prefixed with `'`, as OWASP's CSV injection advice says.
pub fn audit_csv(events: &[crate::store::AuditEvent]) -> String {
    // Every cell quoted; a formula character is defused at the start and after anything a
    // spreadsheet may split a cell on (`;` in many locales, a tab, `|`), past any spaces.
    fn cell(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        let mut at_start = true;
        for c in s.chars() {
            if at_start && matches!(c, '=' | '+' | '-' | '@' | '\t' | '\r') {
                out.push('\'');
            }
            out.push(c);
            if matches!(c, ';' | '\t' | '|') {
                at_start = true;
            } else if c != ' ' {
                at_start = false;
            }
        }
        format!("\"{}\"", out.replace('"', "\"\""))
    }
    let mut out = String::from("at,token,token_name,source,tool,method,route,product,action,resource,decision,approval,status,ip,duration_ms,client\n");
    for e in events {
        let at = chrono::DateTime::from_timestamp_millis(e.at).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).unwrap_or_default();
        let row = [
            at,
            e.grant_id.clone().unwrap_or_default(),
            e.grant_name.clone().unwrap_or_default(),
            e.source.clone(),
            e.tool.clone().unwrap_or_default(),
            e.method.clone(),
            e.route.clone(),
            e.product.clone().unwrap_or_default(),
            e.action.clone().unwrap_or_default(),
            e.resource.clone().unwrap_or_default(),
            e.decision.clone(),
            e.approval.clone().unwrap_or_default(),
            e.status.to_string(),
            e.ip.clone().unwrap_or_default(),
            e.duration_ms.to_string(),
            e.client.clone().unwrap_or_default(),
        ];
        out.push_str(&row.iter().map(|c| cell(c)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out
}

async fn clients(State(a): S, ext: Extensions) -> crate::Result<Json<Value>> {
    // Clients with a live connection, from the grants that name them (a user's own).
    let who = who(&a, &ext)?;
    let grants = blocking(a, |a| a.store.list()).await?;
    let mut seen = std::collections::BTreeMap::new();
    for g in grants.into_iter().filter(|g| g.revoked_at.is_none() && visible(&who, g)) {
        if let Some(c) = g.client {
            seen.entry(c.id.clone()).or_insert(c);
        }
    }
    Ok(Json(json!(seen.into_values().collect::<Vec<_>>())))
}

/// A client: registered here, or (an https URL) known by its metadata document. A token sees only
/// the clients roll knows: it never makes roll fetch a document.
async fn client(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    let c = match who(&a, &ext)? {
        // People on the consent screen (the operator, a signed-in user) may have a document fetched.
        Who::Operator | Who::User(_) => crate::oauth::known_client(&a, &id).await?,
        Who::Grant(_) => {
            let id = id.clone();
            blocking(a.clone(), move |a| a.store.client(&id)).await?
        }
    };
    let c = c.ok_or_else(|| Error::NotFound("no such client".into()))?;
    Ok(Json(json!(c)))
}

#[cfg(test)]
mod csv_tests {
    /// A spreadsheet opens it safely: a name that would be a formula is text, and commas and quotes
    /// stay in their cell.
    #[test]
    fn the_audit_csv_never_starts_a_formula() {
        let e = crate::store::AuditEvent {
            id: "aud_1".into(),
            at: 0,
            grant_id: Some("tok_1".into()),
            grant_name: Some("=HYPERLINK(\"https://x.example\",\"click\")".into()),
            source: "mcp".into(),
            tool: None,
            method: "GET".into(),
            route: "/v1/projects".into(),
            product: Some("postgres".into()),
            action: Some("read".into()),
            resource: Some("a,b".into()),
            decision: "allow".into(),
            approval: None,
            status: 200,
            args: None,
            ip: None,
            duration_ms: 3,
            client: Some("@evil".into()),
        };
        let csv = super::audit_csv(std::slice::from_ref(&e));
        let row = csv.lines().nth(1).unwrap();
        assert!(row.starts_with("\"1970-01-01T00:00:00Z\",\"tok_1\",\"'=HYPERLINK(\"\"https://x.example\"\",\"\"click\"\")\""), "{row}");
        assert!(row.contains(",\"a,b\",") && row.ends_with(",\"'@evil\""), "{row}");
        // A separator inside a cell starts another cell in some spreadsheets: defused there too.
        let e2 = crate::store::AuditEvent { grant_name: Some("x; =HYPERLINK(1)|@a".into()), ..e };
        let row2 = super::audit_csv(&[e2]);
        assert!(row2.lines().nth(1).unwrap().contains("\"x; '=HYPERLINK(1)|'@a\""), "{row2}");
    }
}
