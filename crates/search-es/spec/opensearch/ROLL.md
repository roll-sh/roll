# Vendored OpenSearch REST API specification

`api/*.json` are the files of OpenSearch's `rest-api-spec/src/main/resources/rest-api-spec/api/`
at tag `3.9.0` (commit `4ee42a94e87f66fbf1e62a9871b1b87f91e02472`,
`https://github.com/opensearch-project/OpenSearch`), unmodified. They are licensed under the
Apache License 2.0 (`LICENSE.txt` and `NOTICE.txt` here are the repository's).

`build.rs` turns them into the opensearch dialect's route table next to Elasticsearch's: a
cluster with the opensearch dialect routes, validates parameters and reports `405`/"no handler"
exactly for the paths, methods and parameters OpenSearch 3.9.0 has. To update, replace `api/`
with a newer tag's files and update the version above.
