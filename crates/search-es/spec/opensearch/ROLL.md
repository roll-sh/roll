# Vendored OpenSearch REST API specification

`api/*.json` are the files of OpenSearch's `rest-api-spec/src/main/resources/rest-api-spec/api/`
at tag `3.9.0` (commit `4ee42a94e87f66fbf1e62a9871b1b87f91e02472`,
`https://github.com/opensearch-project/OpenSearch`), unmodified. They are licensed under the
Apache License 2.0 (`LICENSE.txt` and `NOTICE.txt` here are the repository's).

`build.rs` turns them into the opensearch dialect's route table next to Elasticsearch's: a
cluster with the opensearch dialect routes, validates parameters and reports `405`/"no handler"
exactly for the paths, methods and parameters OpenSearch 3.9.0 has. To update, replace `api/`
with a newer tag's files and update the version above.

`plugins/*.json` are roll's own (not vendored): the REST APIs of the plugins every OpenSearch
distribution bundles that roll implements (the k-NN plugin's `_plugins/_knn/*`, Index State
Management's `_plugins/_ism/*` with its `_opendistro/_ism/*` aliases, the SQL plugin's
`_plugins/_ppl`, `_plugins/_ppl/_explain`, `_plugins/_ppl/_grammar` and
`_plugins/_query/settings`, the Security plugin's `_plugins/_security/api/{internalusers,roles,
rolesmapping,actiongroups,account,tenants,permissionsinfo,securityconfig,audit,ssl,cache}`,
`authinfo`, `whoami` and `health` with their `_opendistro/_security` aliases), written in the
same format from the plugins' route definitions (`RestKNNStatsHandler`, `RestKNNWarmupHandler`,
...). An entry may name `"params_from": "<api>"` to take another API's parameters, and one with
`"overlay": true` adds parameters to a core API whose spec file lacks them (`search`'s
`phase_took` and `cancel_after_time_interval`, which `RestSearchAction` reads).
