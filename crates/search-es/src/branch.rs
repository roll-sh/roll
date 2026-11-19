//! What a branch inherits from its parent and must not run as if it were the parent.
//!
//! A branch ([`crate::ClusterConfig::branch_of`]) starts from a copy of its parent's state,
//! recurring jobs and outside storage included. Left alone, the branch would run them a second
//! time: its SLM policies would take snapshots into, and retention would delete them from, the
//! parent's bucket; its watches would send every webhook and email twice. So the first time a
//! branch loads, before its scheduler starts, [`mark_inherited`] records what it inherited
//! (a marker naming the branch, so a branch of a branch marks again) and:
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
//!   branch owner's decision.
//! - records the connectors, so syncs that read a source with stored credentials stay paused
//!   on the branch ([`inherited_connector`]).
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
    cluster.config.catalog_id.clone().unwrap_or_else(|| cluster.name.clone())
}

fn listed(m: &Value, key: &str, v: &str) -> bool {
    m.get(key).and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(v)))
}

/// Whether `repo` (being registered on this cluster) points at a location the branch
/// inherited from its parent.
pub fn inherited_location(cluster: &Cluster, st: &ClusterState, repo: &Repository) -> bool {
    repo.in_bucket() && marker(cluster, st).is_some_and(|m| listed(m, "repositories", &repo.location_key()))
}

/// Whether repository `name` points at a location the branch inherited from its parent.
pub fn inherited_repository(cluster: &Cluster, st: &ClusterState, name: &str) -> bool {
    st.metadata.repositories.get(name).is_some_and(|r| inherited_location(cluster, st, r))
}

/// Whether connector `id` came with the branch (its syncs stay paused there).
pub fn inherited_connector(cluster: &Cluster, st: &ClusterState, id: &str) -> bool {
    marker(cluster, st).is_some_and(|m| listed(m, "connectors", id))
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
                if !r.in_bucket() {
                    continue;
                }
                locations.push(r.location_key().into());
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
            let watches: Vec<(String, Value)> = st.metadata.xpack.iter(crate::xpack::watcher::WATCHES).map(|(k, v)| (k.clone(), v.clone())).collect();
            for (id, w) in &watches {
                let Value::Obj(mut w) = w.clone() else { continue };
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
