//! A runner for Elasticsearch's YAML REST test format, against roll search or a real
//! Elasticsearch (or OpenSearch).
//!
//! The format is specified in Elasticsearch's
//! `rest-api-spec/src/yamlRestTest/resources/rest-api-spec/test/README.asciidoc`; the reference
//! implementation is the Java runner in `test/yaml-rest-runner`. This crate reproduces that
//! runner's semantics — path selection from the REST API spec, stash rules, assertion type rules,
//! warning checks, prerequisites (`requires`/`skip` with cluster features, capabilities, versions),
//! the between-test cluster wipe — so that a test passing here passes there and vice versa.
//! Two other dialects of the format are supported where they differ: OpenSearch's runner
//! (`Flavor::OpenSearch`) and the official clients' runners, which elasticsearch-clients-tests
//! files (marked by a top-level `requires` document) are written for.
//!
//! Validation: against a real single-node Elasticsearch 9.5.5 every test of `test/free` and of
//! elasticsearch-clients-tests passes or is skipped for an environment reason
//! (`baseline/es-9.5.5.skips.txt`) — with deterministic requests and with `--randomize` (random
//! body content types, paths, methods and `?source=`, as Elasticsearch's CI runs the suites) —
//! and likewise OpenSearch 3.9.0 with its own suite;
//! `scripts/search-yaml-suites.sh es-baseline|os-baseline` reproduces that.
//!
//! Layers, bottom up:
//!
//! - [`value`]: the value model (Java number classes, `toString`, ordered maps);
//! - [`yaml`]: YAML documents with lines, raw scalar text and YAML 1.1 typing;
//! - [`suite`]: test files → setup/teardown/tests/steps;
//! - [`stash`], [`assertions`], [`regex_java`], [`warnings`]: step semantics;
//! - [`spec`]: the REST API spec → HTTP requests;
//! - [`http`]: a keep-alive HTTP/1.1 client;
//! - [`xcontent`]: YAML, CBOR and Smile bodies (the Java runner's random content types);
//! - [`target`], [`wipe`]: what the cluster supports and cleaning it between tests;
//! - [`spawn`]: isolated clusters as local server processes;
//! - [`runner`], [`report`]: scheduling, results, baselines.
//!
//! The `search-yaml` binary (`src/main.rs`) is the command-line front end.

pub mod assertions;
pub mod http;
pub mod regex_java;
pub mod report;
pub mod runner;
pub mod spawn;
pub mod spec;
pub mod stash;
pub mod suite;
pub mod target;
pub mod value;
pub mod warnings;
pub mod wipe;
pub mod xcontent;
pub mod yaml;

pub use report::{Report, Status, TestResult};
pub use runner::{ClusterHook, Config, ContentTypes, SuiteRoot, run};
pub use spawn::SpawnCommand;
pub use target::Flavor;
pub use value::Value;
