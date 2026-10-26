//! `search-proxy`: the roll search front proxy (one per availability zone), or, with
//! `--lambda-upstream`, the Lambda function URL runtime in front of the proxies.

use clap::Parser;
use search_proxy::proxy::{ListenOptions, Port, serve_health, tls_acceptor};
use search_proxy::{CommandWaker, Directory, EcsWaker, Proxy, ProxyConfig, Waker};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(name = "search-proxy", about = "roll search front proxy: routes requests to the node owning their cluster, wakes the fleet")]
struct Args {
    /// Fleet metadata: the nodes' `--search-meta` (`postgres://…`, or a SQLite file on one host).
    #[arg(long, env = "ROLL_SEARCH_META")]
    meta: Option<String>,
    /// Password for the metadata database, if not in the URL (an RDS-managed secret).
    #[arg(long, env = "ROLL_SEARCH_META_PASSWORD", hide_env_values = true)]
    meta_password: Option<String>,
    /// More CA certificates (PEM) to verify the metadata Postgres with (e.g. the RDS bundle).
    #[arg(long, env = "ROLL_SEARCH_META_CA")]
    meta_ca: Option<PathBuf>,
    /// Elasticsearch REST listener.
    #[arg(long, env = "ROLL_SEARCH_PROXY_LISTEN", default_value = "0.0.0.0:9200")]
    listen: SocketAddr,
    /// Control API (`/v1/search`) listener; off unless set.
    #[arg(long, env = "ROLL_SEARCH_PROXY_API_LISTEN")]
    api_listen: Option<SocketAddr>,
    /// Health check listener for load balancers (never touches nodes).
    #[arg(long, env = "ROLL_SEARCH_PROXY_HEALTH")]
    health: Option<SocketAddr>,
    /// This proxy's availability zone (default: from the ECS task metadata).
    #[arg(long, env = "ROLL_SEARCH_AZ")]
    az: Option<String>,
    /// Cluster host names are `<cluster>.<domain>` (SNI or `Host`), as on the nodes.
    #[arg(long, env = "ROLL_SEARCH_DOMAIN")]
    domain: Option<String>,
    /// Serve TLS with this PEM certificate chain (file) …
    #[arg(long, env = "ROLL_SEARCH_PROXY_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    #[arg(long, env = "ROLL_SEARCH_PROXY_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// … or with this PEM chain and key given inline (secrets injected as environment variables).
    #[arg(long, env = "ROLL_SEARCH_PROXY_TLS_CERT_PEM", hide_env_values = true, requires = "tls_key_pem", conflicts_with = "tls_cert")]
    tls_cert_pem: Option<String>,
    #[arg(long, env = "ROLL_SEARCH_PROXY_TLS_KEY_PEM", hide_env_values = true, requires = "tls_cert_pem")]
    tls_key_pem: Option<String>,
    /// … or with a certificate made at boot (behind an NLB TLS target group, which encrypts to
    /// targets without verifying them) …
    #[arg(long, env = "ROLL_SEARCH_PROXY_TLS_SELF_SIGNED", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true", conflicts_with_all = ["tls_cert", "tls_cert_pem", "tls_seed"])]
    tls_self_signed: bool,
    /// … or with the certificate made from this secret (the Lambda function, given the same
    /// secret as `ROLL_SEARCH_LAMBDA_TLS_SEED`, accepts only it). At least 16 bytes.
    #[arg(long, env = "ROLL_SEARCH_PROXY_TLS_SEED", hide_env_values = true, conflicts_with_all = ["tls_cert", "tls_cert_pem"])]
    tls_seed: Option<String>,
    /// Expect PROXY protocol (v1 or v2) headers, as an NLB sends with proxy_protocol_v2.enabled.
    #[arg(long, env = "ROLL_SEARCH_PROXY_PROTOCOL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    proxy_protocol: bool,
    /// Wake by scaling this ECS service (`cluster/service`) to `--wake-count` tasks.
    #[arg(long, env = "ROLL_SEARCH_PROXY_WAKE_ECS_SERVICE")]
    wake_ecs_service: Option<String>,
    #[arg(long, env = "ROLL_SEARCH_PROXY_WAKE_COUNT", default_value_t = 1)]
    wake_count: u32,
    /// Shell command that starts nodes when none runs (instead of ECS).
    #[arg(long, env = "ROLL_SEARCH_PROXY_WAKE_COMMAND")]
    wake_command: Option<String>,
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,
    /// Seconds a request waits for a node (wake, handoff, failover).
    #[arg(long, env = "ROLL_SEARCH_PROXY_HOLD_TIMEOUT", default_value_t = 120)]
    hold_timeout: u64,
    /// A node is gone after this many seconds without a heartbeat.
    #[arg(long, env = "ROLL_SEARCH_NODE_TTL", default_value_t = 5)]
    node_ttl: u64,
    /// Request bodies up to this many MiB are kept while they stream, so a refused one is resent
    /// to the owner (a node's `http.max_content_length` is 100 MiB).
    #[arg(long, env = "ROLL_SEARCH_PROXY_RETRY_BODY_MB", default_value_t = 100)]
    retry_body_mb: usize,
    /// MiB of request bodies kept for resends across all requests in flight.
    #[arg(long, env = "ROLL_SEARCH_PROXY_RETRY_BUFFER_MB", default_value_t = 128)]
    retry_buffer_mb: usize,
    /// Seconds in-flight requests may finish after SIGTERM.
    #[arg(long, env = "ROLL_SEARCH_PROXY_DRAIN", default_value_t = 30)]
    drain: u64,
    /// Run as the Lambda function URL runtime (needs `--lambda-upstream`). The function's
    /// image command: set explicitly, so the image's default command (`rolld`, meant for the
    /// tini entrypoint) is never passed to this binary.
    #[arg(long)]
    lambda: bool,
    /// Lambda mode: forward function URL invocations to this URL (the internal NLB).
    #[arg(long, env = "ROLL_SEARCH_LAMBDA_UPSTREAM")]
    lambda_upstream: Option<String>,
    /// Lambda mode: send `/v1/search…` (the control API) here instead (the proxies' API port).
    #[arg(long, env = "ROLL_SEARCH_LAMBDA_API_UPSTREAM", requires = "lambda_upstream")]
    lambda_api_upstream: Option<String>,
    /// Lambda mode: `https://` upstreams are proxies serving the certificate made from this
    /// secret (their `ROLL_SEARCH_PROXY_TLS_SEED`); nothing else is accepted.
    #[arg(long, env = "ROLL_SEARCH_LAMBDA_TLS_SEED", hide_env_values = true, requires = "lambda_upstream")]
    lambda_tls_seed: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    if args.lambda && args.lambda_upstream.as_deref().is_none_or(str::is_empty) {
        anyhow::bail!("--lambda needs --lambda-upstream (ROLL_SEARCH_LAMBDA_UPSTREAM)");
    }
    if let Some(up) = args.lambda_upstream.as_ref().filter(|u| !u.is_empty()) {
        let api = std::env::var("AWS_LAMBDA_RUNTIME_API").map_err(|_| anyhow::anyhow!("--lambda-upstream runs inside Lambda (AWS_LAMBDA_RUNTIME_API is not set)"))?;
        let seed = args.lambda_tls_seed.as_deref().filter(|s| !s.is_empty()).map(str::as_bytes);
        return search_proxy::lambda::run(&api, up, args.lambda_api_upstream.as_deref().filter(|u| !u.is_empty()), seed).await;
    }
    let meta = args.meta.as_deref().ok_or_else(|| anyhow::anyhow!("--meta (ROLL_SEARCH_META) is required"))?;
    let az = match args.az.clone().filter(|a| !a.is_empty()) {
        Some(a) => Some(a),
        None => search_proxy::aws::ecs_availability_zone().await,
    };
    let tls = match (&args.tls_cert, &args.tls_key, &args.tls_cert_pem, &args.tls_key_pem) {
        (Some(c), Some(k), _, _) => Some(tls_acceptor(&std::fs::read(c)?, &std::fs::read(k)?)?),
        (_, _, Some(c), Some(k)) => Some(tls_acceptor(c.as_bytes(), k.as_bytes())?),
        _ if args.tls_self_signed => {
            let c = search_proxy::upstream::self_signed(&["localhost".into()])?;
            Some(tls_acceptor(c.cert_pem.as_bytes(), c.key_pem.as_bytes())?)
        }
        _ => match args.tls_seed.as_deref().filter(|s| !s.is_empty()) {
            Some(seed) => {
                let c = search_proxy::upstream::seeded(seed.as_bytes())?;
                Some(tls_acceptor(c.cert_pem.as_bytes(), c.key_pem.as_bytes())?)
            }
            None => None,
        },
    };
    let members = search_proxy::fleet::open(meta, args.meta_ca.as_deref(), args.meta_password.as_deref()).await?;
    let cfg = ProxyConfig {
        az: az.clone(),
        domain: args.domain.clone().filter(|d| !d.is_empty()),
        node_ttl: Duration::from_secs(args.node_ttl.max(1)),
        hold_timeout: Duration::from_secs(args.hold_timeout.max(1)),
        retry_body_bytes: args.retry_body_mb << 20,
        retry_buffer_bytes: args.retry_buffer_mb << 20,
        ..ProxyConfig::default()
    };
    let directory = Directory::new(members, cfg);
    // A database that is still starting (or failing over) must not crash-loop the proxy: the
    // refresh loop keeps trying, and requests are held meanwhile.
    if let Err(e) = directory.refresh().await {
        tracing::warn!(error = %e, "reading search node records failed; retrying in the background");
    }
    directory.spawn_refresh();
    let waker: Option<Arc<dyn Waker>> = match (&args.wake_ecs_service, &args.wake_command) {
        (Some(svc), _) => {
            let (cluster, service) = svc.split_once('/').ok_or_else(|| anyhow::anyhow!("--wake-ecs-service wants cluster/service"))?;
            let region = args.region.clone().ok_or_else(|| anyhow::anyhow!("--wake-ecs-service needs AWS_REGION"))?;
            Some(Arc::new(EcsWaker::new(cluster.into(), service.into(), args.wake_count, region)))
        }
        (None, Some(c)) => Some(Arc::new(CommandWaker(c.clone()))),
        (None, None) => None,
    };
    let proxy = Proxy::new(directory, waker);
    if let Some(h) = args.health {
        tokio::spawn(serve_health(tokio::net::TcpListener::bind(h).await?));
    }
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let drain = Duration::from_secs(args.drain);
    let rest = ListenOptions { port: Port::Rest, tls: tls.clone(), proxy_protocol: args.proxy_protocol, drain };
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    info!(addr = %args.listen, tls = tls.is_some(), proxy_protocol = args.proxy_protocol, az = ?az, "search proxy listening");
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(proxy.clone().serve(listener, rest, stop_rx.clone()));
    if let Some(a) = args.api_listen {
        let l = tokio::net::TcpListener::bind(a).await?;
        info!(addr = %a, "search proxy API listening");
        tasks.spawn(proxy.clone().serve(l, ListenOptions { port: Port::Api, tls, proxy_protocol: args.proxy_protocol, drain }, stop_rx));
    }
    tokio::select! {
        r = tasks.join_next() => {
            if let Some(r) = r {
                r??;
            }
        }
        _ = shutdown_signal() => {
            info!("shutting down: finishing requests in progress");
            let _ = stop_tx.send(true);
            while let Some(r) = tasks.join_next().await {
                r??;
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        super::Args::command().debug_assert();
    }

    /// The Lambda function overrides the image's entrypoint (tini) with this binary; Lambda
    /// passes the image's command (`rolld`) to whatever entrypoint runs unless the function sets
    /// its own, which then has to parse.
    #[test]
    fn the_lambda_function_sets_a_command_this_binary_takes() {
        use clap::Parser;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/search");
        let template = std::fs::read_to_string(dir.join("aws/roll-search.yaml")).unwrap();
        let dockerfile = std::fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(dockerfile.lines().any(|l| l.trim() == r#"CMD ["rolld"]"#), "the image's default command changed: re-check the function");
        let line = template.lines().find(|l| l.trim_start().starts_with("ImageConfig:")).expect("ProxyFunction ImageConfig");
        let list = |key: &str| -> Vec<String> {
            let at = line.find(&format!("{key}: [")).unwrap_or_else(|| panic!("{key} missing in {line}")) + key.len() + 3;
            line[at..at + line[at..].find(']').unwrap()].split(',').map(|s| s.trim().to_string()).collect()
        };
        assert_eq!(list("EntryPoint"), ["/usr/local/bin/search-proxy"]);
        let command = list("Command");
        assert_eq!(command, ["--lambda"]);
        // As Lambda runs it (the upstreams come from the function's environment).
        let args = super::Args::try_parse_from(std::iter::once("search-proxy".to_string()).chain(command)).unwrap();
        assert!(args.lambda);
        // What the image's command alone would have done: exit 2 at every cold start.
        assert!(super::Args::try_parse_from(["search-proxy", "rolld"]).is_err());
    }
}
