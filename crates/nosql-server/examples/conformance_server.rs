//! A single-database roll nosql server for `scripts/nosql-conformance.sh --roll <dialect>`.
//!
//! Serves one database over CQL, the ScyllaDB REST API and (ScyllaDB dialect) Alternator on
//! fixed addresses, with object storage in a local directory, and runs until killed.
//!
//! ```text
//! cargo run -p nosql-server --example conformance_server -- \
//!     --dialect scylla --ip 127.0.0.2 [--cql-port 9042] [--rest-port 10000] \
//!     [--alternator-port 8000] [--config default|suite] [--data DIR]
//! ```
//!
//! `--config default` is the reference image's configuration (AllowAllAuthenticator, features as
//! shipped). `--config suite` matches the flags the suites' own `run` / `run-cassandra` scripts
//! give the references: PasswordAuthenticator + CassandraAuthorizer with the superuser
//! `cassandra`/`cassandra` (the engine's initial superuser), user-defined functions,
//! materialized views and SASI enabled; for Alternator, test/alternator/run's flags: SigV4
//! enforced (the role's salted hash is the secret), `only_rmw_uses_lwt` write isolation, no
//! streams time window and a 0.5 s TTL scan period. `--alternator-port 0` turns Alternator off.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use nosql_engine::config::Superuser;
use nosql_engine::{Database, DbConfig, Dialect};
use nosql_server::{Credential, Directory, ListenOptions, Single, alternator, net, rest};
use nosql_storage::store::{Store, StoreConfig};
use nosql_storage::{Objects, StorageConfig};

struct Args {
    dialect: Dialect,
    ip: IpAddr,
    cql_port: u16,
    rest_port: u16,
    alternator_port: u16,
    suite: bool,
    data: Option<PathBuf>,
}

fn parse() -> anyhow::Result<Args> {
    let mut a = Args { dialect: Dialect::Scylla, ip: "127.0.0.1".parse()?, cql_port: 9042, rest_port: 10000, alternator_port: 8000, suite: false, data: None };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--dialect" => {
                a.dialect = match value()?.as_str() {
                    "scylla" => Dialect::Scylla,
                    "cassandra" => Dialect::Cassandra,
                    d => bail!("unknown dialect {d}"),
                }
            }
            "--ip" => a.ip = value()?.parse()?,
            "--cql-port" => a.cql_port = value()?.parse()?,
            "--rest-port" => a.rest_port = value()?.parse()?,
            "--alternator-port" => a.alternator_port = value()?.parse()?,
            "--config" => {
                a.suite = match value()?.as_str() {
                    "default" => false,
                    "suite" => true,
                    c => bail!("unknown config {c}"),
                }
            }
            "--data" => a.data = Some(value()?.into()),
            f => bail!("unknown flag {f}"),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let args = parse()?;

    let data = match &args.data {
        Some(d) => d.clone(),
        None => std::env::temp_dir().join(format!("roll-nosql-conformance-{}", std::process::id())),
    };
    std::fs::create_dir_all(&data)?;
    let objects = Objects::new(&StorageConfig::Local(data.clone()))?;
    let store = Store::start(objects, StoreConfig::default()).await?;

    let mut cfg = DbConfig::new("conformance", args.dialect);
    cfg.address = args.ip;
    cfg.native_port = args.cql_port;
    if args.suite {
        let s = &mut cfg.settings;
        s.authenticator = "PasswordAuthenticator".into();
        s.authorizer = "CassandraAuthorizer".into();
        s.user_defined_functions_enabled = true;
        s.materialized_views_enabled = true;
        s.sasi_indexes_enabled = true;
        // test/cqlpy/run-cassandra: `-Dcassandra.sai.vector.allow_custom_parameters=true`.
        s.sai_vector_allow_custom_parameters = true;
        // test/cqlpy/run.py: `--strict-allow-filtering=true`.
        s.strict_allow_filtering = nosql_engine::StrictAllowFiltering::True;
        // `--minimum-replication-factor-warn-threshold=-1 --query-tombstone-page-limit=1000`.
        s.minimum_replication_factor_warn_threshold = -1;
        s.scylla.query_tombstone_page_limit = 1000;
        // `--alternator-ttl-period-in-seconds=0.5` (per-row TTL expiration too).
        s.scylla.alternator_ttl_period_in_seconds = 0.5;
        // `--auth-superuser-name=cassandra` (ScyllaDB has no default superuser; Cassandra's
        // default role is the same one).
        cfg.superuser = Some(Superuser { name: "cassandra".into(), password: "cassandra".into() });
    }
    let handle = store.open(&cfg.id, Default::default()).await?;
    let db = Database::open(cfg, handle).await?;
    let single = Single::with_spec(db, |spec| {
        if args.suite {
            spec.credentials.push(Credential { role: "cassandra".into(), password: "cassandra".into() });
        }
    });
    let dir: Arc<dyn Directory> = Arc::new(single);

    let cql = net::start_at(dir.clone(), ListenOptions::default(), SocketAddr::new(args.ip, args.cql_port)).await?;
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(args.ip, args.rest_port)).await?;
    let rest_addr = listener.local_addr()?;
    tokio::spawn(rest::serve(listener, dir.clone()));
    let mut alternator = String::new();
    let mut alternator_opts = None;
    if args.alternator_port != 0 && args.dialect == Dialect::Scylla {
        let mut opts = alternator::AlternatorOptions::default();
        opts.config.local_nodes[0].address = args.ip.to_string();
        if args.suite {
            opts.config.write_isolation = nosql_alternator::WriteIsolation::OnlyRmwUsesLwt;
            opts.config.streams_time_window_ms = 0;
            opts.config.allow_system_table_write = true;
            opts.ttl_period = std::time::Duration::from_millis(500);
        }
        let addr = alternator::start(dir.clone(), opts.clone(), SocketAddr::new(args.ip, args.alternator_port)).await?;
        alternator = format!(", Alternator on {addr}");
        alternator_opts = Some(opts);
    }
    if args.dialect == Dialect::Scylla {
        // ScyllaDB's Prometheus port: CQL transport and guardrail metrics, and Alternator's.
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(args.ip, 9180)).await?;
        let router = nosql_server::metrics::router(dir, alternator_opts);
        tokio::spawn(async move { axum::serve(listener, router).await });
    }
    println!("conformance_server: {:?} CQL on {cql}, REST on {rest_addr}{alternator}, data in {}", args.dialect, data.display());

    tokio::signal::ctrl_c().await?;
    Ok(())
}
