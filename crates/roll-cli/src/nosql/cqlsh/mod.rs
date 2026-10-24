//! `roll nosql cqlsh`: a CQL shell with cqlsh's behaviour and output, over `nosql_proto`'s client.
//!
//! The shell reproduces the cqlsh shipped with each reference: Cassandra's 6.2 when the server is
//! Cassandra, the 6.0 in the ScyllaDB image when it is ScyllaDB (detected from SUPPORTED). Its
//! layout and formatting are ported from Apache Cassandra's `pylib/cqlshlib` (Apache-2.0); see
//! each module. Non-interactive output (`-e`, `-f`, piped input) is meant to be byte-for-byte
//! what cqlsh prints; `tests/cqlsh.rs` checks it against both references.

pub mod complete;
pub mod copy;
pub mod describe;
pub mod export;
pub mod format;
pub mod help;
pub mod lex;
pub mod pyfmt;
pub mod render;
pub mod session;
pub mod shell;
pub mod trace;

use std::cell::RefCell;
use std::io::IsTerminal;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Args as ClapArgs;
use nosql_proto::client::{ClientError, TlsConfig};
use nosql_proto::{Consistency, Version};

use format::Flavor;
use session::{Conn, Target, py_repr};
use shell::{Input, Io, Shell, Versions};

/// cqlsh's options.
#[derive(ClapArgs, Clone, Debug, Default)]
pub struct Args {
    /// Host to connect to (instead of a roll database).
    #[arg(long, env = "CQLSH_HOST")]
    pub host: Option<String>,
    #[arg(long, env = "CQLSH_PORT")]
    pub port: Option<u16>,
    /// Authenticate as user.
    #[arg(short = 'u', long)]
    pub username: Option<String>,
    /// Authenticate using password.
    #[arg(short = 'p', long)]
    pub password: Option<String>,
    /// Authenticate to the given keyspace.
    #[arg(short = 'k', long)]
    pub keyspace: Option<String>,
    /// Execute the statement and quit.
    #[arg(short = 'e', long)]
    pub execute: Option<String>,
    /// Execute commands from FILE, then exit.
    #[arg(short = 'f', long)]
    pub file: Option<String>,
    /// Use SSL.
    #[arg(long)]
    pub ssl: bool,
    /// The TLS server name (SNI), when it differs from the host.
    #[arg(long)]
    pub server_name: Option<String>,
    /// Always use color output.
    #[arg(short = 'C', long)]
    pub color: bool,
    /// Never use color output.
    #[arg(long, conflicts_with = "color")]
    pub no_color: bool,
    /// Force tty mode (command prompt).
    #[arg(short = 't', long)]
    pub tty: bool,
    /// Specify a specific protocol version.
    #[arg(long)]
    pub protocol_version: Option<u8>,
    /// Connection timeout in seconds.
    #[arg(long, default_value_t = 5)]
    pub connect_timeout: u64,
    /// Request timeout in seconds.
    #[arg(long, default_value_t = 10)]
    pub request_timeout: u64,
    /// Disable saving of history.
    #[arg(long)]
    pub disable_history: bool,
    /// Accepted for compatibility (output is always UTF-8).
    #[arg(long)]
    pub encoding: Option<String>,
    /// Accepted for compatibility.
    #[arg(long)]
    pub cqlversion: Option<String>,
    /// Behave like the cqlsh of this product instead of the one the server suggests.
    #[arg(long, hide = true, value_parser = ["cassandra", "scylla"])]
    pub flavor: Option<String>,
    #[arg(long, hide = true)]
    pub insecure_password_without_warning: bool,
}

/// `Connection error: ('Unable to connect to any servers', {'host:port': Error('…')})`.
pub fn connect_error(target: &Target, what: &str) -> String {
    format!("('Unable to connect to any servers', {{'{}': {what}}})", target.addr())
}

fn class_repr(class: &str, msg: &str) -> String {
    format!("{class}({})", py_repr(msg))
}

/// The driver's text for a failed connect.
pub fn connect_failure(target: &Target, e: &ClientError) -> String {
    let what = match e {
        ClientError::Server(b) if b.code == nosql_proto::error_body::code::BAD_CREDENTIALS => class_repr(
            "AuthenticationFailed",
            &format!("Failed to authenticate to {}: {}", target.addr(), session::summary_msg(b)),
        ),
        ClientError::Server(b) => {
            let (class, msg) = session::error_message(b);
            class_repr(class, &msg)
        }
        ClientError::Auth(m) if m.contains("requires authentication") => {
            class_repr("AuthenticationFailed", "Remote end requires authentication")
        }
        ClientError::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => format!(
            "ConnectionRefusedError(111, \"Tried connecting to [('{}', {})]. Last error: Connection refused\")",
            target.host, target.port
        ),
        ClientError::Timeout => class_repr("OperationTimedOut", "errors=Timed out creating connection (5 seconds), last_host=None"),
        other => class_repr("ConnectionException", &other.to_string()),
    };
    connect_error(target, &what)
}

/// Reads a password without echo (`getpass`).
pub fn read_password() -> String {
    if std::io::stdin().is_terminal() {
        let mut ed = match rustyline::Editor::<complete::Masked, rustyline::history::DefaultHistory>::new() {
            Ok(e) => e,
            Err(_) => return String::new(),
        };
        ed.set_helper(Some(complete::Masked));
        tokio::task::block_in_place(|| ed.readline("Password: ")).unwrap_or_default()
    } else {
        let mut l = String::new();
        let _ = std::io::stdin().read_line(&mut l);
        l.trim_end_matches(['\r', '\n']).to_string()
    }
}

fn history_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("CQL_HISTORY") {
        return Some(p.into());
    }
    let home = std::env::var("HOME").ok()?;
    Some(std::path::Path::new(&home).join(".cassandra").join("roll_cqlsh_history"))
}

fn tls_config(server_name: &str) -> Result<TlsConfig, String> {
    let Ok(file) = std::env::var("SSL_CERTFILE") else {
        return TlsConfig::native_roots(server_name).map_err(|e| e.to_string());
    };
    let pem = std::fs::read(&file).map_err(|e| format!("{file}: {e}"))?;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut &pem[..]) {
        let c = c.map_err(|e| e.to_string())?;
        roots.add(c).map_err(|e| e.to_string())?;
    }
    TlsConfig::with_roots(roots, server_name).map_err(|e| e.to_string())
}

/// Where to connect, resolved from a roll database or the flags.
#[derive(Clone, Debug, Default)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub server_name: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Runs the shell; returns the process exit code.
pub async fn run(args: Args, ep: Endpoint) -> i32 {
    let mut stderr = std::io::stderr();
    let username = args.username.clone().or(ep.username.clone());
    let mut password = args.password.clone().or(ep.password.clone());
    if args.password.is_some() && !args.insecure_password_without_warning {
        use std::io::Write;
        let _ = writeln!(
            stderr,
            "\nWarning: Using a password on the command line interface can be insecure.\nRecommendation: use the credentials file to securely provide the password.\n"
        );
    }
    if username.is_some() && password.is_none() {
        password = Some(read_password());
    }
    let tls = if args.ssl || ep.tls {
        let name = args.server_name.clone().or(ep.server_name.clone()).unwrap_or_else(|| ep.host.clone());
        match tls_config(&name) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("Connection error: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    let target = Target {
        host: ep.host.clone(),
        port: ep.port,
        credentials: username.clone().map(|u| (u, password.clone().unwrap_or_default())),
        tls,
        protocol: args.protocol_version.and_then(Version::from_u8),
        connect_timeout: Duration::from_secs(args.connect_timeout.max(1)),
        request_timeout: Duration::from_secs(args.request_timeout.max(1)),
    };
    let client = match target.connect().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Connection error: {}", connect_failure(&target, &e));
            return 1;
        }
    };
    let flavor = match args.flavor.as_deref() {
        Some("cassandra") => Flavor::Cassandra,
        Some("scylla") => Flavor::Scylla,
        _ if client.ctx().dialect == nosql_proto::Dialect::Scylla => Flavor::Scylla,
        _ => Flavor::Cassandra,
    };
    let conn = Conn { client, target: Arc::new(target) };
    if let Some(ks) = &args.keyspace
        && conn.query(&format!("USE {}", lex::protect_name(ks)), nosql_proto::QueryParams::default(), false).await.is_err()
    {
        eprintln!("Connection error: (\"Unable to connect to any servers using keyspace {}\", [{}])", py_repr(ks), py_repr(&conn.target.host));
        return 1;
    }
    let tty = if args.execute.is_some() || args.file.is_some() { false } else { args.tty || std::io::stdin().is_terminal() };
    let color = if args.color {
        true
    } else if args.no_color || args.file.is_some() {
        false
    } else {
        std::io::stdout().is_terminal() && !matches!(std::env::var("TERM").as_deref(), Ok("dumb") | Ok("") | Err(_))
    };
    let mut opts = format::Opts::new(flavor);
    opts.color = color;
    opts.zone = pyfmt::Zone::from_env();
    let (input, name) = if let Some(e) = &args.execute {
        let e = if e.ends_with(';') { e.clone() } else { format!("{e};") };
        (Input::Single(e), "<stdin>".to_string())
    } else if let Some(f) = &args.file {
        match std::fs::File::open(f) {
            Ok(file) => (Input::Lines(Box::new(std::io::BufReader::new(shell::skip_bom(file)))), f.clone()),
            Err(e) => {
                eprintln!("Can't open {}: {}", py_repr(f), shell::os_error(&e, f));
                return 1;
            }
        }
    } else if tty && !std::io::stdin().is_terminal() {
        (Input::Prompted(Box::new(std::io::BufReader::new(std::io::stdin()))), "<stdin>".to_string())
    } else if tty {
        let cfg = rustyline::Config::builder().auto_add_history(true).max_history_size(1000).map(|b| b.build()).unwrap_or_default();
        let mut ed: complete::Editor = match rustyline::Editor::with_config(cfg) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };
        ed.set_helper(Some(complete::CqlHelper { schema: Arc::new(Mutex::new(complete::Schema::default())), flavor }));
        if let Some(p) = history_path() {
            let _ = ed.load_history(&p);
        }
        (Input::Tty(Box::new(ed)), "<stdin>".to_string())
    } else {
        (Input::Lines(Box::new(std::io::BufReader::new(std::io::stdin()))), "<stdin>".to_string())
    };
    // Observed: ScyllaDB's cqlsh describes from driver metadata for `-e` with several statements
    // only; one `-e` statement, `-f` and SOURCE use server-side DESCRIBE.
    let client_describe =
        matches!(&args.execute, Some(e) if lex::split(e).map(|s| s.statements.iter().filter(|s| !s.is_empty()).count() > 1).unwrap_or(false));
    let io = Rc::new(RefCell::new(Io { stdout: Box::new(std::io::stdout()), stderr: Box::new(std::io::stderr()) }));
    let mut sh = Shell {
        conn,
        flavor,
        opts,
        io,
        input,
        username: username.clone(),
        keyspace: args.keyspace.clone(),
        consistency: Consistency::One,
        serial: Consistency::Serial,
        tracing: false,
        expand: false,
        paging: true,
        page_size: shell::DEFAULT_PAGE_SIZE,
        tty,
        show_line_nums: !tty,
        lineno: 1,
        stdin_name: name,
        statement_error: false,
        stop: false,
        statement: String::new(),
        prompt: String::new(),
        empty_lines: 0,
        lastcmd: String::new(),
        capture: None,
        versions: Versions::default(),
        max_trace_wait: 10.0,
        client_describe,
        history: vec![],
        max_history_shown: 50,
        date_overflow_warned: false,
        decoding_errors: vec![],
        meta: shell::MetaTrack::default(),
        col_kinds: vec![],
    };
    if client_describe {
        for q in ["SELECT keyspace_name, table_name FROM system_schema.tables", "SELECT keyspace_name, view_name FROM system_schema.views"] {
            for r in sh.conn.strings(q).await.unwrap_or_default() {
                if let [Some(k), Some(t)] = &r[..] {
                    sh.meta.at_start.insert((k.clone(), t.clone()));
                }
            }
        }
    }
    if let Err(f) = sh.load_versions().await {
        eprintln!("Connection error: {}", connect_error(&sh.conn.target, &f.line()));
        return 1;
    }
    sh.check_build_versions();
    if tty {
        if flavor == Flavor::Cassandra && !args.disable_history && std::io::stdin().is_terminal() {
            let path = history_path().map(|p| p.display().to_string()).unwrap_or_default();
            sh.print("");
            sh.print(&format!("ATTENTION: All commands will be saved to history file: {path}"));
            sh.print("This may include sensitive information such as passwords.");
            sh.print("To disable history, use --disable-history or set 'disabled = true' in the [history] section of cqlshrc.");
            sh.print("See https://cassandra.apache.org/doc/latest/tools/cqlsh.html for more information.");
            sh.print("");
        }
        sh.reset_prompt();
        sh.show_host();
        sh.print(&shell::version_line(flavor, &sh.versions));
        sh.print("Use HELP for help.");
    }
    sh.reset_prompt();
    sh.cmdloop().await;
    sh.flush();
    if let Input::Tty(ed) = &mut sh.input
        && !args.disable_history
        && let Some(p) = history_path()
    {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = ed.save_history(&p);
    }
    if !tty && sh.statement_error { 2 } else { 0 }
}
