//! API handlers, one module per family, and the dispatch from [`Api`] to them.
//!
//! Every API of the spec routes somewhere: the ones roll search implements run their handler,
//! the rest answer `unsupported_operation_exception` naming the API, so clients get a clear
//! error instead of a 404-style "no handler".

pub mod aliases;
pub mod analyze;
pub mod cat;
mod cat_columns;
mod cat_columns_os;
pub mod cluster;
pub mod data_streams;
pub mod desired_nodes;
pub mod field_caps;
pub mod health;
pub mod indices;
pub mod ingest;
pub mod misc;
pub mod nodes;
pub mod reroute;
pub mod resize;
pub mod rollover;
pub mod root;
pub mod scripts;
pub mod shards;
pub mod snapshots;
pub mod synonyms;
pub mod tasks;
pub mod templates;

use crate::error::{EsError, EsResult};
use crate::meta::resolve::{self, IndicesOptions};
use crate::rest::{Ctx, Reply};
use crate::routes::Api;

/// Query parameters a handler reads that the spec doesn't list.
pub fn extra_params(api: Api) -> &'static [&'static str] {
    match api {
        Api::IndicesCreate => &["include_type_name"],
        Api::IndicesPutSettings => &["reopen"],
        Api::IndicesGetMapping | Api::IndicesGet | Api::IndicesGetSettings => &["include_type_name"],
        Api::IngestProcessorGrok => &["s", "ecs_compatibility"],
        Api::CatTasks => &["ts", "pri", "size", "detailed"],
        a if a.name().starts_with("cat.") => &["ts", "pri", "size"],
        _ => &[],
    }
}

/// The handler of every implemented API. [`dispatch`] runs them and [`implemented`] (which
/// `_capabilities` reports) is generated from the same table, so they can't drift apart.
macro_rules! handlers {
    ($ctx:ident; $( $($api:ident)|+ => $e:expr, )*) => {
        pub async fn dispatch($ctx: &Ctx<'_>) -> EsResult<Reply> {
            match $ctx.api {
                $( $(Api::$api)|+ => $e, )*
                other => Err(EsError::not_implemented(other.name())),
            }
        }

        /// Whether roll implements an API (the others answer `unsupported_operation_exception`).
        pub fn implemented(api: Api) -> bool {
            matches!(api, $( $(Api::$api)|+ )|*)
        }
    };
}

handlers! { ctx;

    Info => root::info(ctx),
    Ping => root::ping(ctx),
    Capabilities => root::capabilities(ctx),

    IndicesCreate => indices::create(ctx).await,
    IndicesDelete => indices::delete(ctx).await,
    // Elasticsearch answers these HEAD APIs with the matching GET's response (body dropped).
    IndicesExists => indices::get(ctx),
    IndicesGet => indices::get(ctx),
    IndicesGetMapping => indices::get_mapping(ctx),
    IndicesPutMapping => indices::put_mapping(ctx).await,
    IndicesGetFieldMapping => indices::get_field_mapping(ctx),
    IndicesGetSettings => indices::get_settings(ctx),
    IndicesPutSettings => indices::put_settings(ctx).await,
    IndicesOpen => indices::open(ctx).await,
    IndicesClose => indices::close(ctx).await,
    IndicesAddBlock => indices::add_block(ctx).await,
    IndicesRemoveBlock => indices::remove_block(ctx).await,
    IndicesStats => indices::stats(ctx),
    IndicesResolveIndex => indices::resolve_index(ctx),
    IndicesRefresh => indices::refresh(ctx),
    IndicesFlush => indices::flush(ctx),
    IndicesForcemerge => indices::forcemerge(ctx),

    IndicesAnalyze => analyze::analyze(ctx),
    FieldCaps => field_caps::field_caps(ctx),

    SearchShards => shards::search_shards(ctx),
    IndicesShardStores => shards::shard_stores(ctx),
    IndicesSegments => shards::segments(ctx),
    NodesReloadSecureSettings => shards::reload_secure_settings(ctx),

    TasksList => tasks::list(ctx),
    TasksGet => tasks::get(ctx),
    TasksCancel => tasks::cancel(ctx),

    IndicesGetAlias => aliases::get_alias(ctx),
    IndicesExistsAlias => aliases::get_alias(ctx),
    IndicesPutAlias => aliases::put_alias(ctx).await,
    IndicesDeleteAlias => aliases::delete_alias(ctx).await,
    IndicesUpdateAliases => aliases::update_aliases(ctx).await,

    IndicesPutIndexTemplate => templates::put_index_template(ctx).await,
    IndicesGetIndexTemplate => templates::get_index_template(ctx),
    IndicesDeleteIndexTemplate => templates::delete_index_template(ctx).await,
    IndicesExistsIndexTemplate => templates::get_index_template(ctx),
    IndicesSimulateIndexTemplate => templates::simulate_index(ctx),
    IndicesSimulateTemplate => templates::simulate_template(ctx),
    ClusterPutComponentTemplate => templates::put_component_template(ctx).await,
    ClusterGetComponentTemplate => templates::get_component_template(ctx),
    ClusterDeleteComponentTemplate => templates::delete_component_template(ctx).await,
    ClusterExistsComponentTemplate => templates::get_component_template(ctx),
    IndicesPutTemplate => templates::put_template(ctx).await,
    IndicesGetTemplate => templates::get_template(ctx),
    IndicesDeleteTemplate => templates::delete_template(ctx).await,
    IndicesExistsTemplate => templates::get_template(ctx),

    IndicesGetDataStream => data_streams::get(ctx),
    IndicesDeleteDataStream => data_streams::delete(ctx).await,
    IndicesCreateDataStream => data_streams::create(ctx).await,
    IndicesDataStreamsStats => data_streams::stats(ctx),
    IndicesMigrateToDataStream => data_streams::migrate(ctx).await,
    IndicesPromoteDataStream => data_streams::promote(ctx).await,
    IndicesModifyDataStream => data_streams::modify(ctx).await,
    IndicesGetDataLifecycle => data_streams::get_lifecycle(ctx),
    IndicesPutDataLifecycle => data_streams::put_lifecycle(ctx).await,
    IndicesDeleteDataLifecycle => data_streams::delete_lifecycle(ctx).await,
    IndicesExplainDataLifecycle => data_streams::explain_lifecycle(ctx),
    IndicesGetDataLifecycleStats => data_streams::lifecycle_stats(ctx),
    IndicesGetDataStreamOptions => data_streams::get_options(ctx),
    IndicesPutDataStreamOptions => data_streams::put_options(ctx).await,
    IndicesDeleteDataStreamOptions => data_streams::delete_options(ctx).await,
    IndicesGetDataStreamSettings => data_streams::get_settings(ctx),
    IndicesGetDataStreamMappings => data_streams::get_mappings(ctx),
    IndicesPutDataStreamSettings => data_streams::put_settings(ctx).await,
    IndicesPutDataStreamMappings => data_streams::put_mappings(ctx).await,
    IndicesRollover => rollover::rollover(ctx).await,
    IndicesShrink => resize::handle(ctx, resize::ResizeType::Shrink).await,
    IndicesSplit => resize::handle(ctx, resize::ResizeType::Split).await,
    IndicesClone => resize::handle(ctx, resize::ResizeType::Clone).await,

    ClusterHealth => cluster::health(ctx).await,
    ClusterState => cluster::state(ctx),
    ClusterStats => cluster::stats(ctx),
    ClusterGetSettings => cluster::get_settings(ctx),
    ClusterPutSettings => cluster::put_settings(ctx).await,
    ClusterAllocationExplain => cluster::allocation_explain(ctx),
    ClusterPendingTasks => cluster::pending_tasks(ctx),
    ClusterInfo => cluster::info(ctx),

    NodesInfo => nodes::info(ctx),
    NodesStats => nodes::stats(ctx),
    NodesUsage => nodes::usage(ctx),

    PutScript => scripts::put(ctx).await,
    GetScript => scripts::get(ctx),
    DeleteScript => scripts::delete(ctx).await,
    GetScriptLanguages => scripts::languages(ctx),
    GetScriptContext => scripts::contexts(ctx),

    IngestPutPipeline => ingest::put(ctx).await,
    IngestGetPipeline => ingest::get(ctx),
    IngestDeletePipeline => ingest::delete(ctx).await,
    IngestProcessorGrok => ingest::grok(ctx),

    SynonymsPutSynonym => synonyms::put_set(ctx).await,
    SynonymsGetSynonym => synonyms::get_set(ctx),
    SynonymsDeleteSynonym => synonyms::delete_set(ctx).await,
    SynonymsGetSynonymsSets => synonyms::list_sets(ctx),
    SynonymsPutSynonymRule => synonyms::put_rule(ctx).await,
    SynonymsGetSynonymRule => synonyms::get_rule(ctx),
    SynonymsDeleteSynonymRule => synonyms::delete_rule(ctx).await,

    CatIndices => cat::indices(ctx),
    CatAliases => cat::aliases(ctx),
    CatHealth => cat::health(ctx),
    CatNodes => cat::nodes(ctx),
    CatShards => cat::shards(ctx),
    CatTemplates => cat::templates(ctx),
    CatCount => cat::count(ctx),
    CatMaster | CatClusterManager => cat::master(ctx),
    CatSegmentReplication => cat::segment_replication(ctx),
    CatAllocation => cat::allocation(ctx),
    CatHelp => cat::help(ctx),
    CatPendingTasks => cat::pending_tasks(ctx),
    CatSegments => cat::segments(ctx),
    CatRecovery => cat::recovery(ctx),
    CatThreadPool => cat::thread_pool(ctx),
    CatPlugins => cat::plugins(ctx),
    CatNodeattrs => cat::nodeattrs(ctx),
    CatFielddata => cat::fielddata(ctx),
    CatTasks => cat::tasks(ctx),
    CatComponentTemplates => cat::component_templates(ctx),
    CatCircuitBreaker => cat::circuit_breaker(ctx),
    CatRepositories => cat::repositories(ctx),
    CatSnapshots => cat::snapshots(ctx),

    IndicesClearCache => misc::clear_cache(ctx),
    IndicesRecovery => misc::recovery(ctx),
    ClusterRemoteInfo => misc::remote_info(ctx),
    ClusterReroute => reroute::reroute(ctx),
    FeaturesGetFeatures => misc::get_features(ctx),
    FeaturesResetFeatures => misc::reset_features(ctx).await,
    ClusterPostVotingConfigExclusions => misc::post_voting_exclusions(ctx).await,
    ClusterDeleteVotingConfigExclusions => misc::delete_voting_exclusions(ctx).await,
    NodesHotThreads => misc::hot_threads(ctx),
    IndicesResolveCluster => misc::resolve_cluster(ctx),
    SnapshotCreateRepository => snapshots::put_repository(ctx).await,
    SnapshotGetRepository => snapshots::get_repository(ctx),
    SnapshotDeleteRepository => snapshots::delete_repository(ctx).await,
    SnapshotVerifyRepository => snapshots::verify_repository(ctx),
    SnapshotCleanupRepository => snapshots::cleanup_repository(ctx),
    SnapshotCreate => snapshots::create(ctx).await,
    SnapshotGet => snapshots::get(ctx),
    SnapshotDelete => snapshots::delete(ctx).await,
    SnapshotStatus => snapshots::status(ctx),
    SnapshotClone => snapshots::clone(ctx).await,
    SnapshotRestore => snapshots::restore(ctx).await,

    HealthReport => health::health_report(ctx),
    InternalGetDesiredBalance => health::desired_balance(ctx),
    InternalDeleteDesiredBalance => health::reset_desired_balance(ctx),
    InternalPrevalidateNodeRemoval => health::prevalidate_node_removal(ctx),

    InternalUpdateDesiredNodes => desired_nodes::update(ctx).await,
    InternalGetDesiredNodes => desired_nodes::get_latest(ctx),
    InternalDeleteDesiredNodes => desired_nodes::delete(ctx).await,
}

/// Resolves the `{index}` path part (or all indices when absent).
pub fn resolve_indices(ctx: &Ctx, defaults: IndicesOptions) -> EsResult<Vec<String>> {
    let opts = ctx.indices_options(defaults)?;
    let exprs = ctx.param("index").map(resolve::split_expressions).unwrap_or_default();
    resolve::resolve(&ctx.state(), &exprs, &opts, ctx.now_ms)
}

/// Elasticsearch's deprecation warning for requests that reach system indices directly
/// (`IndexNameExpressionResolver.checkSystemIndexAccess`), names sorted.
pub fn warn_system_access(ctx: &Ctx, names: &[String]) {
    let st = ctx.state();
    let mut system: Vec<&str> = names.iter().filter(|n| st.metadata.indices.get(n.as_str()).is_some_and(|i| i.system)).map(String::as_str).collect();
    if system.is_empty() {
        return;
    }
    system.sort_unstable();
    ctx.warn(format!(
        "this request accesses system indices: [{}], but in a future major version, direct access to system indices will be prevented by default",
        system.join(", ")
    ));
}

/// The `{index}` expressions of the path.
pub fn index_expressions(ctx: &Ctx) -> Vec<String> {
    ctx.param("index").map(resolve::split_expressions).unwrap_or_default()
}
