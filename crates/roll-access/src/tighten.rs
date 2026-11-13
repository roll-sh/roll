//! "Tighten to what it used" (docs/permissions.md): a token's rules narrowed to the actions and
//! resources its allowed requests used, as AWS's access analyzer makes a policy from activity.
//!
//! Each proposed rule is within the rule it came from: the same product (one rule per product a
//! `*` rule was used for), environments, paths and effects (`ask` stays `ask`), fewer actions, and
//! for resources the ids it used when the rule named every resource or ids already. A rule placed
//! by project keeps its project (a resource can leave the project; its id would still match). An
//! action used without a resource (a list, the product as a whole) keeps the rule's resources.
//! What a person allowed for a while is never made permanent here: only the token's own rules are
//! narrowed.

use roll_auth::scope::{Action, Effect, Resources, Rule, Scope, Selector};
use std::collections::{BTreeMap, BTreeSet};

/// One kind of allowed request in the audit log.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Used {
    pub product: String,
    pub action: Action,
    /// What it was about (`None`: a list or the product as a whole).
    pub resource: Option<String>,
    pub count: i64,
    /// Unix ms.
    pub last_at: i64,
}

/// At most this many resources named one by one; a rule used on more keeps its own resources.
const MAX_IDS: usize = 20;

/// A resource as the audit log names it, as a rule selects it (search's `cl_1/logs-a` is cluster
/// `cl_1`, index `logs-a`).
fn selector(product: &str, resource: &str) -> Selector {
    match resource.split_once('/') {
        Some((id, index)) if product == "search" => Selector::Id { id: id.into(), index: Some(index.into()) },
        _ => Selector::id(resource),
    }
}

/// `rules` narrowed to what `used` shows: never wider than `rules`, and the same rules when
/// nothing was used under them (a token unused for the period is better revoked than emptied).
pub fn tighten(rules: &Scope, used: &[Used]) -> Scope {
    if used.is_empty() {
        return rules.clone();
    }
    let mut out = Vec::new();
    for r in &rules.0 {
        let under: Vec<&Used> = used.iter().filter(|u| (r.product == "*" || r.product == u.product) && r.actions.contains_key(&u.action)).collect();
        let products: BTreeSet<&str> = under.iter().map(|u| u.product.as_str()).collect();
        for p in products {
            let (mut wide, mut narrow): (BTreeMap<Action, Effect>, BTreeMap<Action, Effect>) = Default::default();
            let mut ids = BTreeSet::new();
            for u in under.iter().filter(|u| u.product == p) {
                let effect = r.actions[&u.action];
                match &u.resource {
                    None => wide.insert(u.action, effect),
                    Some(id) => {
                        ids.insert(id.as_str());
                        narrow.insert(u.action, effect)
                    }
                };
            }
            narrow.retain(|a, _| !wide.contains_key(a));
            let base = Rule { product: p.to_string(), resources: r.resources.clone(), environments: r.environments.clone(), paths: r.paths.clone(), actions: BTreeMap::new() };
            if !wide.is_empty() {
                out.push(Rule { actions: wide, ..base.clone() });
            }
            if !narrow.is_empty() {
                let mut rule = Rule { actions: narrow, ..base };
                let named = Rule { resources: Resources::Some(ids.iter().map(|id| selector(p, id)).collect()), ..rule.clone() };
                if ids.len() <= MAX_IDS && Scope(vec![r.clone()]).covers(&Scope(vec![named.clone()])) {
                    rule = named;
                }
                out.push(rule);
            }
        }
    }
    let proposed = Scope(out);
    if proposed.0.is_empty() || !rules.covers(&proposed) { rules.clone() } else { proposed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope(v: serde_json::Value) -> Scope {
        serde_json::from_value(v).unwrap()
    }
    fn used(product: &str, action: Action, resource: Option<&str>) -> Used {
        Used { product: product.into(), action, resource: resource.map(str::to_string), count: 1, last_at: 0 }
    }

    /// Narrowed to the actions and resources used, per product, within each rule; asks stay asks;
    /// a project's rule keeps its project; lists keep the rule's resources; never wider.
    #[test]
    fn narrows_to_what_was_used() {
        let rules = scope(json!([
            { "product": "*", "resources": "*", "environments": ["staging"], "actions": { "read": "allow", "write": "allow", "delete": "ask" } },
            { "product": "redis", "resources": [{ "project": "p1" }], "actions": { "read": "allow", "write": "allow" } },
        ]));
        let got = tighten(&rules, &[
            used("postgres", Action::Read, Some("prj_a")),
            used("postgres", Action::Delete, Some("prj_a")),
            used("search", Action::Read, Some("cl_1/logs-a")),
            used("redis", Action::Read, None),
            used("redis", Action::Write, Some("rdb_1")),
        ]);
        assert!(rules.covers(&got), "never wider");
        let want = scope(json!([
            { "product": "postgres", "resources": [{ "id": "prj_a" }], "environments": ["staging"], "actions": { "read": "allow", "delete": "ask" } },
            { "product": "redis", "resources": "*", "environments": ["staging"], "actions": { "read": "allow" } },
            { "product": "redis", "resources": [{ "id": "rdb_1" }], "environments": ["staging"], "actions": { "write": "allow" } },
            { "product": "search", "resources": [{ "id": "cl_1", "index": "logs-a" }], "environments": ["staging"], "actions": { "read": "allow" } },
            { "product": "redis", "resources": [{ "project": "p1" }], "actions": { "read": "allow" } },
            { "product": "redis", "resources": [{ "project": "p1" }], "actions": { "write": "allow" } },
        ]));
        assert_eq!(got, want);
        assert_eq!(tighten(&rules, &[]), rules, "nothing used: unchanged");
    }
}
