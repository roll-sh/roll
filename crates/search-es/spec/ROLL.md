# Vendored Elasticsearch REST API specification

`api/*.json` are the files of the Maven artifact `org.elasticsearch:rest-api-spec:9.5.5`
(`https://repo1.maven.org/maven2/org/elasticsearch/rest-api-spec/9.5.5/rest-api-spec-9.5.5.jar`,
sha256 `5b110851f870bbaf268ba3abb0032515d69ccd67490e3cbdea46574bbf3b595f`), directory
`rest-api-spec/api/`, unmodified. The artifact is licensed under the Apache License 2.0
(`LICENSE.txt` and `NOTICE.txt` here are the artifact's `META-INF` files).

`build.rs` turns them into the route table (`$OUT_DIR/spec.rs`): every API, its paths, methods,
path parts and typed query parameters. To update, replace `api/` with the files of a newer
artifact and update the version above.
