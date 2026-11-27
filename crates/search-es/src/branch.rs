//! What a branch inherits from its parent and must not run as if it were the parent.
//!
//! A branch ([`crate::ClusterConfig::branch_of`]) starts from a copy of its parent's state,
//! recurring jobs and outside storage included. Left alone, the branch would run them a second
//! time: its SLM policies would take snapshots into, and retention would delete them from, the
//! parent's bucket; its watches would send every webhook and email twice. So on a new branch's
//! first load (the host passes `branch_of` only then), before its scheduler starts,
//! [`mark_inherited`] records what it inherited (a marker naming the branch, so a branch of a
//! branch marks again) and:
//!
//! - registers every repository in a tenant's bucket (`s3`, `gcs`, `azure`) read-only, as
//!   Elastic Cloud registers another deployment's repository: listing and restoring work,
//!   creating, cloning, deleting and cleaning up are refused. Registering one again at an
//!   inherited location stays read-only unless the request says `readonly: false`
//!   ([`inherited_location`]). roll's default repository and `fs` repositories keep their
//!   contents in the cluster's own storage, which the branch has a copy of: they stay writable.
//! - pauses every SLM policy (`_roll.inherited`; `last_failure` says why): it doesn't run or
//!   apply its retention until the branch puts the policy again.
//! - deactivates every watch, as `_watcher/watch/{id}/_deactivate` does; activating one is the
//!   branch owner's decision. Its secrets (passwords, credential headers, URLs:
//!   `xpack::watcher::http`) are taken out, so it can't call the parent's webhooks until the
//!   branch puts the watch again ([`inherited_watch`], [`adopt_watch`]).
//! - records the connectors, so syncs that read a source with stored credentials stay paused
//!   on the branch ([`inherited_connector`]): no job of theirs becomes due or starts there until
//!   the branch puts the connector again or writes its configuration ([`adopt_connector`]).
//! - removes the connector and Fleet secret stores' entries (`_connector/_secret`,
//!   `_fleet/secret`: connectors' API keys, Fleet outputs' passwords) and takes out inference
//!   endpoints' keys.
//!
//! A branch's people aren't necessarily the parent's (a branch-scoped token, the branch's own
//! password, a preview's data key), so the parent's secrets stay the parent's: every secret a
//! cluster stores is sealed bound to the cluster ([`crate::seal::bind`]) and doesn't open in the
//! branch's copy, and what the branch inherited holds [`crate::seal::INHERITED`] in its place.
//! That also keeps a point-in-time branch from holding a key the parent has since rotated or
//! deleted. Inference endpoints come with the branch without their keys: one that calls a
//! provider answers "this branch doesn't have the parent's provider key; set it on the branch"
//! without calling it, until the branch's owner updates the endpoint with a key of its own
//! (`_inference/{task}/{id}/_update`, or the Inference tab's "A roll secret"). roll secrets'
//! `search-inference` sync names the parent's id and doesn't reach the branch; no key is given
//! to a branch on its own, since it may not be one roll secrets would give it. Endpoints that
//! need no key (E5 on the node, the preconfigured ones) keep working. A write that would embed
//! with such an endpoint fails the same way, unless it opts to keep the text unembedded and drop
//! the field's old embeddings (`X-Roll-Unembedded: drop`, roll previews' masking:
//! [`crate::xpack::inference::semantic::UNEMBEDDED_HEADER`]).
//!
//! Branches marked before this rule, and branches made before marking existed
//! ([`crate::ClusterConfig::parent`]), get the same as they open: [`crate::seal::seal_leftovers`]
//! takes out every secret that doesn't open as the branch's own.
//!
//! Everything else that runs on a schedule (ILM and ISM, data stream lifecycle, transforms,
//! rollups) works inside the branch's own indices and keeps running; the steps of theirs that
//! reach a repository meet the read-only registration, and ILM's cleanup of a snapshot in an
//! inherited repository leaves the parent's snapshot alone.

use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::json::Value;
use crate::meta::ClusterState;
use crate::meta::snapshots::Repository;
use crate::obj;
use crate::xpack::store;
use std::sync::Arc;

/// The marker's family and id (kept in the cluster state, shown by no API).
const FAMILY: &str = "roll.branch";
const ID: &str = "inherited";

/// Why an inherited SLM policy doesn't run (its `last_failure`).
pub const SLM_PAUSED: &str = "this policy was inherited from the parent cluster when the branch was made: it doesn't run or apply its retention on the branch. Put the policy again to run it here";

/// The branch's marker, when it is this cluster's (a branch's branch holds its parent's until
/// it marks its own).
fn marker<'a>(cluster: &Cluster, st: &'a ClusterState) -> Option<&'a Value> {
    st.metadata.xpack.get(FAMILY, ID).filter(|m| m.get("cluster").and_then(Value::as_str) == Some(me(cluster).as_str()))
}

fn me(cluster: &Cluster) -> String {
    cluster_id(cluster)
}

/// The cluster's id in roll's catalog (its name without one): what a branch's marker names, and
/// what connectors' sealed credentials are bound to.
pub fn cluster_id(cluster: &Cluster) -> String {
    cluster.config.catalog_id.clone().unwrap_or_else(|| cluster.name.clone())
}

fn listed(m: &Value, key: &str, v: &str) -> bool {
    m.get(key).and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(v)))
}

/// Whether `repo` (being registered on this cluster) reaches bucket storage the branch
/// inherited from its parent ([`Repository::bucket_place`]: the marker and this check compare
/// the same key, so another spelling of the parent's place is the parent's too).
pub fn inherited_location(cluster: &Cluster, st: &ClusterState, repo: &Repository) -> bool {
    repo.bucket_place().is_some_and(|place| marker(cluster, st).is_some_and(|m| listed(m, "repositories", &place)))
}

/// Whether repository `name` points at a location the branch inherited from its parent.
pub fn inherited_repository(cluster: &Cluster, st: &ClusterState, name: &str) -> bool {
    st.metadata.repositories.get(name).is_some_and(|r| inherited_location(cluster, st, r))
}

/// Whether connector `id` came with the branch (its syncs stay paused there).
pub fn inherited_connector(cluster: &Cluster, st: &ClusterState, id: &str) -> bool {
    marker(cluster, st).is_some_and(|m| listed(m, "connectors", id))
}

/// Whether the cluster is a branch: its host says so ([`crate::ClusterConfig::parent`],
/// [`crate::ClusterConfig::branch_of`]), or it marked what it inherited.
pub fn is_branch(cluster: &Cluster, st: &ClusterState) -> bool {
    cluster.config.parent.is_some() || cluster.config.branch_of.is_some() || marker(cluster, st).is_some()
}

/// Whether watch `id` came with the branch and wasn't put since: its secrets are the parent's.
/// On a branch that has no list of them (made before marking), every watch.
pub fn inherited_watch(cluster: &Cluster, st: &ClusterState, id: &str) -> bool {
    match marker(cluster, st) {
        Some(m) => listed(m, "watches", id),
        None => is_branch(cluster, st),
    }
}

/// Makes connector `id` the branch's own (it was put again, or its configuration written): its
/// syncs run on the branch from now on.
pub fn adopt_connector(st: &mut ClusterState, id: &str, now_ms: i64) -> EsResult<()> {
    adopt(st, "connectors", id, now_ms)
}

/// Makes watch `id` the branch's own (it was put again): its secrets are the branch's.
pub fn adopt_watch(st: &mut ClusterState, id: &str, now_ms: i64) -> EsResult<()> {
    adopt(st, "watches", id, now_ms)
}

fn adopt(st: &mut ClusterState, list: &str, id: &str, now_ms: i64) -> EsResult<()> {
    let Some(Value::Obj(mut m)) = st.metadata.xpack.get(FAMILY, ID).cloned() else { return Ok(()) };
    let Some(Value::Arr(list)) = m.get_mut(list) else { return Ok(()) };
    let before = list.len();
    list.retain(|x| x.as_str() != Some(id));
    if list.len() == before {
        return Ok(());
    }
    store::put(st, FAMILY, ID, Value::Obj(m), now_ms).map(|_| ())
}

/// Marks what a branch inherited, once (see the module docs). Call it before the cluster's
/// background work starts; a cluster that isn't a branch is left as it is.
pub async fn mark_inherited(cluster: &Cluster) -> EsResult<()> {
    let Some(parent) = cluster.config.branch_of.clone() else { return Ok(()) };
    if marker(cluster, &cluster.state()).is_some() {
        return Ok(());
    }
    let me = me(cluster);
    let now = crate::time::now_millis();
    cluster
        .update(move |st| {
            let mut locations: Vec<Value> = Vec::new();
            for r in st.metadata.repositories.values_mut() {
                let Some(place) = r.bucket_place() else { continue };
                locations.push(place.into());
                if !r.read_only() {
                    Arc::make_mut(r).settings.set("readonly", "true");
                }
            }
            let policies: Vec<(String, Value)> = st.metadata.xpack.iter(crate::xpack::slm::POLICIES).map(|(k, v)| (k.clone(), v.clone())).collect();
            for (id, rec) in &policies {
                let Value::Obj(mut r) = rec.clone() else { continue };
                let mut roll = r.get("_roll").and_then(Value::as_object).cloned().unwrap_or_default();
                roll.insert("inherited".into(), true.into());
                r.insert("_roll".into(), Value::Obj(roll));
                let reason = obj! { "type" => "illegal_state_exception", "reason" => SLM_PAUSED };
                let name = rec.pointer(&["policy", "name"]).cloned().unwrap_or(Value::Null);
                r.insert("last_failure".into(), obj! { "snapshot_name" => name, "time" => now, "details" => reason.to_json() });
                store::put(st, crate::xpack::slm::POLICIES, id, Value::Obj(r), now)?;
            }
            // OpenSearch's snapshot management policies pause the same way (put again, one is
            // the branch's own).
            let sm: Vec<(String, Value)> = st.metadata.xpack.iter(crate::opensearch::sm::POLICIES).map(|(k, v)| (k.clone(), v.clone())).collect();
            for (id, rec) in &sm {
                let Value::Obj(mut r) = rec.clone() else { continue };
                let mut roll = r.get("_roll").and_then(Value::as_object).cloned().unwrap_or_default();
                roll.insert("inherited".into(), true.into());
                r.insert("_roll".into(), Value::Obj(roll));
                store::put(st, crate::opensearch::sm::POLICIES, id, Value::Obj(r), now)?;
            }
            let watches: Vec<(String, Value)> = st.metadata.xpack.iter(crate::xpack::watcher::WATCHES).map(|(k, v)| (k.clone(), v.clone())).collect();
            for (id, w) in &watches {
                let Value::Obj(mut w) = w.clone() else { continue };
                if let Some(inner) = w.get("watch").map(crate::xpack::watcher::parse::inherited_watch) {
                    w.insert("watch".into(), inner);
                }
                let mut status = w.get("status").and_then(Value::as_object).cloned().unwrap_or_default();
                status.insert("state".into(), obj! { "active" => false, "timestamp" => crate::xpack::watcher::exec::fmt_ms(now) });
                w.insert("status".into(), Value::Obj(status));
                let mut roll = w.get("_roll").and_then(Value::as_object).cloned().unwrap_or_default();
                roll.insert("activated".into(), now.into());
                roll.shift_remove("next_run");
                w.insert("_roll".into(), Value::Obj(roll));
                store::put(st, crate::xpack::watcher::WATCHES, id, Value::Obj(w), now)?;
            }
            let connectors: Vec<Value> = st.metadata.xpack.iter(crate::xpack::entsearch::connectors::CONNECTORS).map(|(k, _)| Value::from(k.clone())).collect();
            // The parent's secrets: the stores' entries go, endpoints keep their settings.
            store::clear(st, crate::xpack::entsearch::connectors::SECRETS);
            store::clear(st, crate::xpack::fleet::SECRETS);
            for family in crate::xpack::inference::SECRET_FAMILIES {
                let keys: Vec<(String, Value)> = st.metadata.xpack.iter(family).map(|(k, v)| (k.clone(), v.clone())).collect();
                for (id, rec) in keys {
                    let Value::Obj(m) = rec else { continue };
                    let none = m.into_iter().map(|(k, v)| (k, if v.is_null() { v } else { Value::from(crate::seal::INHERITED) })).collect();
                    store::put(st, family, &id, Value::Obj(none), now)?;
                }
            }
            let m = obj! {
                "cluster" => me,
                "parent" => parent,
                "marked_at" => now,
                "repositories" => Value::Arr(locations),
                "slm" => Value::Arr(policies.into_iter().map(|(k, _)| Value::from(k)).collect()),
                "watches" => Value::Arr(watches.into_iter().map(|(k, _)| Value::from(k)).collect()),
                "connectors" => Value::Arr(connectors),
            };
            store::put(st, FAMILY, ID, m, now).map(|_| ())
        })
        .await
}

/// Whether an SLM policy record came with the branch and is paused there.
pub fn paused_policy(rec: &Value) -> bool {
    rec.pointer(&["_roll", "inherited"]).and_then(Value::as_bool) == Some(true)
}
