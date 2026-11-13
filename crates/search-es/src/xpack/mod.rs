//! The x-pack REST APIs of Elasticsearch's default distribution, one module per API family.
//!
//! Each family is implemented for real where it is about data roll has: security (the native
//! realm, API keys, tokens, authorization of every request), ES|QL and SQL (query engines over
//! roll's indices), EQL, enrich policies and the enrich processor, transforms, async search,
//! search applications, query rules, connectors, ILM policies, graph explore, text structure
//! detection. Families whose function needs machine-learning models or external services (ML
//! jobs and trained models, watcher, logstash pipelines, searchable snapshots, migrations) keep
//! their objects and state transitions exactly as Elasticsearch reports them, without the
//! native processes behind them (each module's docs say precisely what is emulated).
//!
//! Stored objects live in the cluster metadata ([`store`]); results that only live while a
//! cluster is loaded (async searches, ES|QL and SQL async queries, cursors, verified
//! credentials) live in [`Runtime`]. Families built on the core APIs call them through
//! [`internal`].

pub mod async_results;
pub mod async_search;
pub mod autoscaling;
pub mod downsample;
pub mod enrich;
pub mod entsearch;
pub mod eql;
pub mod fleet;
pub mod esql;
pub mod graph;
pub mod ilm;
pub mod internal;
pub mod logstash;
pub mod migration;
pub mod misc;
pub mod ml;
pub mod rollup;
pub mod mvt;
pub mod searchable_snapshots;
pub mod security;
pub mod sql;
pub mod store;
pub mod streams;
pub mod terms_enum;
pub mod text_structure;
pub mod transform;
pub mod watcher;

use crate::error::EsResult;
use crate::json::Value;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::routes::Api;
use std::collections::HashSet;

/// The x-pack state of a loaded cluster that isn't persisted.
#[derive(Default)]
pub struct Runtime {
    /// Verified credentials (SHA-256 of user, password and stored hash), so a password is
    /// hashed once per cluster load, not per request.
    pub verified: parking_lot::Mutex<HashSet<[u8; 32]>>,
    /// Results of async searches, ES|QL / SQL / EQL async queries and SQL cursors.
    pub results: async_results::Results,
    /// Trained model deployments.
    pub deployments: ml::models::Deployments,
    /// Enrich indices loaded for the enrich processor.
    pub enrich: enrich::Tables,
    /// The file realm's roles (`roll.security.file_roles`), parsed on first use.
    pub file_roles: std::sync::OnceLock<security::FileRoles>,
    /// ES|QL usage counters (`_xpack/usage` `esql`).
    pub esql_telemetry: esql::telemetry::Telemetry,
    /// Transforms running a trigger right now.
    pub transforms: transform::Runs,
    /// Rollup jobs running a trigger right now.
    pub rollups: rollup::Runs,
}

/// The `_nodes` header plus the node, as the cache-clearing APIs answer.
pub fn nodes_ack(ctx: &Ctx) -> Value {
    let mut nodes = crate::json::Map::new();
    nodes.insert(ctx.cluster.node.id.clone(), obj! { "name" => ctx.cluster.node.name.clone() });
    obj! {
        "_nodes" => obj! { "total" => 1, "successful" => 1, "failed" => 0 },
        "cluster_name" => ctx.cluster.name.clone(),
        "nodes" => Value::Obj(nodes),
    }
}

/// Where the top-level field `key` sits in the request body (`(1, 1)` when unknown), for
/// `x_content_parse_exception` locations.
pub fn key_loc(ctx: &Ctx, key: &str) -> (usize, usize) {
    ctx.raw_json().and_then(|src| crate::json::locate::key_location(src, &[], key)).unwrap_or((1, 1))
}

/// Where the value of the top-level field `key` starts in the request body.
pub fn value_loc(ctx: &Ctx, key: &str) -> (usize, usize) {
    use crate::json::locate::{Seg, Target, locate};
    ctx.raw_json().and_then(|src| locate(src, &[Seg::Key(key)], Target::Value)).unwrap_or((1, 1))
}

/// Percent-encodes a name for a path segment of an internal call.
pub fn enc(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'*' | b',' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `[line:col] [parser] unknown field [key]`.
pub fn unknown_field(ctx: &Ctx, parser: &str, key: &str) -> crate::error::EsError {
    let (l, c) = key_loc(ctx, key);
    crate::error::EsError::x_content_parse(l, c, format!("[{parser}] unknown field [{key}]"))
}

macro_rules! handlers {
    ($ctx:ident; $( $($api:ident)|+ => $e:expr, )*) => {
        /// Runs an x-pack API ([`implemented`] said it is one).
        pub async fn dispatch($ctx: &Ctx<'_>) -> EsResult<Reply> {
            match $ctx.api {
                $( $(Api::$api)|+ => $e, )*
                other => Err(crate::error::EsError::not_implemented(other.name())),
            }
        }

        /// Whether an API is one of the x-pack APIs roll implements.
        pub fn implemented(api: Api) -> bool {
            matches!(api, $( $(Api::$api)|+ )|*)
        }
    };
}

handlers! { ctx;
    SecurityAuthenticate => security::tokens::authenticate_api(ctx),
    SecurityPutUser => security::users::put_user(ctx).await,
    SecurityGetUser => security::users::get_user(ctx),
    SecurityDeleteUser => security::users::delete_user(ctx).await,
    SecurityEnableUser => security::users::set_enabled(ctx, true).await,
    SecurityDisableUser => security::users::set_enabled(ctx, false).await,
    SecurityChangePassword => security::users::change_password(ctx).await,
    SecurityQueryUser => security::users::query_user(ctx),
    SecurityPutRole => security::users::put_role(ctx).await,
    SecurityGetRole => security::users::get_role(ctx),
    SecurityDeleteRole => security::users::delete_role(ctx).await,
    SecurityBulkPutRole => security::users::bulk_put_role(ctx).await,
    SecurityBulkDeleteRole => security::users::bulk_delete_role(ctx).await,
    SecurityQueryRole => security::users::query_role(ctx),
    SecurityPutRoleMapping => security::users::put_role_mapping(ctx).await,
    SecurityGetRoleMapping => security::users::get_role_mapping(ctx),
    SecurityDeleteRoleMapping => security::users::delete_role_mapping(ctx).await,
    SecurityPutPrivileges => security::users::put_privileges(ctx).await,
    SecurityGetPrivileges => security::users::get_privileges(ctx),
    SecurityDeletePrivileges => security::users::delete_privileges(ctx).await,
    SecurityGetBuiltinPrivileges => security::users::builtin_privileges(ctx),
    SecurityGetUserPrivileges => security::users::get_user_privileges(ctx),
    SecurityHasPrivileges => security::users::has_privileges(ctx),
    SecurityClearCachedRoles | SecurityClearCachedRealms | SecurityClearCachedPrivileges | SecurityClearApiKeyCache
        | SecurityClearCachedServiceTokens => {
        if ctx.api == Api::SecurityClearCachedRealms {
            ctx.cluster.xpack.verified.lock().clear();
        }
        Reply::ok(nodes_ack(ctx))
    },
    SecurityCreateApiKey => security::api_keys::create(ctx).await,
    SecurityGrantApiKey => security::api_keys::grant(ctx).await,
    SecurityCreateCrossClusterApiKey => security::api_keys::create_cross_cluster(ctx).await,
    SecurityGetApiKey => security::api_keys::get(ctx),
    SecurityQueryApiKeys => security::api_keys::query_keys(ctx),
    SecurityInvalidateApiKey => security::api_keys::invalidate(ctx).await,
    SecurityUpdateApiKey => security::api_keys::update(ctx, false).await,
    SecurityUpdateCrossClusterApiKey => security::api_keys::update(ctx, true).await,
    SecurityBulkUpdateApiKeys => security::api_keys::bulk_update(ctx).await,
    SecurityGetToken => security::tokens::get_token(ctx).await,
    SecurityInvalidateToken => security::tokens::invalidate_token(ctx).await,
    SecurityGetServiceAccounts => security::tokens::get_service_accounts(ctx),
    SecurityCreateServiceToken => security::tokens::create_service_token(ctx).await,
    SecurityDeleteServiceToken => security::tokens::delete_service_token(ctx).await,
    SecurityGetServiceCredentials => security::tokens::get_service_credentials(ctx),
    SecurityActivateUserProfile => security::tokens::activate_profile(ctx).await,
    SecurityGetUserProfile => security::tokens::get_profile(ctx),
    SecurityEnableUserProfile => security::tokens::set_profile_enabled(ctx, true).await,
    SecurityDisableUserProfile => security::tokens::set_profile_enabled(ctx, false).await,
    SecurityUpdateUserProfileData => security::tokens::update_profile_data(ctx).await,
    SecuritySuggestUserProfiles => security::tokens::suggest_profiles(ctx),
    SecurityHasPrivilegesUserProfile => security::tokens::has_privileges_profile(ctx),
    SecurityGetStats => security::tokens::stats(ctx),
    SecurityGetSettings => security::tokens::get_settings(ctx),
    SecurityUpdateSettings => security::tokens::update_settings(ctx).await,

    XpackInfo => misc::xpack_info(ctx),
    XpackUsage => misc::xpack_usage(ctx),
    SslCertificates => misc::ssl_certificates(ctx),
    LicenseGet => misc::license(ctx),

    MlInfo => ml::info(ctx),
    MlGetMemoryStats => ml::memory_stats(ctx),
    MlSetUpgradeMode => ml::set_upgrade_mode(ctx),
    CatMlJobs => ml::cat(ctx, "anomaly_detectors"),
    CatMlDatafeeds => ml::cat(ctx, "datafeeds"),
    CatMlTrainedModels => ml::cat(ctx, "trained_models"),
    CatMlDataFrameAnalytics => ml::cat(ctx, "data_frame_analytics"),
    MlPutJob => ml::jobs::put_job(ctx).await,
    MlGetJobs => ml::jobs::get_jobs(ctx),
    MlGetJobStats => ml::jobs::job_stats(ctx),
    MlDeleteJob => ml::jobs::delete_job(ctx).await,
    MlOpenJob => ml::jobs::open_job(ctx).await,
    MlCloseJob => ml::jobs::close_job(ctx).await,
    MlResetJob => ml::jobs::reset_job(ctx).await,
    MlFlushJob => ml::jobs::flush_job(ctx),
    MlForecast => ml::jobs::forecast(ctx),
    MlDeleteForecast => ml::jobs::delete_forecast(ctx),
    MlUpdateJob => ml::jobs::update_job(ctx).await,
    MlEstimateModelMemory => ml::jobs::estimate_model_memory(ctx),
    MlValidateDetector => ml::jobs::validate_detector(ctx),
    MlValidate => ml::jobs::validate_job(ctx),
    MlDeleteExpiredData => ml::jobs::delete_expired_data(ctx),
    MlGetBuckets => ml::jobs::get_buckets(ctx).await,
    MlGetOverallBuckets => ml::jobs::get_overall_buckets(ctx).await,
    MlGetRecords => ml::jobs::get_records(ctx).await,
    MlGetInfluencers => ml::jobs::get_influencers(ctx).await,
    MlGetCategories => ml::jobs::get_categories(ctx).await,
    MlGetModelSnapshots => ml::jobs::get_model_snapshots(ctx).await,
    MlDeleteModelSnapshot => ml::jobs::delete_model_snapshot(ctx).await,
    MlUpdateModelSnapshot => ml::jobs::update_model_snapshot(ctx).await,
    MlRevertModelSnapshot => ml::jobs::revert_model_snapshot(ctx).await,
    MlUpgradeJobSnapshot => ml::jobs::upgrade_job_snapshot(ctx).await,
    MlGetModelSnapshotUpgradeStats => ml::jobs::upgrade_stats(ctx),
    MlPutDatafeed => ml::jobs::put_datafeed(ctx).await,
    MlUpdateDatafeed => ml::jobs::update_datafeed(ctx).await,
    MlGetDatafeeds => ml::jobs::get_datafeeds(ctx),
    MlGetDatafeedStats => ml::jobs::datafeed_stats(ctx),
    MlStartDatafeed => ml::jobs::start_datafeed(ctx).await,
    MlStopDatafeed => ml::jobs::stop_datafeed(ctx).await,
    MlDeleteDatafeed => ml::jobs::delete_datafeed(ctx).await,
    MlPreviewDatafeed => ml::jobs::preview_datafeed(ctx).await,
    MlPutCalendar => ml::jobs::put_calendar(ctx).await,
    MlGetCalendars => ml::jobs::get_calendars(ctx),
    MlDeleteCalendar => ml::jobs::delete_calendar(ctx).await,
    MlPutCalendarJob => ml::jobs::calendar_job(ctx, true).await,
    MlDeleteCalendarJob => ml::jobs::calendar_job(ctx, false).await,
    MlPostCalendarEvents => ml::jobs::post_events(ctx).await,
    MlGetCalendarEvents => ml::jobs::get_events(ctx),
    MlDeleteCalendarEvent => ml::jobs::delete_event(ctx).await,
    MlPutFilter => ml::jobs::put_filter(ctx).await,
    MlGetFilters => ml::jobs::get_filters(ctx),
    MlUpdateFilter => ml::jobs::update_filter(ctx).await,
    MlDeleteFilter => ml::jobs::delete_filter(ctx).await,
    MlPutTrainedModel => ml::models::put_model(ctx).await,
    MlGetTrainedModels => ml::models::get_models(ctx),
    MlGetTrainedModelsStats => ml::models::model_stats(ctx),
    MlDeleteTrainedModel => ml::models::delete_model(ctx).await,
    MlPutTrainedModelDefinitionPart => ml::models::put_definition_part(ctx).await,
    MlPutTrainedModelVocabulary => ml::models::put_vocabulary(ctx).await,
    MlPutTrainedModelAlias => ml::models::put_alias(ctx).await,
    MlDeleteTrainedModelAlias => ml::models::delete_alias(ctx).await,
    MlStartTrainedModelDeployment => ml::models::start_deployment(ctx),
    MlUpdateTrainedModelDeployment => ml::models::update_deployment(ctx),
    MlStopTrainedModelDeployment => ml::models::stop_deployment(ctx),
    MlClearTrainedModelDeploymentCache => ml::models::clear_cache(ctx),
    MlInferTrainedModel => ml::models::infer(ctx),
    MlPutDataFrameAnalytics => ml::dfa::put(ctx).await,
    MlGetDataFrameAnalytics => ml::dfa::get(ctx),
    MlGetDataFrameAnalyticsStats => ml::dfa::stats(ctx),
    MlStartDataFrameAnalytics => ml::dfa::start(ctx).await,
    MlStopDataFrameAnalytics => ml::dfa::stop(ctx).await,
    MlDeleteDataFrameAnalytics => ml::dfa::delete(ctx).await,
    MlUpdateDataFrameAnalytics => ml::dfa::update(ctx).await,
    MlExplainDataFrameAnalytics => ml::dfa::explain(ctx).await,
    MlEvaluateDataFrame => ml::dfa::evaluate(ctx).await,

    // ES|QL and SQL read whatever indices their query names: as the client, never as the owner.
    EsqlQuery => internal::as_caller(internal::Caller::of(ctx), esql::query(ctx)).await,
    EsqlAsyncQuery => internal::as_caller(internal::Caller::of(ctx), esql::async_query(ctx)).await,
    EsqlAsyncQueryGet => esql::async_get(ctx),
    EsqlAsyncQueryStop => esql::async_stop(ctx),
    EsqlAsyncQueryDelete => esql::async_delete(ctx),
    EsqlListQueries => esql::list_queries(ctx),
    EsqlGetQuery => esql::get_query(ctx),
    EsqlPutView => esql::put_view(ctx).await,
    EsqlGetView => esql::get_view(ctx),
    EsqlDeleteView => esql::delete_view(ctx).await,
    EsqlPutDataSource => esql::datasources::put_data_source(ctx).await,
    EsqlGetDataSource => esql::datasources::get_data_source(ctx),
    EsqlDeleteDataSource => esql::datasources::delete_data_source(ctx).await,
    EsqlPutDataset => esql::datasources::put_dataset(ctx).await,
    EsqlGetDataset => esql::datasources::get_dataset(ctx),
    EsqlDeleteDataset => esql::datasources::delete_dataset(ctx).await,

    SqlQuery => internal::as_caller(internal::Caller::of(ctx), sql::query(ctx)).await,
    SqlTranslate => internal::as_caller(internal::Caller::of(ctx), sql::translate(ctx)).await,
    SqlClearCursor => sql::clear_cursor(ctx),
    SqlGetAsync => sql::get_async(ctx),
    SqlGetAsyncStatus => sql::get_async_status(ctx),
    SqlDeleteAsync => sql::delete_async(ctx),

    EqlSearch => eql::search(ctx).await,
    EqlGet => eql::get(ctx),
    EqlGetStatus => eql::status(ctx),
    EqlDelete => eql::delete(ctx),

    AsyncSearchSubmit => async_search::submit(ctx).await,
    AsyncSearchGet => async_search::get(ctx),
    AsyncSearchStatus => async_search::status(ctx),
    AsyncSearchDelete => async_search::delete(ctx),

    LogstashPutPipeline => logstash::put_pipeline(ctx).await,
    LogstashGetPipeline => logstash::get_pipeline(ctx),
    LogstashDeletePipeline => logstash::delete_pipeline(ctx).await,

    StreamsLogsEnable => streams::set_enabled(ctx, true).await,
    StreamsLogsDisable => streams::set_enabled(ctx, false).await,
    StreamsStatus => streams::status(ctx),

    MigrationGetFeatureUpgradeStatus => migration::feature_upgrade_status(ctx),
    MigrationPostFeatureUpgrade => migration::post_feature_upgrade(ctx),
    MigrationDeprecations => migration::deprecations(ctx),
    IndicesMigrateReindex => migration::migrate_reindex(ctx).await,
    IndicesGetMigrateReindexStatus => migration::migrate_reindex_status(ctx),
    IndicesCancelMigrateReindex => migration::cancel_migrate_reindex(ctx).await,
    IndicesCreateFrom => migration::create_from(ctx).await,

    TransformPutTransform => transform::put(ctx).await,
    TransformUpdateTransform => transform::update(ctx).await,
    TransformGetTransform => transform::get(ctx),
    TransformGetTransformStats => transform::stats(ctx).await,
    TransformPreviewTransform => transform::preview(ctx).await,
    TransformStartTransform => transform::start(ctx).await,
    TransformStopTransform => transform::stop(ctx).await,
    TransformScheduleNowTransform => transform::schedule_now(ctx).await,
    TransformResetTransform => transform::reset(ctx).await,
    TransformDeleteTransform => transform::delete(ctx).await,
    TransformGetNodeStats => transform::node_stats(ctx),
    TransformUpgradeTransforms => transform::upgrade(ctx),
    TransformSetUpgradeMode => transform::set_upgrade_mode(ctx),
    CatTransforms => transform::cat(ctx),

    RollupPutJob => rollup::put(ctx).await,
    RollupGetJobs => rollup::get(ctx),
    RollupStartJob => rollup::start(ctx).await,
    RollupStopJob => rollup::stop(ctx).await,
    RollupDeleteJob => rollup::delete(ctx).await,
    RollupGetRollupCaps => rollup::caps(ctx),
    RollupGetRollupIndexCaps => rollup::index_caps(ctx),
    RollupRollupSearch => rollup::rollup_search(ctx).await,

    EnrichPutPolicy => enrich::put_policy(ctx).await,
    EnrichGetPolicy => enrich::get_policy(ctx),
    EnrichDeletePolicy => enrich::delete_policy(ctx).await,
    EnrichExecutePolicy => enrich::execute_policy(ctx).await,
    EnrichStats => enrich::stats(ctx),

    FleetGlobalCheckpoints => fleet::global_checkpoints(ctx).await,
    FleetSearch => fleet::search(ctx).await,
    FleetMsearch => fleet::msearch(ctx).await,
    FleetPostSecret => fleet::post_secret(ctx).await,
    FleetGetSecret => fleet::get_secret(ctx),
    FleetDeleteSecret => fleet::delete_secret(ctx).await,
    AutoscalingPutAutoscalingPolicy => autoscaling::put(ctx).await,
    AutoscalingGetAutoscalingPolicy => autoscaling::get(ctx),
    AutoscalingDeleteAutoscalingPolicy => autoscaling::delete(ctx).await,
    AutoscalingGetAutoscalingCapacity => autoscaling::capacity(ctx),
    ConnectorPut => entsearch::connectors::put(ctx).await,
    ConnectorPost => entsearch::connectors::post(ctx).await,
    ConnectorGet => entsearch::connectors::get(ctx),
    ConnectorList => entsearch::connectors::list(ctx),
    ConnectorDelete => entsearch::connectors::delete(ctx).await,
    ConnectorCheckIn => entsearch::connectors::check_in(ctx).await,
    ConnectorUpdateActiveFiltering | ConnectorUpdateApiKeyId | ConnectorUpdateConfiguration | ConnectorUpdateError | ConnectorUpdateFeatures
        | ConnectorUpdateFiltering | ConnectorUpdateFilteringValidation | ConnectorUpdateIndexName | ConnectorUpdateName | ConnectorUpdateNative
        | ConnectorUpdatePipeline | ConnectorUpdateScheduling | ConnectorUpdateServiceType | ConnectorUpdateStatus | ConnectorLastSync => entsearch::connectors::update(ctx).await,
    ConnectorSyncJobPost => entsearch::connectors::sync_job_post(ctx).await,
    ConnectorSyncJobGet => entsearch::connectors::sync_job_get(ctx),
    ConnectorSyncJobList => entsearch::connectors::sync_job_list(ctx),
    ConnectorSyncJobCheckIn => entsearch::connectors::sync_job_check_in(ctx).await,
    ConnectorSyncJobCancel => entsearch::connectors::sync_job_cancel(ctx).await,
    ConnectorSyncJobError => entsearch::connectors::sync_job_error(ctx).await,
    ConnectorSyncJobUpdateStats => entsearch::connectors::sync_job_update_stats(ctx).await,
    ConnectorSyncJobClaim => entsearch::connectors::sync_job_claim(ctx).await,
    ConnectorSyncJobDelete => entsearch::connectors::sync_job_delete(ctx).await,
    ConnectorSecretPost => entsearch::connectors::secret_post(ctx).await,
    ConnectorSecretPut => entsearch::connectors::secret_put(ctx).await,
    ConnectorSecretGet => entsearch::connectors::secret_get(ctx),
    ConnectorSecretDelete => entsearch::connectors::secret_delete(ctx).await,

    QueryRulesPutRuleset => entsearch::query_rules::put_ruleset(ctx).await,
    QueryRulesGetRuleset => entsearch::query_rules::get_ruleset(ctx),
    QueryRulesDeleteRuleset => entsearch::query_rules::delete_ruleset(ctx).await,
    QueryRulesListRulesets => entsearch::query_rules::list_rulesets(ctx),
    QueryRulesPutRule => entsearch::query_rules::put_rule(ctx).await,
    QueryRulesGetRule => entsearch::query_rules::get_rule(ctx),
    QueryRulesDeleteRule => entsearch::query_rules::delete_rule(ctx).await,
    QueryRulesTest => entsearch::query_rules::test(ctx),

    SearchApplicationPut => entsearch::search_application::put(ctx).await,
    SearchApplicationGet => entsearch::search_application::get(ctx),
    SearchApplicationList => entsearch::search_application::list(ctx),
    SearchApplicationDelete => entsearch::search_application::delete(ctx).await,
    SearchApplicationSearch => entsearch::search_application::search(ctx).await,
    SearchApplicationRenderQuery => entsearch::search_application::render_query(ctx),
    SearchApplicationPutBehavioralAnalytics => entsearch::search_application::put_analytics(ctx).await,
    SearchApplicationGetBehavioralAnalytics => entsearch::search_application::get_analytics(ctx),
    SearchApplicationDeleteBehavioralAnalytics => entsearch::search_application::delete_analytics(ctx).await,
    SearchApplicationPostBehavioralAnalyticsEvent => entsearch::search_application::post_event(ctx).await,

    IlmPutLifecycle => ilm::put_lifecycle(ctx).await,
    IlmGetLifecycle => ilm::get_lifecycle(ctx),
    IlmDeleteLifecycle => ilm::delete_lifecycle(ctx).await,
    IlmGetStatus => ilm::get_status(ctx),
    IlmStart => ilm::set_mode(ctx, true).await,
    IlmStop => ilm::set_mode(ctx, false).await,
    IlmExplainLifecycle => ilm::explain(ctx).await,
    IlmRemovePolicy => ilm::remove_policy(ctx).await,
    IlmRetry => ilm::retry(ctx).await,
    IlmMoveToStep => ilm::move_to_step(ctx).await,
    IlmMigrateToDataTiers => ilm::migrate_to_data_tiers(ctx),

    WatcherPutWatch => watcher::put_watch(ctx).await,
    WatcherGetWatch => watcher::get_watch(ctx),
    WatcherDeleteWatch => watcher::delete_watch(ctx).await,
    WatcherExecuteWatch => watcher::execute_watch(ctx).await,
    WatcherAckWatch => watcher::ack_watch(ctx).await,
    WatcherActivateWatch => watcher::set_active(ctx, true).await,
    WatcherDeactivateWatch => watcher::set_active(ctx, false).await,
    WatcherQueryWatches => watcher::query_watches(ctx),
    WatcherStats => watcher::stats(ctx),
    WatcherStart => watcher::set_running(ctx, true).await,
    WatcherStop => watcher::set_running(ctx, false).await,
    WatcherGetSettings => watcher::get_settings(ctx),
    WatcherUpdateSettings => watcher::update_settings(ctx).await,

    TermsEnum => terms_enum::terms_enum(ctx).await,
    GraphExplore => graph::explore(ctx).await,

    TextStructureFindStructure => text_structure::find_structure(ctx),
    TextStructureFindMessageStructure => text_structure::find_message_structure(ctx),
    TextStructureFindFieldStructure => text_structure::find_field_structure(ctx).await,
    TextStructureTestGrokPattern => text_structure::test_grok_pattern(ctx),

    IndicesDownsample => downsample::downsample(ctx).await,
    SearchMvt => mvt::search_mvt(ctx).await,

    SearchableSnapshotsMount => searchable_snapshots::mount(ctx).await,
    SearchableSnapshotsStats => searchable_snapshots::stats(ctx),
    SearchableSnapshotsCacheStats => searchable_snapshots::cache_stats(ctx),
    SearchableSnapshotsClearCache => searchable_snapshots::clear_cache(ctx),
}
