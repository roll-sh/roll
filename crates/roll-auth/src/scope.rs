//! What a scoped token may do (docs/permissions.md): rules of products × resources × actions, each
//! action `allow` or `ask`. Pure: no I/O. The evaluator in `roll-access` finds a request's
//! [`Target`] and asks [`Scope::outcome`]; products that need a second check inside one request
//! (a secrets promote reading another environment) ask [`crate::Grant::allows`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Every route is exactly one action on one product. Actions don't imply each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// See metadata and data.
    Read,
    /// Create and change.
    Write,
    /// Destroy, or make irreversible.
    Delete,
    /// See a credential or a secret value.
    Reveal,
    /// Change production: land on a default branch, deploy, roll back or promote to production.
    Deploy,
    /// Manage access and settings.
    Admin,
}

impl Action {
    pub const ALL: [Action; 6] = [Action::Read, Action::Write, Action::Delete, Action::Reveal, Action::Deploy, Action::Admin];

    pub fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Write => "write",
            Action::Delete => "delete",
            Action::Reveal => "reveal",
            Action::Deploy => "deploy",
            Action::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.as_str() == s)
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a rule says about an action it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    /// Pause the request until a person approves it.
    Ask,
    Allow,
}

/// What the scope decides for one action on one target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Deny,
    Ask,
    Allow,
}

impl From<Effect> for Outcome {
    fn from(e: Effect) -> Self {
        match e {
            Effect::Ask => Outcome::Ask,
            Effect::Allow => Outcome::Allow,
        }
    }
}

/// The products a rule can name (`"*"` names them all). New products add their name here.
pub const PRODUCTS: &[&str] = &[
    "workspace", "postgres", "redis", "storage", "streams", "nosql", "queues", "tasks", "code", "agents", "secrets", "apps",
    "previews", "search", "observability", "access",
];

/// One resource: by its product's own id, or every resource in a workspace project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Selector {
    Project {
        project: String,
    },
    Id {
        id: String,
        /// Only some indices of it (search: `logs-*`, `a,b`), as Elasticsearch names them: each
        /// name or pattern a request's index expression lists must be one of these, or start with
        /// what's before one's trailing `*`. A request about the resource as a whole never matches.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<String>,
    },
}

impl Selector {
    pub fn id(id: impl Into<String>) -> Selector {
        Selector::Id { id: id.into(), index: None }
    }

    fn matches(&self, r: &Resource) -> bool {
        match self {
            Selector::Id { id, index } => {
                r.id.as_deref() == Some(id.as_str())
                    && match index {
                        None => true,
                        Some(mine) => r.index.as_deref().is_some_and(|asked| index_covers(mine, asked)),
                    }
            }
            Selector::Project { project } => r.project.as_deref() == Some(project) || r.also_in.contains(project),
        }
    }

    /// Whether everything `other` selects, this selects too.
    fn covers(&self, other: &Selector) -> bool {
        match (self, other) {
            (Selector::Project { project: a }, Selector::Project { project: b }) => a == b,
            (Selector::Id { id: a, index: None }, Selector::Id { id: b, .. }) => a == b,
            (Selector::Id { id: a, index: Some(mine) }, Selector::Id { id: b, index: Some(theirs) }) => a == b && index_covers(mine, theirs),
            _ => false,
        }
    }
}

/// The names and patterns an index expression lists (`logs-*,-logs-old` → `logs-*`): exclusions
/// only narrow what the others reach, so only these need covering. `_all` is `*`.
fn index_names(expr: &str) -> impl Iterator<Item = &str> {
    expr.split(',').map(str::trim).filter(|n| !n.is_empty() && !n.starts_with('-')).map(|n| if n == "_all" { "*" } else { n })
}

/// Whether the indices `mine` lists cover every name or pattern `asked` lists. A pattern covers a
/// name or pattern that starts with what's before its trailing `*`; date math (`<logs-{now/d}>`)
/// and other clusters' indices (`remote:logs`) only by `*`, and never at all, respectively.
fn index_covers(mine: &str, asked: &str) -> bool {
    let mine: Vec<&str> = index_names(mine).collect();
    let mut asked = index_names(asked).peekable();
    asked.peek().is_some()
        && asked.all(|n| {
            !n.contains(':')
                && mine.iter().any(|m| {
                    *m == "*" || (!n.starts_with('<') && (*m == n || m.strip_suffix('*').is_some_and(|prefix| !prefix.is_empty() && n.starts_with(prefix))))
                })
        })
}

/// `"*"` (every resource, including ones made later) or a list of selectors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resources {
    All,
    Some(Vec<Selector>),
}

impl Serialize for Resources {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Resources::All => s.serialize_str("*"),
            Resources::Some(v) => v.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Resources {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Star(String),
            List(Vec<Selector>),
        }
        match Raw::deserialize(d)? {
            Raw::Star(s) if s == "*" => Ok(Resources::All),
            Raw::Star(s) => Err(serde::de::Error::custom(format!("resources must be \"*\" or a list, not {s:?}"))),
            Raw::List(v) => Ok(Resources::Some(v)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// A name from [`PRODUCTS`], or `"*"`.
    pub product: String,
    pub resources: Resources,
    /// Only resources whose environment the product knows and is one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environments: Option<Vec<String>>,
    /// Only resources with a path under one of these (`/api` covers `/api/x`, not `/apix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    pub actions: BTreeMap<Action, Effect>,
}

/// One line of text with no backticks: what a rule names is shown to agents and people as text.
fn plain(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() || c == '`')
}

/// Whether environment entry `pattern` covers `env` (a name, or another entry): the same name, or,
/// for a pattern ending in `*`, any name starting with what's before it (`pv-*` covers `pv-42`, a
/// preview's own environment).
fn env_covers(pattern: &str, env: &str) -> bool {
    pattern == env || pattern.strip_suffix('*').is_some_and(|prefix| !prefix.is_empty() && env.starts_with(prefix))
}

/// One resource a request is about, as far as the evaluator found it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resource {
    pub product: String,
    /// The product's own id (`None`: the product as a whole).
    pub id: Option<String>,
    /// The workspace project it's in, at the time of the request.
    pub project: Option<String>,
    /// Other workspace projects it's in too (each user places resources in their own projects).
    /// Build a `Resource` with `..Default::default()` so fields like this one can be added.
    pub also_in: Vec<String>,
    pub environment: Option<String>,
    pub path: Option<String>,
    /// The indices of it the request names (search's `{index}`: an index expression).
    pub index: Option<String>,
}

impl Resource {
    pub fn new(product: &str, id: &str) -> Self {
        Resource { product: product.into(), id: Some(id.into()), ..Default::default() }
    }
}

/// What a request is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// One resource.
    One(Resource),
    /// A list of a product's resources: rules that name resources filter it.
    List { product: String },
    /// A new resource, made in a workspace project when one is named.
    Create { product: String, project: Option<String>, environment: Option<String> },
    /// The product as a whole (settings, its usage): only rules for every resource.
    Product { product: String },
}

impl Target {
    pub fn product(&self) -> &str {
        match self {
            Target::One(r) => &r.product,
            Target::List { product } | Target::Create { product, .. } | Target::Product { product } => product,
        }
    }
}

fn under(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    prefix.is_empty() || path == prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
}

impl Rule {
    fn product_matches(&self, product: &str) -> bool {
        self.product == "*" || self.product == product
    }

    fn environment_matches(&self, env: Option<&str>) -> bool {
        match &self.environments {
            None => true,
            Some(list) => env.is_some_and(|e| list.iter().any(|l| env_covers(l, e))),
        }
    }

    fn path_matches(&self, path: Option<&str>) -> bool {
        match &self.paths {
            None => true,
            Some(list) => path.is_some_and(|p| list.iter().any(|l| under(p, l))),
        }
    }

    /// Whether this rule is about `target` at all (whatever its actions).
    pub fn matches(&self, target: &Target) -> bool {
        if !self.product_matches(target.product()) {
            return false;
        }
        match target {
            Target::One(r) => {
                let resource = match &self.resources {
                    Resources::All => true,
                    Resources::Some(sel) => sel.iter().any(|s| s.matches(r)),
                };
                resource && self.environment_matches(r.environment.as_deref()) && self.path_matches(r.path.as_deref())
            }
            Target::List { .. } => true,
            Target::Create { project, environment, .. } => {
                let resource = match &self.resources {
                    Resources::All => true,
                    Resources::Some(sel) => project.as_ref().is_some_and(|p| sel.iter().any(|s| matches!(s, Selector::Project { project } if project == p))),
                };
                resource && self.environment_matches(environment.as_deref()) && self.paths.is_none()
            }
            Target::Product { .. } => self.resources == Resources::All && self.environments.is_none() && self.paths.is_none(),
        }
    }

    /// Whether everything `other` allows, this rule allows too (and asks no less than it).
    fn covers(&self, other: &Rule) -> bool {
        let product = self.product == "*" || self.product == other.product;
        let resources = match (&self.resources, &other.resources) {
            (Resources::All, _) => true,
            (Resources::Some(_), Resources::All) => false,
            (Resources::Some(mine), Resources::Some(theirs)) => theirs.iter().all(|t| mine.iter().any(|m| m.covers(t))),
        };
        let environments = match (&self.environments, &other.environments) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(mine), Some(theirs)) => theirs.iter().all(|t| mine.iter().any(|m| env_covers(m, t))),
        };
        let paths = match (&self.paths, &other.paths) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(mine), Some(theirs)) => theirs.iter().all(|t| mine.iter().any(|m| under(t, m))),
        };
        let actions = other.actions.iter().all(|(a, e)| self.actions.get(a).is_some_and(|mine| mine >= e));
        product && resources && environments && paths && actions
    }
}

/// A grant's rules. Anything no rule allows is denied.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scope(pub Vec<Rule>);

impl Scope {
    /// The most permissive outcome among the rules about `target` that name `action`.
    pub fn outcome(&self, target: &Target, action: Action) -> Outcome {
        self.0
            .iter()
            .filter(|r| r.matches(target))
            .filter_map(|r| r.actions.get(&action).copied().map(Outcome::from))
            .max()
            .unwrap_or(Outcome::Deny)
    }

    /// Whether any rule names this product with this action (what `tools/list` shows).
    pub fn may(&self, product: &str, action: Action) -> bool {
        self.0.iter().any(|r| r.product_matches(product) && r.actions.contains_key(&action))
    }

    /// Whether this scope allows everything `other` does. Conservative: each of `other`'s rules
    /// must be covered by one rule here.
    pub fn covers(&self, other: &Scope) -> bool {
        other.0.iter().all(|t| self.0.iter().any(|m| m.covers(t)))
    }

    /// A list of problems, empty when the scope is well formed.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.0.is_empty() {
            out.push("a token needs at least one rule".to_string());
        }
        if self.0.len() > 64 {
            out.push("at most 64 rules".to_string());
        }
        for (i, r) in self.0.iter().enumerate() {
            let at = format!("rule {}", i + 1);
            if r.product != "*" && !PRODUCTS.contains(&r.product.as_str()) {
                out.push(format!("{at}: unknown product {:?} (one of *, {})", r.product, PRODUCTS.join(", ")));
            }
            if r.actions.is_empty() {
                out.push(format!("{at}: names no actions"));
            }
            match &r.resources {
                Resources::Some(v) if v.is_empty() => out.push(format!("{at}: resources is empty (use \"*\" for all)")),
                Resources::Some(v) if v.len() > 256 => out.push(format!("{at}: at most 256 resources")),
                Resources::Some(v) => {
                    for s in v {
                        let (Selector::Project { project: x } | Selector::Id { id: x, .. }) = s;
                        if x.is_empty() || x.len() > 200 || !plain(x) {
                            out.push(format!("{at}: a resource id must be 1-200 characters, one line without backticks"));
                        }
                        if let Selector::Id { index: Some(ix), .. } = s
                            && (index_names(ix).next().is_none() || ix.len() > 200 || !plain(ix))
                        {
                            out.push(format!("{at}: an index is 1-200 characters naming at least one index or pattern"));
                        }
                    }
                }
                Resources::All => {}
            }
            for (name, list) in [("environments", &r.environments), ("paths", &r.paths)] {
                if let Some(l) = list
                    && (l.is_empty() || l.len() > 32 || l.iter().any(|e| e.is_empty() || e.len() > 200 || !plain(e)))
                {
                    out.push(format!("{at}: {name} must list 1-32 non-empty names"));
                }
            }
            if r.environments.as_ref().is_some_and(|l| l.iter().any(|e| e == "*")) {
                out.push(format!("{at}: \"*\" alone isn't an environment (leave environments out for all of them)"));
            }
            if let Some(paths) = &r.paths
                && paths.iter().any(|p| !p.starts_with('/'))
            {
                out.push(format!("{at}: paths start with /"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(json: serde_json::Value) -> Rule {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn an_environment_ending_in_a_star_is_a_prefix() {
        let r: Rule = serde_json::from_value(serde_json::json!({ "product": "apps", "resources": "*", "environments": ["preview", "pv-*"], "actions": { "write": "allow" } })).unwrap();
        assert!(r.environment_matches(Some("pv-42")) && r.environment_matches(Some("preview")));
        assert!(!r.environment_matches(Some("production")) && !r.environment_matches(None));
        let narrower: Rule = serde_json::from_value(serde_json::json!({ "product": "apps", "resources": "*", "environments": ["pv-42"], "actions": { "write": "allow" } })).unwrap();
        assert!(r.covers(&narrower) && !narrower.covers(&r));
    }

    /// A selector's indices cover an index expression name by name and pattern by pattern, as
    /// Elasticsearch authorizes one; the cluster as a whole is never theirs.
    #[test]
    fn indices_narrow_a_resource() {
        let r = rule(serde_json::json!({ "product": "search", "resources": [{ "id": "cl_1", "index": "logs-*,metrics" }], "actions": { "read": "allow" } }));
        let at = |index: Option<&str>| Target::One(Resource { index: index.map(str::to_string), ..Resource::new("search", "cl_1") });
        for ok in ["logs-2025", "logs-2025-*", "logs-*,-logs-old", "metrics", "logs-a,metrics"] {
            assert!(r.matches(&at(Some(ok))), "{ok}");
        }
        for no in ["logs", "l*", "*", "_all", "logs-a,users", "remote:logs-a", "<logs-{now/d}>", "-logs-a", ""] {
            assert!(!r.matches(&at(Some(no))), "{no}");
        }
        assert!(!r.matches(&at(None)), "the cluster itself");
        assert!(!r.matches(&Target::One(Resource { index: Some("logs-a".into()), ..Resource::new("search", "cl_2") })));
        let whole = rule(serde_json::json!({ "product": "search", "resources": [{ "id": "cl_1" }], "actions": { "read": "allow" } }));
        let one = rule(serde_json::json!({ "product": "search", "resources": [{ "id": "cl_1", "index": "logs-a" }], "actions": { "read": "allow" } }));
        assert!(whole.matches(&at(Some("anything"))) && whole.covers(&r) && r.covers(&one) && !one.covers(&r) && !r.covers(&whole));
        assert!(!Scope(vec![rule(serde_json::json!({ "product": "search", "resources": [{ "id": "cl_1", "index": "-x" }], "actions": { "read": "allow" } }))]).problems().is_empty());
    }

    /// Resources, environments and paths narrow a rule; the most permissive matching rule wins;
    /// a narrower scope is covered and a wider one isn't.
    #[test]
    fn rules_narrow_and_cover() {
        let scope = Scope(vec![
            rule(serde_json::json!({ "product": "postgres", "resources": [{ "project": "prj_a" }], "actions": { "read": "allow", "delete": "ask" } })),
            rule(serde_json::json!({ "product": "secrets", "resources": "*", "environments": ["development"], "paths": ["/api"], "actions": { "reveal": "allow" } })),
            rule(serde_json::json!({ "product": "*", "resources": "*", "actions": { "read": "ask" } })),
        ]);
        let in_a = Target::One(Resource { project: Some("prj_a".into()), ..Resource::new("postgres", "db1") });
        let elsewhere = Target::One(Resource::new("postgres", "db2"));
        assert_eq!(scope.outcome(&in_a, Action::Read), Outcome::Allow);
        assert_eq!(scope.outcome(&elsewhere, Action::Read), Outcome::Ask);
        assert_eq!(scope.outcome(&in_a, Action::Delete), Outcome::Ask);
        assert_eq!(scope.outcome(&elsewhere, Action::Delete), Outcome::Deny);
        assert_eq!(scope.outcome(&in_a, Action::Write), Outcome::Deny);
        let secret = |env: Option<&str>, path: &str| {
            Target::One(Resource { environment: env.map(Into::into), path: Some(path.into()), ..Resource::new("secrets", "prj_a") })
        };
        assert_eq!(scope.outcome(&secret(Some("development"), "/api/db"), Action::Reveal), Outcome::Allow);
        assert_eq!(scope.outcome(&secret(Some("development"), "/apix"), Action::Reveal), Outcome::Deny);
        assert_eq!(scope.outcome(&secret(Some("production"), "/api"), Action::Reveal), Outcome::Deny);
        assert_eq!(scope.outcome(&secret(None, "/api"), Action::Reveal), Outcome::Deny);
        // Creating needs the project named; the product as a whole needs "*".
        let create = |p: Option<&str>| Target::Create { product: "postgres".into(), project: p.map(Into::into), environment: None };
        let one = Scope(vec![rule(serde_json::json!({ "product": "postgres", "resources": [{ "project": "prj_a" }], "actions": { "write": "allow" } }))]);
        assert_eq!(one.outcome(&create(Some("prj_a")), Action::Write), Outcome::Allow);
        assert_eq!(one.outcome(&create(None), Action::Write), Outcome::Deny);
        assert_eq!(one.outcome(&Target::Product { product: "postgres".into() }, Action::Write), Outcome::Deny);
        // Covering: never more than the maker.
        assert!(!scope.covers(&one));
        let wide = Scope(vec![rule(serde_json::json!({ "product": "*", "resources": "*", "actions": { "write": "allow", "read": "allow" } }))]);
        assert!(wide.covers(&one) && !one.covers(&wide));
        let asks = Scope(vec![rule(serde_json::json!({ "product": "postgres", "resources": "*", "actions": { "read": "ask" } }))]);
        assert!(scope.covers(&asks) && !asks.covers(&Scope(vec![rule(serde_json::json!({ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }))])));
        assert!(Scope(vec![]).problems().len() == 1 && serde_json::from_value::<Rule>(serde_json::json!({ "product": "x", "resources": "all", "actions": {} })).is_err());
    }
}
