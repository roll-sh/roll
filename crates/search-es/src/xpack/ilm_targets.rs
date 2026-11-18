//! The ILM actions that make a new index from a managed one and swap it in: `shrink`
//! (`ShrinkAction`), `downsample` (`DownsampleAction`) and `searchable_snapshot`
//! (`SearchableSnapshotAction`), with Elasticsearch's steps and step names.
//!
//! Each names its new index in the execution state (`shrink_index_name`, `rollup_index_name`,
//! the snapshot and its repository, `force_merge_clone_index_name`), makes it through roll's
//! own APIs (`_shrink`, `_downsample`, `_clone`, `_forcemerge`, a snapshot and `_mount`), then
//! hands it the execution state (`copy-execution-state`: the step after the action, or
//! `is-shrunken-index` for a shrink) and the policy (`copy-settings`), and swaps it in for the
//! source: in place of the source among its data stream's backing indices (keeping its
//! position; the generation moves on), or by moving the source's aliases to it (without
//! `is_write_index`; shrink and searchable snapshot also add an alias named after the source)
//! while deleting the source in the same request. The steps that wait on shard allocation
//! (`set-single-node-allocation`, `check-shrink-allocation`, `shrunk-shards-allocated`,
//! `wait-for-index-color`, `wait-for-data-tier`) have nothing to wait for on roll's storage and
//! go on at once, as they do on a single green node. A resized or cloned index is made with
//! `index.lifecycle.skip` (ILM leaves it alone until it has its execution state), as
//! `ResizeIndexStep` makes it.
//!
//! A searchable snapshot is taken into the action's repository and mounted from it as
//! `restored-<index>` (`full_copy`) or, in the frozen phase, `partial-<index>`
//! (`shared_cache`): what `_mount` makes (docs/search/compatibility.md, "Searchable
//! snapshots"). The delete action's `cleanup-snapshot` deletes that snapshot again.

use super::ilm_run::{Env, Exec, Outcome, fail_with, write_index_of};
use crate::error::EsError;
use crate::json::Value;
use crate::meta::{ClusterState, IndexMeta};
use crate::obj;
use crate::units::ByteSize;
use crate::xpack::{enc, store};
use std::sync::Arc;

/// Whether a shrink's `number_of_shards` divides the index's shards (`CheckTargetShardsCountStep`).
pub(super) fn shards_divide(idx: &IndexMeta, conf: &Value) -> bool {
    match conf.get("number_of_shards").and_then(Value::as_i64) {
        Some(n) if n > 0 => idx.number_of_shards() as i64 % n == 0,
        _ => true,
    }
}

/// Whether the index a shrunk index was made from still exists (`ShrunkenIndexCheckStep`).
pub(super) fn shrink_source_left(st: &ClusterState, idx: &IndexMeta) -> bool {
    idx.settings.get("index.resize.source.name").is_some_and(|s| st.metadata.indices.contains_key(s))
}

/// `GenerateUniqueIndexNameStep.generateValidIndexName`: `<prefix><4 random chars>-<index>`.
fn unique_name(prefix: &str, index: &str) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let tag: String = (0..4).map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char).collect();
    format!("{prefix}{tag}-{index}")
}

/// The index the action makes from `idx`.
fn target_of(e: &Exec, action: &str, idx: &IndexMeta) -> Option<String> {
    match action {
        "shrink" => e.shrink_index_name.clone(),
        "downsample" => e.downsample_index_name.clone(),
        "searchable_snapshot" => Some(format!("{}{}", mount_prefix(e), idx.name)),
        _ => None,
    }
}

/// `restored-`, or `partial-` for a mount in the frozen phase.
fn mount_prefix(e: &Exec) -> &'static str {
    if e.phase.as_deref() == Some("frozen") { "partial-" } else { "restored-" }
}

fn missing_target(env: &Env<'_>, target: &str) -> Outcome {
    fail_with(format!("target index [{target}] doesn't exist. stopping execution of lifecycle [{}] for index [{}]", env.policy, env.index))
}

pub(super) async fn execute(env: &Env<'_>, e: &mut Exec, idx: &IndexMeta, action: &str, step: &str, conf: &Value) -> Outcome {
    let i = env.index.as_str();
    let st = env.cluster.state();
    let exists = |name: &str| st.metadata.indices.contains_key(name);
    let target = target_of(e, action, idx);
    match (action, step) {
        // ------------------------------------------------------------------------- shrink
        ("shrink", "branch-check-prerequisites") => {
            drop(st);
            let shards = idx.number_of_shards() as i64;
            let wanted = match conf.get("number_of_shards").and_then(Value::as_i64) {
                Some(t) => t,
                None => {
                    let max = conf.get("max_primary_shard_size").and_then(|v| ByteSize::parse(&v.to_es_string(), "").ok()).map(|b| b.bytes()).unwrap_or(i64::MAX).max(1);
                    let size = match env.call("GET", &format!("/{}/_stats/store", enc(i)), None).await {
                        Ok(v) => v.pointer(&["_all", "primaries", "store", "size_in_bytes"]).and_then(Value::as_i64).unwrap_or(0),
                        Err(_) => 0,
                    };
                    (size.saturating_add(max - 1) / max).clamp(1, shards.max(1))
                }
            };
            if shards != wanted && idx.settings.get("index.store.snapshot.index_name").is_none() {
                Outcome::Next
            } else if conf.get("allow_write_after_shrink").and_then(Value::as_bool) == Some(true) {
                Outcome::Goto("update-settings")
            } else {
                Outcome::SkipAction
            }
        }
        ("shrink", "check-target-shards-count") => {
            if shards_divide(idx, conf) {
                return Outcome::Next;
            }
            let n = conf.get("number_of_shards").map(Value::to_es_string).unwrap_or_default();
            Outcome::Wait(Some(obj! { "message" => format!(
                "lifecycle action of policy [{}] for index [{i}] cannot make progress because the target shards count [{n}] must be a factor of the source index's shards count [{}]",
                env.policy,
                idx.number_of_shards()
            ) }))
        }
        ("shrink", "cleanup-shrink-index") => {
            drop(st);
            cleanup_generated(env, e.shrink_index_name.as_deref()).await
        }
        ("shrink", "generate-index-name") => {
            e.shrink_index_name = Some(unique_name("shrink-", i));
            Outcome::Next
        }
        ("shrink", "set-single-node-allocation" | "check-shrink-allocation") => Outcome::Next,
        ("shrink", "shrink") => {
            let Some(target) = target else { return fail_with(format!("no shrink index name was generated for index [{i}]")) };
            // `ResizeIndexStep`: a target made by an earlier attempt is kept.
            if exists(&target) {
                return Outcome::Next;
            }
            drop(st);
            let mut settings = obj! { "index.number_of_replicas" => idx.number_of_replicas() as i64, "index.lifecycle.skip" => true };
            let mut body = crate::json::Map::new();
            match conf.get("number_of_shards") {
                Some(n) => {
                    if let Value::Obj(m) = &mut settings {
                        m.insert("index.number_of_shards".into(), n.clone());
                    }
                }
                None => {
                    if let Some(m) = conf.get("max_primary_shard_size") {
                        body.insert("max_primary_shard_size".into(), m.clone());
                    }
                }
            }
            body.insert("settings".into(), settings);
            match env.act("POST", &format!("/{}/_shrink/{}", enc(i), enc(&target)), Some(Value::Obj(body))).await {
                Outcome::Next => mark_resized(env, &target).await,
                other => other,
            }
        }
        ("shrink", "shrunk-shards-allocated") => match target {
            Some(t) if exists(&t) => Outcome::Next,
            _ => Outcome::Wait(Some(obj! { "message" => "Waiting for shrunk index to be created" })),
        },
        // Run by the shrunk index, which got the execution state at this step.
        ("shrink", "is-shrunken-index") => {
            let Some(source) = idx.settings.get("index.resize.source.name").map(str::to_string) else {
                return fail_with(format!("step[is-shrunken-index] is checking an un-shrunken index[{i}]"));
            };
            if exists(&source) {
                return Outcome::Wait(Some(obj! { "original_index_name" => source, "message" => "shrunk index waiting for original index to be deleted" }));
            }
            Outcome::Next
        }
        ("shrink", "update-settings") => {
            drop(st);
            env.settings(obj! { "index.blocks.write" => false }).await
        }
        ("shrink", "aliases") => {
            drop(st);
            swap_aliases(env, idx, target, true).await
        }
        // --------------------------------------------------------------------- downsample
        ("downsample", "branch-on-timeseries-check") => {
            if idx.settings.get("index.mode") == Some("time_series") { Outcome::Next } else { Outcome::SkipAction }
        }
        ("downsample", "cleanup-target-index") => Outcome::Next,
        // `DownsamplePrepareLifeCycleStateStep`: `downsample-<interval>-<origin index>`.
        ("downsample", "generate-downsampled-index-name") => {
            let interval = conf.get("fixed_interval").map(Value::to_es_string).unwrap_or_default();
            let origin = idx.settings.get("index.downsample.origin.name").unwrap_or(i);
            e.downsample_index_name = Some(format!("downsample-{interval}-{origin}"));
            Outcome::Next
        }
        ("downsample", "rollup") => {
            let Some(target) = target else { return fail_with(format!("no downsample index name was generated for index [{i}]")) };
            drop(st);
            let mut body = obj! { "fixed_interval" => conf.get("fixed_interval").cloned().unwrap_or(Value::Null) };
            if let (Some(m), Value::Obj(b)) = (conf.get("sampling_method"), &mut body) {
                b.insert("sampling_method".into(), m.clone());
            }
            env.act("POST", &format!("/{}/_downsample/{}", enc(i), enc(&target)), Some(body)).await
        }
        // ------------------------------------------------------------ searchable_snapshot
        // An index already mounted from this repository with this storage skips the action;
        // one mounted from another repository too (Elasticsearch only logs it).
        ("searchable_snapshot", "branch-check-prerequisites") => match idx.settings.get("index.store.snapshot.repository_name") {
            None => Outcome::Next,
            Some(repo) => {
                let same_storage = (idx.settings.get("index.store.snapshot.partial") == Some("true")) == (mount_prefix(e) == "partial-");
                if repo == repository(conf) && !same_storage { Outcome::Next } else { Outcome::SkipAction }
            }
        },
        // A mounted index (converted in a later phase) is mounted again from its snapshot.
        ("searchable_snapshot", "branch-check-existing-snapshot") => {
            let mounted = idx.settings.get("index.store.snapshot.snapshot_name").map(str::to_string);
            match mounted {
                Some(snap) => {
                    e.snapshot_repository = idx.settings.get("index.store.snapshot.repository_name").map(str::to_string);
                    e.snapshot_name = Some(snap);
                    Outcome::Goto("wait-for-data-tier")
                }
                None if conf.get("force_merge_index").and_then(Value::as_bool) == Some(false) => Outcome::Goto("generate-snapshot-name"),
                None => Outcome::Next,
            }
        }
        // Without replicas the index itself is force merged; with them a clone without any is.
        ("searchable_snapshot", "branch-skip-clone-check") => {
            if idx.number_of_replicas() == 0 { Outcome::Goto("forcemerge") } else { Outcome::Next }
        }
        ("searchable_snapshot", "cleanup-generated-index") => {
            drop(st);
            cleanup_generated(env, e.force_merge_clone_index_name.as_deref()).await
        }
        ("searchable_snapshot", "generate-index-name") => {
            e.force_merge_clone_index_name = Some(unique_name("fm-clone-", i));
            Outcome::Next
        }
        ("searchable_snapshot", "clone") => {
            let Some(clone) = e.force_merge_clone_index_name.clone() else { return fail_with(format!("no clone index name was generated for index [{i}]")) };
            if exists(&clone) {
                return Outcome::Next;
            }
            drop(st);
            let body = obj! { "settings" => obj! { "index.number_of_replicas" => 0, "index.lifecycle.skip" => true } };
            match env.act("POST", &format!("/{}/_clone/{}", enc(i), enc(&clone)), Some(body)).await {
                Outcome::Next => mark_resized(env, &clone).await,
                other => other,
            }
        }
        ("searchable_snapshot", "wait-for-index-color-cloned-index" | "segment-count" | "wait-for-data-tier") => Outcome::Next,
        ("searchable_snapshot", "forcemerge") => {
            let merged = e.force_merge_clone_index_name.clone().filter(|c| exists(c)).unwrap_or_else(|| i.to_string());
            drop(st);
            env.act("POST", &format!("/{}/_forcemerge?max_num_segments=1", enc(&merged)), None).await
        }
        // `GenerateSnapshotNameStep`: `<now/d>-<index>-<policy>-<random>` in the repository.
        ("searchable_snapshot", "generate-snapshot-name") => {
            let day = chrono::DateTime::from_timestamp_millis(env.now).map(|d| d.format("%Y.%m.%d").to_string()).unwrap_or_default();
            e.snapshot_repository = Some(repository(conf).to_string());
            e.snapshot_name = Some(format!("{day}-{i}-{}-{}", env.policy, crate::meta::names::random_uuid().to_lowercase()));
            Outcome::Next
        }
        (_, "cleanup-snapshot") => {
            drop(st);
            cleanup_snapshot(env, e).await
        }
        ("searchable_snapshot", "create-snapshot") => {
            let (Some(repo), Some(snap)) = (e.snapshot_repository.clone(), e.snapshot_name.clone()) else {
                return fail_with(format!("snapshot name was not generated for policy [{}] and index [{i}]", env.policy));
            };
            let snapshotted = e.force_merge_clone_index_name.clone().filter(|c| exists(c)).unwrap_or_else(|| i.to_string());
            drop(st);
            let body = obj! { "indices" => snapshotted, "include_global_state" => false };
            match env.call("PUT", &format!("/_snapshot/{}/{}?wait_for_completion=true", enc(&repo), enc(&snap)), Some(body)).await {
                // `CreateSnapshotStep`: an incomplete snapshot is cleaned up and taken again.
                Ok(r) if r.pointer(&["snapshot", "state"]).and_then(Value::as_str).is_some_and(|s| s != "SUCCESS") => Outcome::Goto("cleanup-snapshot"),
                Ok(_) => Outcome::Next,
                Err(err) => Outcome::Fail(err),
            }
        }
        ("searchable_snapshot", "mount-snapshot") => {
            let Some(target) = target else { return Outcome::Next };
            // `MountSnapshotStep`: an index mounted by an earlier attempt is kept.
            if exists(&target) {
                return Outcome::Next;
            }
            let (Some(repo), Some(snap)) = (e.snapshot_repository.clone(), e.snapshot_name.clone()) else {
                return fail_with(format!("snapshot repository is not present for policy [{}] and index [{i}]", env.policy));
            };
            let snapshotted = idx
                .settings
                .get("index.store.snapshot.index_name")
                .map(str::to_string)
                .or_else(|| e.force_merge_clone_index_name.clone())
                .unwrap_or_else(|| i.to_string());
            drop(st);
            let partial = mount_prefix(e) == "partial-";
            let mut settings = obj! { "index.auto_expand_replicas" => false };
            if let Value::Obj(m) = &mut settings {
                if partial {
                    m.insert("index.number_of_replicas".into(), Value::from(0));
                }
                if let Some(t) = conf.get("total_shards_per_node") {
                    m.insert("index.routing.allocation.total_shards_per_node".into(), t.clone());
                }
            }
            let body = obj! {
                "index" => snapshotted,
                "renamed_index" => target,
                "index_settings" => settings,
                "ignore_index_settings" => vec![Value::from("index.lifecycle.name"), Value::from("index.lifecycle.skip")],
            };
            let storage = if partial { "shared_cache" } else { "full_copy" };
            env.act("POST", &format!("/_snapshot/{}/{}/_mount?wait_for_completion=true&storage={storage}", enc(&repo), enc(&snap)), Some(body)).await
        }
        ("searchable_snapshot", "branch-delete-force-merged-index") => {
            if e.force_merge_clone_index_name.as_deref().is_some_and(exists) { Outcome::Next } else { Outcome::Goto("branch-on-datastream-check") }
        }
        ("searchable_snapshot", "delete-force-merged-index") => {
            let Some(clone) = e.force_merge_clone_index_name.clone() else { return Outcome::Next };
            drop(st);
            match env.call("DELETE", &format!("/{}", enc(&clone)), None).await {
                Ok(_) => Outcome::Next,
                Err(err) if err.get("type").and_then(Value::as_str) == Some("index_not_found_exception") => Outcome::Next,
                Err(err) => Outcome::Fail(err),
            }
        }
        ("searchable_snapshot", "swap-aliases") => {
            drop(st);
            swap_aliases(env, idx, target, true).await
        }
        // ------------------------------------------------------- swapping the new index in
        (_, "copy-execution-state") => {
            drop(st);
            copy_execution_state(env, e, action, target).await
        }
        (_, "copy-settings") => {
            let target = match target {
                Some(t) if exists(&t) => t,
                other => return missing_target(env, other.as_deref().unwrap_or_default()),
            };
            drop(st);
            env.settings_of(&target, obj! { "index.lifecycle.name" => env.policy.clone() }).await
        }
        (_, "branch-on-datastream-check") => {
            if st.metadata.data_stream_of(i).is_some() {
                Outcome::Next
            } else {
                Outcome::Goto(if action == "shrink" { "aliases" } else { "swap-aliases" })
            }
        }
        (_, "replace-datastream-backing-index") => {
            drop(st);
            replace_backing_index(env, target).await
        }
        // The source, once its data stream has the new index in its place.
        (_, "delete") => {
            drop(st);
            match env.call("DELETE", &format!("/{}", enc(i)), None).await {
                Ok(_) => Outcome::Gone,
                Err(err) => Outcome::Fail(err),
            }
        }
        ("downsample", "swap-aliases") => {
            drop(st);
            swap_aliases(env, idx, target, false).await
        }
        _ => fail_with(format!("step [{step}] of action [{action}] is not supported by roll search")),
    }
}

fn repository(conf: &Value) -> &str {
    conf.get("snapshot_repository").and_then(Value::as_str).unwrap_or_default()
}

/// `ResizeIndexStep` names the source on the index it makes (`index.resize.source.name`, which
/// `is-shrunken-index` and the cleanup read; the resize APIs themselves don't set it).
async fn mark_resized(env: &Env<'_>, target: &str) -> Outcome {
    let (source, target) = (env.index.clone(), target.to_string());
    let done = env
        .cluster
        .update(move |st| {
            if let Some(t) = st.metadata.indices.get(&target).cloned() {
                let mut ni = (*t).clone();
                ni.settings.set("index.resize.source.name", source);
                ni.settings_version += 1;
                st.metadata.indices.insert(target, Arc::new(ni));
            }
            Ok(())
        })
        .await;
    match done {
        Ok(()) => Outcome::Next,
        Err(err) => Outcome::Fail(err.to_value(false)),
    }
}

/// `CleanupGeneratedIndexStep`: deletes an index an earlier attempt made from this one.
async fn cleanup_generated(env: &Env<'_>, name: Option<&str>) -> Outcome {
    let Some(name) = name else { return Outcome::Next };
    let made_here = env.cluster.state().metadata.indices.get(name).is_some_and(|t| t.settings.get("index.resize.source.name") == Some(env.index.as_str()));
    if !made_here {
        return Outcome::Next;
    }
    match env.call("DELETE", &format!("/{}", enc(name)), None).await {
        Ok(_) => Outcome::Next,
        Err(err) if err.get("type").and_then(Value::as_str) == Some("index_not_found_exception") => Outcome::Next,
        Err(err) => Outcome::Fail(err),
    }
}

/// `CleanupSnapshotStep`: deletes the snapshot the execution state names, if it exists.
async fn cleanup_snapshot(env: &Env<'_>, e: &Exec) -> Outcome {
    let (Some(repo), Some(snap)) = (e.snapshot_repository.as_deref(), e.snapshot_name.as_deref()) else { return Outcome::Next };
    match env.call("DELETE", &format!("/_snapshot/{}/{}", enc(repo), enc(snap)), None).await {
        Ok(_) => Outcome::Next,
        Err(err) if matches!(err.get("type").and_then(Value::as_str), Some("snapshot_missing_exception" | "repository_missing_exception")) => Outcome::Next,
        Err(err) => Outcome::Fail(err),
    }
}

/// `CopyExecutionStateStep`: the new index gets the source's execution state, at
/// `is-shrunken-index` after a shrink, else at the step after the action; and loses the
/// `index.lifecycle.skip` it was made with. It gets the policy in the same update (what
/// `copy-settings` sets next): roll keeps execution states only for managed indices, and drops
/// one an unmanaged index holds.
async fn copy_execution_state(env: &Env<'_>, e: &Exec, action: &str, target: Option<String>) -> Outcome {
    let Some(target) = target else { return missing_target(env, "") };
    let at = if action == "shrink" {
        Some(("shrink".to_string(), "is-shrunken-index"))
    } else {
        super::ilm_run::after_action(e)
    };
    let Some((next_action, next_step)) = at else { return fail_with(format!("no step follows action [{action}] for index [{}]", env.index)) };
    let mut copied = e.clone();
    copied.action = Some(next_action);
    copied.step = Some(next_step.to_string());
    copied.step_time = Some(env.now);
    copied.unrecorded = false;
    let (index, policy) = (env.index.clone(), env.policy.clone());
    let done = env
        .cluster
        .update(move |st| {
            let Some(t) = st.metadata.indices.get(&target).cloned() else {
                return Err(EsError::illegal_state(format!("unable to copy execution state from [{index}] to [{target}] as target index does not exist")));
            };
            copied.uuid = t.uuid.clone();
            if t.settings.get("index.lifecycle.skip").is_some() || t.settings.get("index.lifecycle.name") != Some(policy.as_str()) {
                let mut ni = (*t).clone();
                ni.settings.remove("index.lifecycle.skip");
                ni.settings.set("index.lifecycle.name", policy.clone());
                ni.settings_version += 1;
                st.metadata.indices.insert(target.clone(), Arc::new(ni));
            }
            store::family_mut(st, super::ilm_run::STATE).insert(target.clone(), copied.to_value());
            Ok(())
        })
        .await;
    match done {
        Ok(()) => Outcome::Next,
        Err(err) => Outcome::Fail(err.to_value(false)),
    }
}

/// `ReplaceDataStreamBackingIndexStep`: the new index takes the source's place among its data
/// stream's backing indices.
async fn replace_backing_index(env: &Env<'_>, target: Option<String>) -> Outcome {
    let Some(target) = target else { return missing_target(env, "") };
    let st = env.cluster.state();
    let i = env.index.clone();
    let Some(idx) = st.metadata.indices.get(&i).cloned() else { return Outcome::Gone };
    if let Some((ds, failure)) = write_index_of(&st, &idx) {
        return fail_with(format!(
            "index [{i}] is the {}write index for data stream [{ds}], pausing ILM execution of lifecycle [{}] until this index is no longer the write index for the data stream via manual or automated rollover",
            if failure { "failure store " } else { "" },
            env.policy
        ));
    }
    if !st.metadata.indices.contains_key(&target) {
        return missing_target(env, &target);
    }
    drop(st);
    let policy = env.policy.clone();
    let done = env
        .cluster
        .update(move |st| {
            let Some(ds) = st.metadata.data_stream_of(&i).cloned() else {
                return Err(EsError::illegal_state(format!("index [{i}] is not part of a data stream. stopping execution of lifecycle [{policy}] as the data stream cannot be updated")));
            };
            let mut d = (*ds).clone();
            for list in [&mut d.indices, &mut d.failure_indices] {
                if let Some(pos) = list.iter().position(|n| *n == i) {
                    list[pos] = target.clone();
                }
            }
            d.generation += 1;
            if let Some(t) = st.metadata.indices.get(&target).cloned()
                && t.settings.get("index.hidden") != Some("true")
            {
                let mut ni = (*t).clone();
                ni.settings.set("index.hidden", "true");
                ni.settings_version += 1;
                st.metadata.indices.insert(target.clone(), Arc::new(ni));
            }
            st.metadata.data_streams.insert(d.name.clone(), Arc::new(d));
            Ok(())
        })
        .await;
    match done {
        Ok(()) => Outcome::Next,
        Err(err) => Outcome::Fail(err.to_value(false)),
    }
}

/// `SwapAliasesAndDeleteSourceIndexStep`: in one request, deletes the source and points its
/// aliases (all but `is_write_index`) at the new index, with an alias named after the source
/// when `source_alias`.
async fn swap_aliases(env: &Env<'_>, idx: &IndexMeta, target: Option<String>, source_alias: bool) -> Outcome {
    let target = match target {
        Some(t) if env.cluster.state().metadata.indices.contains_key(&t) => t,
        other => return missing_target(env, other.as_deref().unwrap_or_default()),
    };
    let mut actions = vec![obj! { "remove_index" => obj! { "index" => idx.name.clone() } }];
    if source_alias {
        actions.push(obj! { "add" => obj! { "index" => target.clone(), "alias" => idx.name.clone() } });
    }
    for (name, a) in &idx.aliases {
        let mut add = obj! { "index" => target.clone(), "alias" => name.clone() };
        if let Value::Obj(m) = &mut add {
            if let Some(f) = &a.filter {
                m.insert("filter".into(), f.clone());
            }
            if let Some(r) = &a.index_routing {
                m.insert("index_routing".into(), Value::from(r.clone()));
            }
            if let Some(r) = &a.search_routing {
                m.insert("search_routing".into(), Value::from(r.clone()));
            }
            if let Some(h) = a.is_hidden {
                m.insert("is_hidden".into(), Value::Bool(h));
            }
        }
        actions.push(obj! { "add" => add });
    }
    match env.call("POST", "/_aliases", Some(obj! { "actions" => actions })).await {
        Ok(_) => Outcome::Gone,
        Err(err) => Outcome::Fail(err),
    }
}

#[cfg(test)]
mod tests {
    use crate::cluster::{Cluster, ClusterConfig, MemoryStore};
    use crate::json::Value;
    use crate::rest::EsRequest;
    use std::sync::Arc;

    const DAY: i64 = 24 * 3600 * 1000;

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
        let req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
        let r = crate::rest::handle(c, req).await;
        (r.status, crate::json::parse(&r.body).ok().flatten().unwrap_or(Value::Null))
    }

    async fn ok(c: &Cluster, method: &str, uri: &str, body: &str) -> Value {
        let (s, v) = call(c, method, uri, body).await;
        assert!(s < 300, "{method} {uri}: {s} {v}");
        v
    }

    fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
        static NULL: Value = Value::Null;
        v.pointer(path).unwrap_or(&NULL)
    }

    fn first_hit(v: &Value) -> &Value {
        static NULL: Value = Value::Null;
        at(v, &["hits", "hits"]).as_array().and_then(|h| h.first()).unwrap_or(&NULL)
    }

    async fn cluster(repo: Option<&std::path::Path>) -> Arc<Cluster> {
        let mut cfg = ClusterConfig::new("c");
        if let Some(dir) = repo {
            cfg.node_settings.push(("path.repo".into(), dir.to_string_lossy().into_owned()));
        }
        Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap())
    }

    async fn explain(c: &Cluster, index: &str) -> Value {
        let v = ok(c, "GET", &format!("/{index}/_ilm/explain"), "").await;
        at(&v, &["indices", index]).clone()
    }

    fn step(v: &Value) -> String {
        format!("{}/{}/{}", at(v, &["phase"]).as_str().unwrap_or("-"), at(v, &["action"]).as_str().unwrap_or("-"), at(v, &["step"]).as_str().unwrap_or("-"))
    }

    /// ILM passes at `now` until nothing moves (a new index goes on in the pass after the one
    /// that made it).
    async fn settle(c: &Cluster, now: i64) {
        for _ in 0..4 {
            super::super::ilm_run::run(c, now).await;
        }
    }

    /// The history items of `index`, as `phase/action/step` (failures marked `!`).
    async fn history(c: &Cluster, index: &str) -> Vec<String> {
        ok(c, "POST", "/ilm-history-7/_refresh", "").await;
        let body = format!(r#"{{"size":100,"query":{{"term":{{"index":"{index}"}}}},"sort":["_doc"]}}"#);
        let v = ok(c, "POST", "/ilm-history-7/_search", &body).await;
        at(&v, &["hits", "hits"])
            .as_array()
            .into_iter()
            .flatten()
            .map(|h| {
                let s = at(h, &["_source", "state"]);
                let mark = if at(h, &["_source", "success"]) == &Value::Bool(true) { "" } else { "!" };
                format!("{mark}{}/{}/{}", at(s, &["phase"]).as_str().unwrap_or("-"), at(s, &["action"]).as_str().unwrap_or("-"), at(s, &["step"]).as_str().unwrap_or("-"))
            })
            .collect()
    }

    fn named(c: &Cluster, prefix: &str, suffix: &str) -> String {
        c.state().metadata.indices.keys().find(|n| n.starts_with(prefix) && n.ends_with(suffix)).cloned().unwrap_or_else(|| panic!("no index {prefix}*{suffix}"))
    }

    const SHRINK: &str = r#"{"policy":{"phases":{"hot":{"actions":{"rollover":{"max_docs":1}}},"warm":{"actions":{"shrink":{"number_of_shards":1}}}}}}"#;

    /// A rolled-over index shrinks to `shrink-<4>-<index>`, which takes over its aliases
    /// (without `is_write_index`, plus one named after it) and its execution state; the
    /// history has every move Elasticsearch records; nothing is due afterwards.
    #[tokio::test]
    async fn shrink_swaps_in_through_an_alias() {
        let c = cluster(None).await;
        ok(&c, "PUT", "/_ilm/policy/p", SHRINK).await;
        let body = r#"{"settings":{"number_of_shards":2,"index.lifecycle.name":"p","index.lifecycle.rollover_alias":"sa"},"aliases":{"sa":{"is_write_index":true},"other":{"filter":{"term":{"a":1}},"routing":"1"}}}"#;
        ok(&c, "PUT", "/sa-000001", body).await;
        ok(&c, "POST", "/sa/_doc?refresh=true", r#"{"a":1}"#).await;
        settle(&c, crate::time::now_millis()).await;
        let shrunk = named(&c, "shrink-", "-sa-000001");
        let st = c.state();
        assert!(!st.metadata.indices.contains_key("sa-000001"));
        let idx = &st.metadata.indices[&shrunk];
        assert_eq!((idx.number_of_shards(), idx.settings.get("index.lifecycle.name"), idx.settings.get("index.lifecycle.skip")), (1, Some("p"), None));
        assert_eq!(idx.aliases.keys().map(String::as_str).collect::<Vec<_>>(), ["other", "sa", "sa-000001"]);
        assert_eq!((idx.aliases["sa"].is_write_index, idx.aliases["other"].index_routing.as_deref()), (None, Some("1")));
        drop(st);
        let v = explain(&c, &shrunk).await;
        assert_eq!(step(&v), "warm/complete/complete", "{v}");
        assert_eq!(at(&v, &["shrink_index_name"]).as_str(), Some(shrunk.as_str()), "{v}");
        assert_eq!(at(&ok(&c, "GET", "/sa/_count", "").await, &["count"]).as_i64(), Some(1));
        let h = history(&c, "sa-000001").await;
        let tail = [
            "warm/shrink/cleanup-shrink-index",
            "warm/shrink/generate-index-name",
            "warm/shrink/set-single-node-allocation",
            "warm/shrink/check-shrink-allocation",
            "warm/shrink/shrink",
            "warm/shrink/shrunk-shards-allocated",
            "warm/shrink/aliases",
            "warm/shrink/complete",
        ];
        assert_eq!(h.first().map(String::as_str), Some("new/complete/complete"), "{h:?}");
        assert!(h.ends_with(&tail.map(String::from)), "{h:?}");
        assert_eq!(history(&c, &shrunk).await, ["warm/complete/complete"]);
        assert_eq!(super::super::ilm_run::next_due(&c.state()), None);
    }

    /// In a data stream the shrunk index takes the source's place among the backing indices.
    #[tokio::test]
    async fn shrink_swaps_in_through_a_data_stream() {
        let c = cluster(None).await;
        ok(&c, "PUT", "/_ilm/policy/p", SHRINK).await;
        ok(&c, "PUT", "/_index_template/t", r#"{"index_patterns":["logs-s*"],"data_stream":{},"template":{"settings":{"number_of_shards":2,"index.lifecycle.name":"p"}}}"#).await;
        ok(&c, "POST", "/logs-s/_doc?refresh=true", r#"{"@timestamp":"2026-01-01T00:00:00Z"}"#).await;
        let first = c.state().metadata.data_streams["logs-s"].indices[0].clone();
        settle(&c, crate::time::now_millis()).await;
        let shrunk = named(&c, "shrink-", &first);
        let st = c.state();
        let ds = &st.metadata.data_streams["logs-s"];
        assert_eq!((ds.indices[0].as_str(), ds.indices.len(), ds.generation), (shrunk.as_str(), 2, 3));
        assert!(!st.metadata.indices.contains_key(&first));
        assert_eq!(st.metadata.indices[&shrunk].settings.get("index.hidden"), Some("true"));
        drop(st);
        assert_eq!(step(&explain(&c, &shrunk).await), "warm/complete/complete");
        assert_eq!(at(&ok(&c, "GET", "/logs-s/_count", "").await, &["count"]).as_i64(), Some(1));
    }

    /// A time series backing index is downsampled once its end time has passed; the
    /// downsample replaces it in its data stream with the policy and the next step.
    #[tokio::test]
    async fn downsample_swaps_into_a_data_stream() {
        let c = cluster(None).await;
        ok(&c, "PUT", "/_ilm/policy/p", r#"{"policy":{"phases":{"hot":{"actions":{"rollover":{"max_docs":1}}},"warm":{"actions":{"downsample":{"fixed_interval":"1h"}}}}}}"#).await;
        let t = r#"{"index_patterns":["metrics-d*"],"data_stream":{},"template":{"settings":{"index.mode":"time_series","index.routing_path":["host"],"index.lifecycle.name":"p"},"mappings":{"properties":{"@timestamp":{"type":"date"},"host":{"type":"keyword","time_series_dimension":true},"cpu":{"type":"double","time_series_metric":"gauge"}}}}}"#;
        ok(&c, "PUT", "/_index_template/t", t).await;
        let now = crate::time::now_millis();
        for (host, cpu) in [("a", 1.0), ("a", 3.0)] {
            let ts = chrono::DateTime::from_timestamp_millis(now - (cpu as i64) * 1000).unwrap().to_rfc3339();
            ok(&c, "POST", "/metrics-d/_doc?refresh=true", &format!(r#"{{"@timestamp":"{ts}","host":"{host}","cpu":{cpu}}}"#)).await;
        }
        let first = c.state().metadata.data_streams["metrics-d"].indices[0].clone();
        settle(&c, now).await;
        let v = explain(&c, &first).await;
        assert_eq!(step(&v), "warm/downsample/check-ts-end-time-passed", "{v}");
        let end = super::super::ilm_run::next_due(&c.state()).expect("the end time is due");
        settle(&c, end).await;
        let target = format!("downsample-1h-{first}");
        let st = c.state();
        assert_eq!(st.metadata.data_streams["metrics-d"].indices[0], target);
        assert!(!st.metadata.indices.contains_key(&first));
        let idx = &st.metadata.indices[&target];
        assert_eq!((idx.settings.get("index.downsample.status"), idx.settings.get("index.lifecycle.name")), (Some("success"), Some("p")));
        drop(st);
        assert_eq!(step(&explain(&c, &target).await), "warm/complete/complete");
        let v = ok(&c, "POST", &format!("/{target}/_search"), r#"{"query":{"match_all":{}}}"#).await;
        assert_eq!(at(&v, &["hits", "hits"]).as_array().map(Vec::len), Some(1), "{v}");
        assert_eq!(at(first_hit(&v), &["_source", "cpu", "max"]).as_f64(), Some(3.0), "{v}");
        assert_eq!(history(&c, &first).await.last().map(String::as_str), Some("warm/downsample/complete"));
    }

    /// A searchable snapshot of an index with replicas is taken of a force merged clone,
    /// mounted as `restored-<index>` in its place, and the delete phase deletes the mounted
    /// index and its snapshot.
    #[tokio::test]
    async fn searchable_snapshot_mounts_and_swaps_in() {
        let dir = std::env::temp_dir().join(format!("roll-ilm-ss-{}", std::process::id()));
        let c = cluster(Some(&dir)).await;
        ok(&c, "PUT", "/_snapshot/repo", r#"{"type":"fs","settings":{"location":"repo"}}"#).await;
        let policy = r#"{"policy":{"phases":{"hot":{"actions":{"rollover":{"max_docs":1}}},"cold":{"actions":{"searchable_snapshot":{"snapshot_repository":"repo"}}},"delete":{"min_age":"1d","actions":{"delete":{}}}}}}"#;
        ok(&c, "PUT", "/_ilm/policy/p", policy).await;
        ok(&c, "PUT", "/ss-000001", r#"{"settings":{"number_of_replicas":1,"index.lifecycle.name":"p","index.lifecycle.rollover_alias":"ss"},"aliases":{"ss":{"is_write_index":true}}}"#).await;
        ok(&c, "POST", "/ss/_doc?refresh=true", r#"{"a":1}"#).await;
        let now = crate::time::now_millis();
        settle(&c, now).await;
        let st = c.state();
        assert!(!st.metadata.indices.contains_key("ss-000001"));
        assert!(!st.metadata.indices.keys().any(|n| n.starts_with("fm-clone-")), "the clone is deleted");
        let idx = &st.metadata.indices["restored-ss-000001"];
        for (k, v) in [("index.store.type", "snapshot"), ("index.store.snapshot.repository_name", "repo"), ("index.blocks.write", "true"), ("index.lifecycle.name", "p")] {
            assert_eq!(idx.settings.get(k), Some(v), "{k}");
        }
        assert_eq!(idx.aliases.keys().map(String::as_str).collect::<Vec<_>>(), ["ss", "ss-000001"]);
        drop(st);
        let v = explain(&c, "restored-ss-000001").await;
        assert_eq!(step(&v), "cold/complete/complete", "{v}");
        let snapshot = at(&v, &["snapshot_name"]).as_str().unwrap_or_else(|| panic!("{v}")).to_string();
        assert_eq!(at(&v, &["repository_name"]).as_str(), Some("repo"));
        let s = ok(&c, "GET", &format!("/_snapshot/repo/{snapshot}"), "").await;
        let indices = at(&s, &["snapshots"]).as_array().and_then(|a| a.first()).and_then(|s| s.get("indices")).and_then(Value::as_array).cloned().unwrap_or_default();
        assert!(indices.len() == 1 && indices[0].as_str().is_some_and(|i| i.starts_with("fm-clone-")), "{s}");
        ok(&c, "GET", "/restored-ss-000001/_searchable_snapshots/stats", "").await;
        assert_eq!(at(&ok(&c, "GET", "/ss/_count", "").await, &["count"]).as_i64(), Some(1));
        assert_eq!(history(&c, "ss-000001").await.last().map(String::as_str), Some("cold/searchable_snapshot/complete"));
        settle(&c, now + DAY + 60_000).await;
        assert!(!c.state().metadata.indices.contains_key("restored-ss-000001"));
        assert_eq!(call(&c, "GET", &format!("/_snapshot/repo/{snapshot}"), "").await.0, 404, "the delete phase deletes the snapshot");
        assert_eq!(history(&c, "restored-ss-000001").await.last().map(String::as_str), Some("delete/delete/complete"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `indices.lifecycle.history_index_enabled: false` writes no history; a failure is
    /// recorded with its error.
    #[tokio::test]
    async fn history_records_failures_and_can_be_turned_off() {
        let c = cluster(None).await;
        ok(&c, "PUT", "/_ilm/policy/p", r#"{"policy":{"phases":{"hot":{"actions":{"rollover":{"max_docs":1}}}}}}"#).await;
        ok(&c, "PUT", "/erri", r#"{"settings":{"index.lifecycle.name":"p"}}"#).await;
        let now = crate::time::now_millis();
        settle(&c, now).await;
        // Each retry that fails again is recorded too.
        let h = history(&c, "erri").await;
        assert_eq!(h[..4], ["new/complete/complete", "hot/unfollow/branch-check-unfollow-prerequisites", "hot/rollover/check-rollover-ready", "!hot/rollover/ERROR"], "{h:?}");
        assert!(h[4..].iter().all(|s| s == "!hot/rollover/ERROR"), "{h:?}");
        let v = ok(&c, "POST", "/ilm-history-7/_search", r#"{"query":{"term":{"success":false}}}"#).await;
        let details = at(first_hit(&v), &["_source", "error_details"]).as_str().unwrap_or_default().to_string();
        assert!(details.contains("is empty or not defined"), "{v}");
        ok(&c, "PUT", "/_cluster/settings", r#"{"persistent":{"indices.lifecycle.history_index_enabled":false}}"#).await;
        ok(&c, "PUT", "/quiet", r#"{"settings":{"index.lifecycle.name":"p"}}"#).await;
        settle(&c, now + 1).await;
        assert!(history(&c, "quiet").await.is_empty());
    }
}
