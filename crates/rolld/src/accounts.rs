//! roll's own accounts in rolld (docs/accounts.md). Always on: not a product in `ROLL_PRODUCTS`.
//!
//! On a node of its own the data is `accounts.db` under `--home`; with the system database
//! (`ROLL_SYSTEM_DATABASE`) it's that database's `accounts` schema, which every node shares
//! without leases. rolld's gate resolves session cookies through it (`auth.rs`), and every user
//! gets a roll code owner when roll code runs on the node.

use std::path::Path;
use std::sync::Arc;

/// roll code's owners for new users.
struct CodeOwners(Arc<code_control::Code>);

impl roll_accounts::Owners for CodeOwners {
    fn available(&self, handle: &str) -> Result<bool, String> {
        self.0.owner_name_free(handle).map_err(|e| e.to_string())
    }

    fn create(&self, handle: &str, name: &str) -> Result<Option<String>, String> {
        self.0.create_user_owner(handle, name).map_err(|e| e.to_string())
    }
}

pub async fn open(args: &crate::Args, home: &Path, code: Option<Arc<code_control::Code>>, open: bool) -> anyhow::Result<Arc<roll_accounts::Accounts>> {
    let target = match args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty()) {
        Some(url) => code_sql::Target::Postgres { url: url.to_string(), schema: "accounts".into() },
        None => code_sql::Target::Sqlite(home.join("accounts.db")),
    };
    let pg = code_sql::PgOptions { max_connections: args.db_budget()?.accounts, ca: args.code.code_db_ca.clone(), ..Default::default() };
    let api = args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port()));
    let config = roll_accounts::Config::from_env(&api, &args.web_url, open);
    tracing::info!(
        email = !matches!(config.mail, roll_accounts::MailConfig::Off),
        github = config.github.is_some(),
        google = config.google.is_some(),
        totp = config.secret_key.is_some(),
        "accounts: sign-in methods"
    );
    let owners = code.map(|c| Arc::new(CodeOwners(c)) as Arc<dyn roll_accounts::Owners>);
    // Opening blocks on the database.
    let accounts = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(roll_accounts::Accounts::open(&target, pg, config, owners)))?;
    Ok(Arc::new(accounts))
}
