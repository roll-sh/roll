//! `search-yaml`: run Elasticsearch YAML REST suites against a cluster.
//!
//! ```text
//! search-yaml run --url http[s]://127.0.0.1:9200 --suite target/search-yaml/es-9.5.5/rest-api-spec/test/free \
//!     [--api-spec DIR] [--filter RE] [--skip-list FILE] [--report out.json] [--baseline FILE] \
//!     [--write-baseline FILE] [--jobs N --cluster-hook URL | --jobs N --spawn CMD] [--flavor elasticsearch|opensearch] \
//!     [--ids FILE] [--rerun REPORT.json] [--junit FILE] [--ca-cert PEM | --insecure] \
//!     [--content-type json|yaml|cbor|smile|vnd-<type>|random] [--method-strategy natural|first|random] \
//!     [--randomize] [--seed N]
//! search-yaml list --suite DIR [--filter RE]      print test ids
//! search-yaml check --suite DIR                   parse every file, report syntax errors
//! search-yaml dump FILE...                        print each YAML document as one JSON line
//! ```
//!
//! Exit status: 0 when nothing failed (with `--baseline`: nothing regressed), 1 otherwise, 2 when
//! the run itself couldn't happen.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use regex::Regex;

use search_yaml::report::{load_failed_ids, load_id_list, load_skip_list};
use search_yaml::runner::{ClusterHook, Config, ContentTypes, SuiteRoot, discover, run};
use search_yaml::spawn::SpawnCommand;
use search_yaml::spec::MethodStrategy;
use search_yaml::suite::{Suite, suite_id};
use search_yaml::target::{Flavor, load_features_file};
use search_yaml::wipe::WipeMode;

#[derive(Parser)]
#[command(name = "search-yaml", about = "Run Elasticsearch YAML REST test suites against roll search or Elasticsearch")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run suites against a cluster.
    Run(Box<RunArgs>),
    /// Print the ids of the tests in suites.
    List(ListArgs),
    /// Parse suites and report files that don't parse.
    Check(ListArgs),
    /// Print the YAML documents of files as JSON lines (one document per line), for debugging
    /// the parser against other YAML implementations.
    Dump(DumpArgs),
}

#[derive(clap::Args)]
struct DumpArgs {
    /// YAML files.
    #[arg(required = true)]
    files: Vec<PathBuf>,
}

fn dump(args: &DumpArgs) -> anyhow::Result<bool> {
    let mut ok = true;
    for f in &args.files {
        let text = std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        match search_yaml::yaml::load_documents(&text) {
            Ok(docs) => {
                for d in docs {
                    println!("{}", d.to_value().to_json());
                }
            }
            Err(e) => {
                ok = false;
                eprintln!("{}: {e}", f.display());
            }
        }
    }
    Ok(ok)
}

#[derive(clap::Args)]
struct ListArgs {
    /// A directory (or file) of .yml tests; `label=dir` prefixes ids with `label:`. Repeatable.
    #[arg(long, required = true)]
    suite: Vec<String>,
    /// Only tests whose id matches this regex.
    #[arg(long)]
    filter: Option<String>,
}

#[derive(clap::Args)]
struct RunArgs {
    /// The cluster: http[s]://[user:pass@]host:port[/prefix]. Repeatable: identical clusters, one
    /// worker each.
    #[arg(long, env = "SEARCH_YAML_URL")]
    url: Vec<String>,
    /// A directory (or file) of .yml tests; `label=dir` prefixes ids with `label:`. Repeatable.
    #[arg(long, required = true)]
    suite: Vec<String>,
    /// The REST API spec directory (rest-api-spec/api). Defaults to one found next to the suite.
    #[arg(long)]
    api_spec: Option<PathBuf>,
    /// Only tests whose id matches this regex.
    #[arg(long)]
    filter: Option<String>,
    /// Only run the test ids listed in this file (a baseline, or `write-baseline` output).
    /// Repeatable; combines with --filter.
    #[arg(long)]
    ids: Vec<PathBuf>,
    /// Only run the tests that failed in this earlier JSON report (--report). Repeatable.
    #[arg(long)]
    rerun: Vec<PathBuf>,
    /// Write a JUnit XML report here (one testsuite per API area), for CI.
    #[arg(long)]
    junit: Option<PathBuf>,
    /// Tests to skip: `<glob>  # reason` per line. Repeatable.
    #[arg(long)]
    skip_list: Vec<PathBuf>,
    /// Write the full JSON report here.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Compare against a baseline (ids expected to pass); regressions fail the run.
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Write the ids of passing tests here.
    #[arg(long)]
    write_baseline: Option<PathBuf>,
    /// Parallel workers (needs --cluster-hook or --spawn: every worker gets its own cluster).
    #[arg(long, default_value_t = 1)]
    jobs: usize,
    /// API dialect; detected from `GET /` by default.
    #[arg(long)]
    flavor: Option<String>,
    /// File of cluster features the target claims (one per line), on top of the reported ones.
    #[arg(long)]
    features_file: Vec<PathBuf>,
    /// A cluster feature the target claims. Repeatable.
    #[arg(long)]
    feature: Vec<String>,
    /// Between tests: full (ESRestTestCase.wipeCluster), light (deletions only) or none.
    #[arg(long)]
    wipe: Option<String>,
    /// URL that creates an isolated cluster on POST (see the crate docs for the response shape).
    #[arg(long)]
    cluster_hook: Option<String>,
    /// Header for cluster hook requests (`Name: value`). Repeatable.
    #[arg(long)]
    cluster_hook_header: Vec<String>,
    /// Start a server process per isolated cluster: a `sh -c` command with `{port}` (a free
    /// loopback port) and `{name}` substituted, e.g.
    /// `search-dev --listen 127.0.0.1:{port} --yaml-test-cluster`. Enables --jobs.
    #[arg(long, conflicts_with = "cluster_hook")]
    spawn: Option<String>,
    /// With --spawn: where the process listens.
    #[arg(long, default_value = "http://127.0.0.1:{port}")]
    spawn_url: String,
    /// With --spawn: seconds a process may take to answer HTTP.
    #[arg(long, default_value_t = 60)]
    spawn_timeout: u64,
    /// With --cluster-hook or --spawn: a fresh cluster for every test instead of every file.
    #[arg(long)]
    fresh_per_test: bool,
    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = 90)]
    timeout: u64,
    /// How to choose paths and methods: natural (POST with a body, GET without), first, or random
    /// (the Java runner's random path, method and occasional `GET ?source=`, from --seed).
    #[arg(long, default_value = "natural")]
    method_strategy: String,
    /// Content type of request bodies: json, yaml, cbor, smile (vnd-<type> for Elasticsearch's
    /// vendor types), or random: a random type per request, like Elasticsearch's runner.
    #[arg(long, default_value = "json")]
    content_type: String,
    /// Seed for --content-type random and --method-strategy random (default: from the clock;
    /// printed in the summary).
    #[arg(long)]
    seed: Option<u64>,
    /// Randomize like Elasticsearch's own runner: --content-type random --method-strategy random.
    #[arg(long)]
    randomize: bool,
    /// Don't add error_trace=true to requests.
    #[arg(long)]
    no_error_trace: bool,
    /// Override x-pack detection.
    #[arg(long)]
    xpack: Option<bool>,
    /// Run as a serverless target (elasticsearch-clients-tests `requires`).
    #[arg(long)]
    serverless: bool,
    /// Feature flags enabled on the target (clients-tests `requires.feature_flag`). Repeatable.
    #[arg(long)]
    feature_flag: Vec<String>,
    /// Extra header for every request (`Name: value`). Repeatable.
    #[arg(long)]
    header: Vec<String>,
    /// Basic auth user (with --password).
    #[arg(long, env = "SEARCH_YAML_USER")]
    user: Option<String>,
    #[arg(long, env = "SEARCH_YAML_PASSWORD")]
    password: Option<String>,
    /// For https:// targets: trust only the CA certificates in this PEM file (e.g. Elasticsearch's
    /// config/certs/http_ca.crt) instead of the platform's trust store.
    #[arg(long)]
    ca_cert: Option<PathBuf>,
    /// For https:// targets: don't verify the server certificate.
    #[arg(long)]
    insecure: bool,
    /// API key (`Authorization: ApiKey <key>`).
    #[arg(long, env = "SEARCH_YAML_API_KEY")]
    api_key: Option<String>,
    /// Failures listed in the summary.
    #[arg(long, default_value_t = 50)]
    max_failures: usize,
    /// No per-file progress lines.
    #[arg(long)]
    quiet: bool,
    /// Log every request (method, target, status, time) to stderr.
    #[arg(long)]
    trace: bool,
}

fn parse_header(h: &str) -> anyhow::Result<(String, String)> {
    let (k, v) = h.split_once(':').with_context(|| format!("header must be `Name: value`, got {h}"))?;
    Ok((k.trim().to_owned(), v.trim().to_owned()))
}

/// Finds `rest-api-spec/api` (or a sibling `api` directory) above a suite directory.
fn find_api_spec(suite: &Path) -> Option<PathBuf> {
    let mut dir = Some(suite);
    while let Some(d) = dir {
        for candidate in [d.join("api"), d.join("rest-api-spec").join("api")] {
            if candidate.join("_common.json").is_file() || candidate.join("index.json").is_file() {
                return Some(candidate);
            }
        }
        dir = d.parent();
    }
    None
}

fn roots(args: &[String]) -> Vec<SuiteRoot> {
    args.iter().map(|s| SuiteRoot::parse(s)).collect()
}

fn list(args: &ListArgs, check: bool) -> anyhow::Result<bool> {
    let filter = args.filter.as_deref().map(Regex::new).transpose()?;
    let mut ok = true;
    let mut files = 0;
    let mut tests = 0;
    for root in roots(&args.suite) {
        let base = if root.path.is_file() { root.path.parent().unwrap_or(Path::new("")).to_path_buf() } else { root.path.clone() };
        for file in discover(&root.path)? {
            files += 1;
            match Suite::load(&base, &file) {
                Ok(suite) => {
                    for t in &suite.tests {
                        let id = suite.test_id(t);
                        let id = if root.label.is_empty() { id } else { format!("{}:{id}", root.label) };
                        if filter.as_ref().is_none_or(|f| f.is_match(&id)) {
                            tests += 1;
                            if !check {
                                println!("{id}");
                            }
                        }
                    }
                }
                Err(e) => {
                    ok = false;
                    let (id, _) = suite_id(&base, &file);
                    eprintln!("{id}: {e}");
                }
            }
        }
    }
    if check {
        println!("{files} files, {tests} tests{}", if ok { "" } else { ", with parse errors" });
    }
    Ok(ok)
}

async fn run_cmd(a: RunArgs) -> anyhow::Result<bool> {
    search_yaml::http::set_tls_options(search_yaml::http::TlsOptions { ca_file: a.ca_cert.clone(), insecure: a.insecure });
    let suites = roots(&a.suite);
    let api_spec = match a.api_spec {
        Some(p) => p,
        None => suites
            .iter()
            .find_map(|s| find_api_spec(&s.path))
            .context("no --api-spec given and none found next to the suite")?,
    };
    let mut headers = Vec::new();
    for h in &a.header {
        headers.push(parse_header(h)?);
    }
    if let Some(user) = &a.user {
        headers.push(("Authorization".into(), search_yaml::http::basic_auth(user, a.password.as_deref().unwrap_or(""))));
    }
    if let Some(key) = &a.api_key {
        headers.push(("Authorization".into(), format!("ApiKey {key}")));
    }
    let mut claimed = HashSet::new();
    for f in &a.features_file {
        claimed.extend(load_features_file(f)?);
    }
    claimed.extend(a.feature.iter().cloned());
    let mut skip_list = Vec::new();
    for f in &a.skip_list {
        skip_list.extend(load_skip_list(f).with_context(|| format!("reading {}", f.display()))?);
    }
    let only_ids = if a.ids.is_empty() && a.rerun.is_empty() {
        None
    } else {
        let mut ids = HashSet::new();
        for f in &a.ids {
            ids.extend(load_id_list(f).with_context(|| format!("reading {}", f.display()))?);
        }
        for f in &a.rerun {
            ids.extend(load_failed_ids(f).with_context(|| format!("reading {}", f.display()))?);
        }
        Some(ids)
    };
    let cluster_hook = match &a.cluster_hook {
        Some(url) => {
            let mut hh = Vec::new();
            for h in &a.cluster_hook_header {
                hh.push(parse_header(h)?);
            }
            Some(ClusterHook { create_url: url.clone(), headers: hh })
        }
        None => None,
    };
    let spawn = match &a.spawn {
        Some(cmd) => {
            search_yaml::spawn::check_template(&a.spawn_url)?;
            let mut s = SpawnCommand::new(cmd.clone());
            s.url = a.spawn_url.clone();
            s.ready_timeout = Duration::from_secs(a.spawn_timeout);
            Some(s)
        }
        None => None,
    };
    let flavor = match a.flavor.as_deref() {
        Some(f) => Some(Flavor::parse(f).with_context(|| format!("unknown flavor {f}"))?),
        None => None,
    };
    let wipe = match a.wipe.as_deref() {
        Some(w) => Some(WipeMode::parse(w).with_context(|| format!("unknown wipe mode {w}"))?),
        None => None,
    };
    let seed = a.seed.unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0) % 1_000_000_007
    });
    let content_type = ContentTypes::parse(if a.randomize { "random" } else { &a.content_type })
        .with_context(|| format!("unknown content type {} (json, yaml, cbor, smile, vnd-<type>, random)", a.content_type))?;
    let method_strategy = match if a.randomize { "random" } else { a.method_strategy.as_str() } {
        "natural" => MethodStrategy::Natural,
        "first" => MethodStrategy::First,
        "random" => MethodStrategy::Random,
        other => bail!("unknown method strategy {other}"),
    };
    let cfg = Config {
        url: a.url.first().cloned(),
        more_urls: a.url.iter().skip(1).cloned().collect(),
        suites,
        api_spec,
        filter: a.filter.as_deref().map(Regex::new).transpose()?,
        only_ids,
        skip_list,
        jobs: a.jobs,
        flavor,
        claimed_features: claimed,
        wipe,
        cluster_hook,
        spawn,
        fresh_per_test: a.fresh_per_test,
        timeout: Duration::from_secs(a.timeout),
        method_strategy,
        error_trace: !a.no_error_trace,
        content_type,
        seed,
        xpack: a.xpack,
        serverless: a.serverless,
        feature_flags: a.feature_flag.iter().cloned().collect(),
        headers,
        progress: !a.quiet,
        trace: a.trace,
    };
    let mut report = run(cfg).await?;
    if let Some(b) = &a.baseline {
        let ids = load_id_list(b).with_context(|| format!("reading {}", b.display()))?;
        report.compare(&b.display().to_string(), &ids);
    }
    if let Some(p) = &a.report {
        report.write_json(p)?;
    }
    if let Some(p) = &a.junit {
        report.write_junit(p)?;
    }
    if let Some(p) = &a.write_baseline {
        report.write_passing(p)?;
    }
    print!("{}", report.summary(a.max_failures));
    Ok(match &report.baseline {
        Some(b) => b.regressions.is_empty(),
        None => report.totals.failed == 0,
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::List(a) => list(&a, false),
        Cmd::Check(a) => list(&a, true),
        Cmd::Dump(a) => dump(&a),
        Cmd::Run(a) => {
            let a = *a;
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
            rt.block_on(run_cmd(a))
        }
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("search-yaml: {e:#}");
            ExitCode::from(2)
        }
    }
}
