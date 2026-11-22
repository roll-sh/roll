//! `roll token …` and `roll mcp …`: scoped tokens, approvals and the audit log
//! (docs/permissions.md), and connecting MCP clients (docs/mcp.md).

use crate::{Api, ago, s};
use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum Token {
    /// Create a scoped token from a preset, rules of your own, or both. The token is shown once.
    ///
    /// Examples:
    ///   roll token create ci --preset read-only --expires 90d
    ///   roll token create agent --preset project --project prj_x4k9…
    ///   roll token create migrate --product postgres --resource prj_ab… --allow read,write --ask delete
    Create(Box<Create>),
    /// List tokens and OAuth connections, newest first.
    ///   roll token list --product secrets --resource prj_x4k9…   (the ones made for it)
    ///   roll token list --workloads                               (workloads' tokens for minutes)
    ///   roll token list --pending                                 (people's, waiting for an admin)
    #[command(alias = "ls")]
    List {
        /// Only tokens with a rule on this product by name.
        #[arg(long)]
        product: Option<String>,
        /// With --product: only those whose rule names this resource (an id or project).
        #[arg(long, requires = "product")]
        resource: Option<String>,
        /// Workload tokens instead (a CI job's, for minutes; the latest 500).
        #[arg(long, conflicts_with_all = ["product", "resource"])]
        workloads: bool,
        /// Only people's tokens waiting for an admin's approval (the policy's approve_people).
        #[arg(long, conflicts_with = "workloads")]
        pending: bool,
    },
    /// Show a token's rules, expiry and last use.
    Show { id: String },
    /// What an agent connected with a token is offered over MCP: its tools (those that change
    /// things marked), prompts, and what the list costs an agent's context.
    Tools { id: String },
    /// Revoke a token or connection: every request with it fails from now on.
    Revoke { id: String },
    /// Let a person's pending token work (an admin's; the person is mailed).
    Approve { id: String },
    /// Refuse a person's pending token: it's revoked, and they're mailed.
    Deny { id: String },
    /// A new secret for a token, with the same rules; the old one stops working at once.
    Rotate {
        id: String,
        /// The old secret keeps working this long (1h, 2d; at most 7d) while deployments switch.
        #[arg(long)]
        keep_old: Option<String>,
    },
    /// Change a token: its name, expiry, addresses, or (with --preset/--allow/--ask) its rules.
    Edit(Box<Edit>),
    /// Its rules narrowed to the actions and resources its requests used (never wider); `--apply`
    /// makes them its rules.
    ///
    /// Examples:
    ///   roll token tighten tok_…            what it would keep
    ///   roll token tighten tok_… --apply    keep only that
    Tighten {
        id: String,
        /// Days of activity to go by (1-90).
        #[arg(long, default_value_t = 30)]
        days: u32,
        #[arg(long)]
        apply: bool,
    },
    /// Take back what an approval allowed a token for a while (`roll token show` lists them).
    TakeBack { id: String, approval: String },
    /// Ask a person, with the token you're using (ROLL_TOKEN), for one more rule for a while,
    /// saying why; `--wait` waits for the decision.
    ///
    /// Example:
    ///   roll token request --product postgres --allow delete --resource prj_ab… --for 1h \
    ///     --reason "drop the scratch database" --wait
    Request {
        #[arg(long)]
        product: String,
        /// One action: read, write, delete, reveal, deploy or admin.
        #[arg(long)]
        allow: String,
        /// One resource (every resource of the product when left out).
        #[arg(long)]
        resource: Option<String>,
        #[arg(long)]
        environment: Option<String>,
        /// 15m, 1h, 8h or 1d.
        #[arg(long = "for", default_value = "1h")]
        for_: String,
        /// Why: the person deciding reads it.
        #[arg(long)]
        reason: String,
        /// Wait (up to 10 minutes) until a person decides.
        #[arg(long)]
        wait: bool,
    },
    /// Which tokens need attention and why (admin over everything, never expiring, unused,
    /// narrower than their rules, roaming), each with its fix.
    Checkup,
    /// Which credential this CLI uses and where it came from (`ROLL_TOKEN`, `ROLL_ADMIN_TOKEN`,
    /// `roll login`), what it may do, and any other credential set that loses to it.
    Whoami,
    /// The products, actions and presets the builder offers.
    Presets,
    /// Machine clients: automation with no person (CI, a scheduled agent) that trades a JWT it
    /// signed with its key, or a secret, for an hour's token its rules bound (OAuth client
    /// credentials). The operator's, or, signed in, a person's own acting as them.
    #[command(subcommand)]
    Client(MachineCmd),
    /// Enterprise sign-in (SEP-990, ID-JAG): the identity providers roll trusts to vouch for
    /// their people, and the apps they may sign people in to. Nothing to show them.
    ///
    /// Examples:
    ///   roll token enterprise trust https://acme.okta.com --name Okta --domain acme.com
    ///   roll token enterprise app create Claude --product code,workspace --allow read,write
    Enterprise {
        #[command(subcommand)]
        what: Option<EnterpriseCmd>,
    },
    /// roll's rules for every token: `--max-days 90`, `--require-expiry`, `--no-limit`,
    /// `--max-idle-days 60` (`0` for no idle limit), `--approval-rule secrets:reveal@production=2`
    /// (what needs several approvers); nothing to show them.
    Policy {
        #[arg(long)]
        max_days: Option<u32>,
        #[arg(long)]
        require_expiry: Option<bool>,
        #[arg(long, conflicts_with = "max_days")]
        no_limit: bool,
        /// Tokens unused this many days stop working (0: no limit).
        #[arg(long)]
        max_idle_days: Option<u32>,
        /// Add a rule asking several people to approve: `product[:action][@env,…]=N[/approver,…]`,
        /// e.g. `secrets:reveal@production=2` or `postgres:delete=2/usr_a,usr_b`. Repeatable.
        #[arg(long = "approval-rule", value_parser = approval_rule)]
        approval_rules: Vec<Value>,
        /// Remove every approval rule (one person decides each request again).
        #[arg(long, conflicts_with = "approval_rules")]
        no_approval_rules: bool,
    },
}

/// `--approver` values: chat ids, and the roll accounts of those given as `U…=usr_…`.
fn chat_approvers(given: &[String]) -> (Vec<String>, std::collections::BTreeMap<String, String>) {
    let mut ids = Vec::new();
    let mut people = std::collections::BTreeMap::new();
    for a in given {
        let (id, user) = a.split_once('=').map_or((a.as_str(), None), |(i, u)| (i, Some(u)));
        let id = id.trim().trim_start_matches('@').to_string();
        if let Some(u) = user {
            people.insert(id.clone(), u.trim().to_string());
        }
        ids.push(id);
    }
    (ids, people)
}

/// `product[:action][@env,…]=N[/approver,…]` as a policy approval rule.
fn approval_rule(s: &str) -> Result<Value, String> {
    let bad = || format!("{s:?}: product[:action][@env,…]=N[/approver,…], e.g. secrets:reveal@production=2");
    let (what, rest) = s.split_once('=').ok_or_else(bad)?;
    let (n, approvers) = rest.split_once('/').map_or((rest, None), |(n, a)| (n, Some(a)));
    let approvals: u8 = n.trim().parse().map_err(|_| bad())?;
    let (what, envs) = what.split_once('@').map_or((what, None), |(w, e)| (w, Some(e)));
    let (product, action) = what.split_once(':').map_or((what, None), |(p, a)| (p, Some(a)));
    if product.trim().is_empty() {
        return Err(bad());
    }
    let list = |l: &str| l.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>();
    let mut rule = json!({ "product": product.trim(), "approvals": approvals });
    if let Some(a) = action {
        rule["actions"] = json!(list(a));
    }
    if let Some(e) = envs {
        rule["environments"] = json!(list(e));
    }
    if let Some(a) = approvers {
        rule["approvers"] = json!(list(a));
    }
    Ok(rule)
}

#[derive(Args)]
pub struct Create {
    /// A name you'll recognise it by ("ci", "cursor on my laptop").
    name: String,
    #[command(flatten)]
    rules: RuleArgs,
    /// Lifetime: 30d, 12h, 90m, or never (default 30d); from its start with --starts.
    #[arg(long, default_value = "30d")]
    expires: String,
    /// It works only from this far on (2h, 1d): refused until then.
    #[arg(long)]
    starts: Option<String>,
    /// Addresses it may be used from (CIDRs), repeatable.
    #[arg(long = "ip", value_delimiter = ',')]
    ip_allow: Vec<String>,
    /// Requests a minute it may make (per node; over it, 429 with Retry-After).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100_000))]
    rate: Option<u32>,
    /// What the rules don't allow asks you instead of being refused.
    #[arg(long)]
    ask_otherwise: bool,
}

#[derive(Subcommand)]
pub enum MachineCmd {
    /// Make a machine client with rules, as a token has. With --public-key it proves itself with
    /// JWTs its private key signs (nothing secret is kept); without, roll makes a secret, shown once.
    ///
    /// Examples:
    ///   roll token client create nightly --preset read-only --public-key ci.pub.pem
    ///   roll token client create deploy --product apps --allow read,deploy --expires 90d
    Create(Box<ClientCreate>),
    /// List machine clients, newest first.
    #[command(alias = "ls")]
    List,
    /// Revoke a machine client: its tokens stop working at once, and it gets no more.
    Revoke { id: String },
    /// A new credential for a machine client, keeping its id and rules: a new key with
    /// --public-key, CI's own identity with --github, --gitlab or --issuer, else a new secret
    /// (shown once). The old one stops now, with the tokens it
    /// got, unless --keep-old keeps it working while the automation moves over.
    ///
    /// Examples:
    ///   roll token client rotate mcl_… --keep-old 1h
    ///   roll token client rotate mcl_… --public-key ci-2026.pub.pem
    ///   roll token client rotate mcl_… --github acme/web --ref refs/heads/main   (CI's own identity)
    Rotate(Box<ClientRotate>),
}

#[derive(Subcommand)]
pub enum EnterpriseCmd {
    /// Trust an identity provider (its issuer, an https URL); again to change how it's set up.
    Trust {
        issuer: String,
        /// What people call it ("Okta").
        #[arg(long)]
        name: Option<String>,
        /// Its keys pinned (a JWKS file), for a provider roll can't reach.
        #[arg(long)]
        jwks: Option<std::path::PathBuf>,
        /// How a person is found: by their verified email (default), or by `aud_sub` naming
        /// their roll account.
        #[arg(long = "match", value_parser = ["email", "aud_sub"], default_value = "email")]
        matching: String,
        /// An email domain it vouches for (acme.com); repeat for more. Email matching needs one.
        #[arg(long = "domain")]
        domains: Vec<String>,
        /// Let it sign in roll's admins too (they act as the operator).
        #[arg(long)]
        admins: bool,
        /// A multi-tenant issuer's one tenant roll trusts (its assertions' `tenant` claim).
        #[arg(long)]
        tenant: Option<String>,
        /// The SSO connection that is this same provider (`sso_…`, on the account page's single sign-on): it vouches
        /// for the domains a DNS record proved for that connection, instead of `--domain`.
        #[arg(long, conflicts_with = "domains")]
        sso: Option<String>,
    },
    /// Stop trusting an identity provider: no new sign-ins from it.
    Untrust { issuer: String },
    /// The apps an identity provider may sign people in to.
    #[command(subcommand)]
    App(EnterpriseAppCmd),
}

#[derive(Subcommand)]
pub enum EnterpriseAppCmd {
    /// Register an app (its client id is what the provider puts in an assertion): with
    /// --public-key it signs its client assertions; without, roll makes a secret, shown once.
    Create {
        name: String,
        #[command(flatten)]
        rules: RuleArgs,
        /// Its public key: a PEM file (Ed25519 or P-256) or a JWK.
        #[arg(long)]
        public_key: Option<std::path::PathBuf>,
    },
    /// Change an app's name or rules: its people's connections take the new rules at once.
    Update {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        rules: RuleArgs,
    },
    /// A new credential for an app, keeping its client id and its people's connections: with
    /// --public-key a key, without a new secret (shown once). The old secret stops now with the
    /// tokens it got, unless --keep-old keeps it working while the app moves over. A leaked app
    /// signs in again from here.
    Rotate {
        id: String,
        /// Its new public key: a PEM file (Ed25519 or P-256) or a JWK.
        #[arg(long)]
        public_key: Option<std::path::PathBuf>,
        /// The old secret keeps working this long (1h, 2d; at most 7d).
        #[arg(long)]
        keep_old: Option<String>,
    },
    /// Remove an app: every person's connection to it is revoked.
    Remove { id: String },
}

/// CI's own identity as a machine client's credential (`create` and `rotate`): GitHub Actions,
/// GitLab CI, or any issuer.
#[derive(Args)]
pub struct IdentityArgs {
    /// Trust this repository's GitHub Actions instead: its jobs sign in with their own OIDC token
    /// (`permissions: id-token: write`), so nothing is stored in GitHub (owner/repo).
    #[arg(long, conflicts_with_all = ["gitlab", "issuer"])]
    github: Option<String>,
    /// Trust this GitLab.com project's CI jobs instead (`id_tokens`), so nothing is stored in
    /// GitLab (group/project, subgroups too).
    #[arg(long, conflicts_with = "issuer")]
    gitlab: Option<String>,
    /// Trust any CI system's OIDC tokens instead (Buildkite, CircleCI, Terraform Cloud, a
    /// cluster's own): its issuer URL, the token's `iss`. With --subject.
    #[arg(long, requires = "subject")]
    issuer: Option<String>,
    /// With --issuer: the token's `sub`, or a pattern of it with `*` (a shared issuer's must name
    /// one tenant: Buildkite's `organization:acme:…`).
    #[arg(long, requires = "issuer")]
    subject: Option<String>,
    /// With --issuer: another claim the token must carry, `name=value` (`*` patterns too);
    /// repeatable.
    #[arg(long = "claim", requires = "issuer")]
    claims: Vec<String>,
    /// With --issuer: the audience its tokens are minted for (default roll's API URL).
    #[arg(long, requires = "issuer")]
    audience: Option<String>,
    /// With --issuer: the issuer's keys, pinned, for one roll can't reach (a JWKS file:
    /// `kubectl get --raw /openid/v1/jwks`).
    #[arg(long, requires = "issuer")]
    jwks: Option<std::path::PathBuf>,
    /// With --github or --gitlab: only jobs on this ref (GitHub: refs/heads/main; GitLab: the
    /// branch, main).
    #[arg(long, conflicts_with = "environment")]
    r#ref: Option<String>,
    /// With --github or --gitlab: only jobs in this environment (production).
    #[arg(long)]
    environment: Option<String>,
}

impl IdentityArgs {
    /// The identity these flags describe, if any.
    fn identity(&self) -> anyhow::Result<Option<Value>> {
        if (self.r#ref.is_some() || self.environment.is_some()) && self.github.is_none() && self.gitlab.is_none() {
            bail!("--ref and --environment narrow --github or --gitlab");
        }
        Ok(match (&self.github, &self.gitlab, &self.issuer) {
            (Some(repo), ..) => Some(github_identity(repo, self.r#ref.as_deref(), self.environment.as_deref())?),
            (None, Some(project), _) => Some(gitlab_identity(project, self.r#ref.as_deref(), self.environment.as_deref())?),
            (None, None, Some(issuer)) => {
                let mut claims = serde_json::Map::new();
                for kv in &self.claims {
                    let (k, v) = kv.split_once('=').filter(|(k, v)| !k.is_empty() && !v.is_empty()).with_context(|| format!("--claim {kv}: name=value"))?;
                    claims.insert(k.to_string(), json!(v));
                }
                let jwks = match &self.jwks {
                    Some(path) => Some(std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?),
                    None => None,
                };
                Some(json!({ "issuer": issuer, "subject": self.subject, "claims": claims, "audience": self.audience, "jwks": jwks }))
            }
            _ => None,
        })
    }
}

#[derive(Args)]
pub struct ClientRotate {
    id: String,
    /// Its new public key: a PEM file (Ed25519 or P-256) or a JWK.
    #[arg(long, conflicts_with_all = ["github", "gitlab", "issuer"])]
    public_key: Option<std::path::PathBuf>,
    /// Or trust CI's own identity from now on (nothing stored in CI).
    #[command(flatten)]
    identity: IdentityArgs,
    /// The old credential keeps working this long (1h, 2d; at most 7d).
    #[arg(long)]
    keep_old: Option<String>,
}

#[derive(Args)]
pub struct ClientCreate {
    /// A name you'll recognise it by ("nightly report", "deploy from CI").
    name: String,
    #[command(flatten)]
    rules: RuleArgs,
    /// Lifetime: 30d, 12h, 90m, or never (default 30d).
    #[arg(long, default_value = "30d")]
    expires: String,
    /// Its public key: a PEM file (Ed25519 or P-256) or a JWK (Ed25519, P-256 or RSA).
    #[arg(long, conflicts_with_all = ["github", "gitlab", "issuer"])]
    public_key: Option<std::path::PathBuf>,
    #[command(flatten)]
    identity: IdentityArgs,
    /// Addresses its tokens may be used from (CIDRs), repeatable.
    #[arg(long = "ip", value_delimiter = ',')]
    ip_allow: Vec<String>,
    /// Requests a minute its tokens may make (per node).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100_000))]
    rate: Option<u32>,
}

/// A public key file as a JWK: a JWK as is, or a PEM `PUBLIC KEY` (SubjectPublicKeyInfo) for
/// Ed25519 or P-256, whose encodings are fixed: a known prefix, then the key's bytes.
fn public_jwk(text: &str) -> anyhow::Result<Value> {
    use base64::Engine;
    let text = text.trim();
    if text.starts_with('{') {
        return serde_json::from_str(text).context("a JWK is a JSON object");
    }
    if text.contains("PRIVATE KEY") {
        bail!("that's a private key: give roll only the public one (openssl pkey -in key.pem -pubout)");
    }
    let body: String = text.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(body.trim()).context("not a PEM public key")?;
    let url = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    const ED25519: &[u8] = &[0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    const P256: &[u8] = &[0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04];
    if let Some(x) = der.strip_prefix(ED25519).filter(|x| x.len() == 32) {
        return Ok(json!({ "kty": "OKP", "crv": "Ed25519", "x": url(x) }));
    }
    if let Some(xy) = der.strip_prefix(P256).filter(|xy| xy.len() == 64) {
        return Ok(json!({ "kty": "EC", "crv": "P-256", "x": url(&xy[..32]), "y": url(&xy[32..]) }));
    }
    bail!("an Ed25519 or P-256 PEM public key (for RSA, give its JWK)")
}

/// GitHub Actions as a trusted identity: the repository's jobs (on a ref, or in an environment).
fn github_identity(repo: &str, git_ref: Option<&str>, environment: Option<&str>) -> anyhow::Result<Value> {
    let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    let Some((owner, name)) = repo.split_once('/').filter(|(o, n)| ok(o) && ok(n)) else {
        bail!("--github is owner/repo");
    };
    // GitHub's `sub`: repo:<owner>/<repo>:ref:<ref>, or :environment:<name> for an environment's jobs.
    let subject = match (git_ref, environment) {
        (_, Some(env)) => format!("repo:{owner}/{name}:environment:{env}"),
        (Some(r), None) => format!("repo:{owner}/{name}:ref:{r}"),
        (None, None) => format!("repo:{owner}/{name}:*"),
    };
    Ok(json!({ "issuer": "https://token.actions.githubusercontent.com", "subject": subject }))
}

/// GitLab.com's CI as a trusted identity: a project's jobs (on a branch, or in an environment).
fn gitlab_identity(project: &str, branch: Option<&str>, environment: Option<&str>) -> anyhow::Result<Value> {
    let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !project.contains('/') || !project.split('/').all(ok) {
        bail!("--gitlab is group/project (subgroups too)");
    }
    // GitLab's `sub`: project_path:<group>/<project>:ref_type:branch:ref:<branch>; an environment
    // is its own claim.
    let subject = match branch {
        Some(b) => format!("project_path:{project}:ref_type:branch:ref:{b}"),
        None => format!("project_path:{project}:*"),
    };
    let mut identity = json!({ "issuer": "https://gitlab.com", "subject": subject });
    if let Some(env) = environment {
        identity["claims"] = json!({ "environment": env });
    }
    Ok(identity)
}

/// The `.gitlab-ci.yml` job lines that trade the job's ID token for a roll token.
fn gitlab_step(client_id: &str, endpoint: &str, audience: &str) -> String {
    format!(
        "  id_tokens:\n\
         \x20   ROLL_OIDC:\n\
         \x20     aud: {audience}\n\
         \x20 before_script:\n\
         \x20   - export ROLL_TOKEN=$(curl -sSf {endpoint} -d grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer -d client_id={client_id} --data-urlencode \"assertion=$ROLL_OIDC\" | jq -r .access_token)"
    )
}

/// The workflow step that trades a job's OIDC token for a roll token, masked, as `ROLL_TOKEN`.
fn github_step(client_id: &str, endpoint: &str, audience: &str) -> String {
    format!(
        "      - name: Sign in to roll\n\
         \x20       run: |\n\
         \x20         oidc=$(curl -sSf -H \"Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN\" \"$ACTIONS_ID_TOKEN_REQUEST_URL&audience={audience}\" | jq -r .value)\n\
         \x20         token=$(curl -sSf {endpoint} -d grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer -d client_id={client_id} --data-urlencode \"assertion=$oidc\" | jq -r .access_token)\n\
         \x20         echo \"::add-mask::$token\"\n\
         \x20         echo \"ROLL_TOKEN=$token\" >> \"$GITHUB_ENV\""
    )
}

async fn machine(api: &Api, json_out: bool, cmd: MachineCmd) -> anyhow::Result<()> {
    match cmd {
        MachineCmd::Create(c) => {
            let jwk = match &c.public_key {
                Some(path) => Some(public_jwk(&std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)?),
                None => None,
            };
            let oidc = c.identity.identity()?;
            let body = json!({ "name": c.name, "rules": rules(api, &c.rules).await?, "expires_at": expiry(&c.expires)?, "ip_allow": c.ip_allow, "rate_per_minute": c.rate, "public_jwk": jwk, "oidc": oidc });
            let made = api.post("/v1/access/clients", body).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&made)?);
                return Ok(());
            }
            print_grant(&made["grant"]);
            println!("client_id:      {}\ntoken endpoint: {}", s(&made["client_id"]), s(&made["token_endpoint"]));
            let endpoint = s(&made["token_endpoint"]);
            let audience = endpoint.trim_end_matches("/v1/access/oauth/token");
            if c.identity.github.is_some() {
                eprintln!("Nothing to store in GitHub. In the workflow, give the job `permissions: id-token: write`, then:\n");
                println!("{}", github_step(&s(&made["client_id"]), &endpoint, audience));
            } else if c.identity.gitlab.is_some() {
                eprintln!("Nothing to store in GitLab. In .gitlab-ci.yml, add to the job:\n");
                println!("{}", gitlab_step(&s(&made["client_id"]), &endpoint, audience));
            } else if c.identity.issuer.is_some() {
                println!("client_id:      {}\ntoken endpoint: {}", s(&made["client_id"]), endpoint);
                eprintln!(
                    "Nothing to store in CI. Get the job's OIDC token for audience {}, then trade it:\n  curl -sSf {endpoint} -d grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer -d client_id={} --data-urlencode \"assertion=$OIDC_TOKEN\"",
                    c.identity.audience.as_deref().unwrap_or(audience),
                    s(&made["client_id"])
                );
            } else if let Some(secret) = made["client_secret"].as_str() {
                println!("\n{secret}\n");
                eprintln!("Copy the secret now: it isn't shown again. Trade it for a token with grant_type=client_credentials (client_secret_basic or client_secret_post).");
            } else {
                eprintln!("Sign a JWT with its private key (iss and sub the client_id, aud the token endpoint, a new jti, at most 5 minutes to live) and send it as client_assertion with grant_type=client_credentials.");
            }
        }
        MachineCmd::List => {
            let list = api.get("/v1/access/clients").await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            let list = list.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("no machine clients yet: roll token client create <name> --preset read-only --public-key key.pub.pem");
            }
            for c in &list {
                let how = match &c["trusts"] {
                    t if t.is_object() => format!("trusts {}: {}", s(&t["issuer"]).trim_start_matches("https://"), s(&t["subject"])),
                    _ if c["signs_with_key"] == true => "signs with a key".to_string(),
                    _ => "signs with a secret".to_string(),
                };
                let last = c["last_sign_in"].as_i64().map(|t| format!("last sign-in {}", ago(t / 1000))).unwrap_or_else(|| "never signed in".into());
                println!("{}  {how}  ({last})", s(&c["client_id"]));
                print_grant(&c["grant"]);
            }
        }
        MachineCmd::Rotate(r) => {
            let ClientRotate { id, public_key, identity, keep_old } = *r;
            let oidc = identity.identity()?;
            let jwk = match &public_key {
                Some(path) => Some(public_jwk(&std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)?),
                None => None,
            };
            let keep = keep_old.as_deref().map(|k| span(k, "--keep-old")).transpose()?.flatten().map(|ms| ms / 1000);
            let made = api.post(&format!("/v1/access/clients/{}/rotate", urlencoding::encode(&id)), json!({ "public_jwk": jwk, "oidc": oidc, "keep_old_for_s": keep })).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&made)?);
                return Ok(());
            }
            let old = if keep.is_some_and(|s| s > 0) { "The old one keeps working for now." } else { "The old one stopped working, with the tokens it got." };
            match made["client_secret"].as_str() {
                Some(secret) => {
                    println!("{secret}");
                    eprintln!("{old} Copy the new secret now: it isn't shown again.");
                }
                None if oidc.is_some() => eprintln!("{id} trusts CI's own identity now: nothing to store in CI. {old}"),
                None => eprintln!("{id} signs with the new key now. {old}"),
            }
        }
        MachineCmd::Revoke { id } => {
            let r = api.call(reqwest::Method::DELETE, &format!("/v1/access/clients/{}", urlencoding::encode(&id)), None).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("revoked {id}: its tokens stopped working");
            }
        }
    }
    Ok(())
}

/// What a token may do, as flags (`create` and `edit`).
#[derive(Args)]
pub struct RuleArgs {
    /// Start from a preset: read-only, agent, project, development, previews, full.
    #[arg(long)]
    preset: Option<String>,
    /// Products a rule of your own covers (default: every product).
    #[arg(long, value_delimiter = ',')]
    product: Vec<String>,
    /// Workspace projects it covers (with --preset project/development, the project it's for).
    #[arg(long, value_delimiter = ',')]
    project: Vec<String>,
    /// Resources it covers, by their product's id.
    #[arg(long, value_delimiter = ',')]
    resource: Vec<String>,
    /// Only resources in these environments.
    #[arg(long = "env", value_delimiter = ',')]
    environments: Vec<String>,
    /// Actions allowed: read, write, delete, reveal, deploy, admin.
    #[arg(long, value_delimiter = ',')]
    allow: Vec<String>,
    /// Actions that wait for your approval each time.
    #[arg(long, value_delimiter = ',')]
    ask: Vec<String>,
}

impl RuleArgs {
    fn given(&self) -> bool {
        self.preset.is_some() || !self.allow.is_empty() || !self.ask.is_empty()
    }
}

#[derive(Args)]
pub struct Edit {
    id: String,
    /// A new name.
    #[arg(long)]
    name: Option<String>,
    /// New rules, built as `create` builds them (replaces every rule).
    #[command(flatten)]
    rules: RuleArgs,
    /// A new lifetime from now: 30d, 12h, 90m, or never.
    #[arg(long)]
    expires: Option<String>,
    /// New addresses it may be used from (replaces the list; `--ip none` clears it).
    #[arg(long = "ip", value_delimiter = ',')]
    ip_allow: Option<Vec<String>>,
    /// Requests a minute it may make (`none` takes the limit off).
    #[arg(long)]
    rate: Option<String>,
    /// Whether what the rules don't allow asks you instead of being refused.
    #[arg(long)]
    ask_otherwise: Option<bool>,
}

#[derive(Subcommand)]
pub enum SinkCmd {
    /// Stream new audit rows to an https URL (signed batches; the secret is shown once).
    Add {
        url: String,
        /// Send every row the log still has, not just new ones.
        #[arg(long)]
        from_start: bool,
    },
    /// Send a failed sink's batch now rather than after its wait.
    Retry { id: String },
    /// Stop streaming to a sink.
    Rm { id: String },
}

#[derive(Subcommand)]
pub enum Mcp {
    /// The config that connects an MCP client to roll's server (OAuth by default).
    Config {
        #[arg(value_enum, default_value = "claude-code")]
        client: Client,
        /// Use this scoped token instead of OAuth.
        #[arg(long)]
        with_token: Option<String>,
        /// Only these products' tools (comma separated, e.g. postgres,code), or an area of one
        /// (apps.observe, secrets.security: `GET /v1/mcp/toolsets` lists them).
        #[arg(long, value_delimiter = ',')]
        toolsets: Vec<String>,
        /// Only tools that read are listed and run, whatever the token allows.
        #[arg(long)]
        readonly: bool,
        /// What people who can't push to a repo wrote never reaches the agent.
        #[arg(long)]
        lockdown: bool,
        /// A search tool and two run tools instead of every tool (the default for clients with a
        /// tool limit: VS Code, Cursor).
        #[arg(long, conflicts_with = "all_tools")]
        search: bool,
        /// Every tool, even for a client with a tool limit.
        #[arg(long)]
        all_tools: bool,
    },
    /// Requests waiting for your approval (--all: decided ones too).
    Approvals {
        #[arg(long)]
        all: bool,
    },
    /// Approve a request: it runs.
    Approve {
        id: String,
        /// Also allow the same action on the same resource without asking: 1h, 8h, 1d, 7d, 30d or
        /// always.
        #[arg(long, value_parser = ["1h", "8h", "1d", "7d", "30d", "always"])]
        remember: Option<String>,
    },
    /// Deny a request.
    Deny {
        id: String,
        /// Told to the agent.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Where approvals are also posted: a Slack or Discord channel through roll code's chat ops
    /// (`--install chi_… --channel C… --approver U…`), `--off`, or nothing to show it.
    Notify {
        #[arg(long, requires_all = ["channel", "approver"])]
        install: Option<String>,
        #[arg(long, requires = "install")]
        channel: Option<String>,
        /// A chat user id (Slack `U…`, a Discord user id) who may decide there; repeat it for more.
        /// `U…=usr_…` links them to their roll account: they decide as that person (they count
        /// toward the policy's approval rules, never for their own token's request).
        #[arg(long, requires = "install")]
        approver: Vec<String>,
        #[arg(long, conflicts_with_all = ["install", "channel"])]
        off: bool,
    },
    /// Where the audit log streams to (a SIEM): list them, or add, retry or remove one.
    Sinks {
        #[command(subcommand)]
        what: Option<SinkCmd>,
    },
    /// Every request made with a token and every MCP tool call, newest first.
    Audit {
        /// csv or jsonl: the matching events as that, for a spreadsheet or a log tool.
        #[arg(long)]
        format: Option<String>,
        /// Only this token (tok_…).
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        product: Option<String>,
        /// allow, deny or ask.
        #[arg(long)]
        decision: Option<String>,
        /// Who touched this resource (or anything under it: a cluster's indices).
        #[arg(long)]
        resource: Option<String>,
        /// read, write, delete, reveal, deploy or admin.
        #[arg(long)]
        action: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Client {
    ClaudeCode,
    Cursor,
    Vscode,
    ClaudeDesktop,
    Codex,
    Json,
}

/// "30d" → milliseconds from now; "never" → none.
fn expiry(s: &str) -> anyhow::Result<Option<i64>> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    Ok(span(s, "--expires")?.map(|ms| now + ms))
}

/// `--starts` and `--expires` as times: the expiry counts from the start.
fn window(starts: Option<&str>, expires: &str) -> anyhow::Result<(Option<i64>, Option<i64>)> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    let from = match starts {
        Some(s) => Some(now + span(s, "--starts")?.with_context(|| format!("--starts {s}: a number and d, h or m (2h)"))?),
        None => None,
    };
    Ok((from, span(expires, "--expires")?.map(|ms| from.unwrap_or(now) + ms)))
}

/// "30d" → its milliseconds; "never" → none.
fn span(s: &str, flag: &str) -> anyhow::Result<Option<i64>> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("never") {
        return Ok(None);
    }
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = n.parse().with_context(|| format!("{flag} {s}: a number and d, h or m (30d), or never"))?;
    Ok(Some(match unit {
        "d" => n * 86_400_000,
        "h" => n * 3_600_000,
        "m" => n * 60_000,
        _ => bail!("{flag} {s}: a number and d, h or m (30d), or never"),
    }))
}

/// The rules a `create` asks for: the preset's (its "project" filled in), then a rule of its own.
async fn enterprise(api: &Api, json_out: bool, cmd: Option<EnterpriseCmd>) -> anyhow::Result<()> {
    let current = api.get("/v1/access/enterprise").await?;
    let idps = |v: &Value| v["idps"].as_array().cloned().unwrap_or_default();
    let other = |list: Vec<Value>, issuer: &str| -> Vec<Value> { list.into_iter().filter(|i| i["issuer"].as_str().map(|s| s.trim_end_matches('/')) != Some(issuer.trim_end_matches('/'))).collect() };
    let out = match cmd {
        None => current,
        Some(EnterpriseCmd::Trust { issuer, name, jwks, matching, domains, admins, tenant, sso }) => {
            // Again for the same issuer and tenant changes it; another tenant is another entry.
            let mut list: Vec<Value> = idps(&current)
                .into_iter()
                .filter(|i| i["issuer"].as_str().map(|s| s.trim_end_matches('/')) != Some(issuer.trim_end_matches('/')) || i["tenant"].as_str() != tenant.as_deref())
                .collect();
            let mut idp = json!({ "issuer": issuer, "matching": matching, "domains": domains, "admins": admins });
            if let Some(t) = tenant {
                idp["tenant"] = json!(t);
            }
            if let Some(s) = sso {
                idp["sso"] = json!(s);
            }
            if let Some(n) = name {
                idp["name"] = json!(n);
            }
            if let Some(path) = jwks {
                idp["jwks"] = json!(std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?);
            }
            list.push(idp);
            api.call(reqwest::Method::PUT, "/v1/access/enterprise/idps", Some(json!({ "idps": list }))).await?;
            api.get("/v1/access/enterprise").await?
        }
        Some(EnterpriseCmd::Untrust { issuer }) => {
            let list = other(idps(&current), &issuer);
            if list.len() == idps(&current).len() {
                anyhow::bail!("roll doesn't trust {issuer}");
            }
            api.call(reqwest::Method::PUT, "/v1/access/enterprise/idps", Some(json!({ "idps": list }))).await?;
            api.get("/v1/access/enterprise").await?
        }
        Some(EnterpriseCmd::App(EnterpriseAppCmd::Create { name, rules: r, public_key })) => {
            let jwk = match &public_key {
                Some(path) => Some(public_jwk(&std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)?),
                None => None,
            };
            let made = api.post("/v1/access/enterprise/apps", json!({ "name": name, "rules": rules(api, &r).await?, "public_jwk": jwk })).await?;
            if !json_out {
                println!("client_id: {}", s(&made["client_id"]));
                if let Some(secret) = made["client_secret"].as_str() {
                    println!("client_secret (shown once; it's the app's, never the identity provider's): {secret}");
                }
                println!("Give the identity provider this client id for the app: it puts it in each person's assertion.");
                return Ok(());
            }
            made
        }
        Some(EnterpriseCmd::App(EnterpriseAppCmd::Update { id, name, rules: r })) => {
            if name.is_none() && !r.given() {
                bail!("nothing to change: --name, or the app's new rules (--preset, --product, --allow…)");
            }
            let mut body = json!({});
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            if r.given() {
                body["rules"] = rules(api, &r).await?;
            }
            let app = api.call(reqwest::Method::PATCH, &format!("/v1/access/enterprise/apps/{}", urlencoding::encode(&id)), Some(body)).await?;
            if !json_out {
                println!("{} changed: its people's connections have its rules now", s(&app["name"]));
                return Ok(());
            }
            app
        }
        Some(EnterpriseCmd::App(EnterpriseAppCmd::Rotate { id, public_key, keep_old })) => {
            let jwk = match &public_key {
                Some(path) => Some(public_jwk(&std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)?),
                None => None,
            };
            let keep = keep_old.as_deref().map(|k| span(k, "--keep-old")).transpose()?.flatten().map(|ms| ms / 1000);
            let app = api.post(&format!("/v1/access/enterprise/apps/{}/rotate", urlencoding::encode(&id)), json!({ "public_jwk": jwk, "keep_old_for_s": keep })).await?;
            if !json_out {
                // The secret alone on stdout (for a script to hand to the provider); words on stderr.
                let old = if app["old_until"].is_i64() { "The old secret keeps working a while (old_until in --json)." } else { "The old secret and the tokens it got stopped." };
                match app["client_secret"].as_str() {
                    Some(secret) => {
                        println!("{secret}");
                        eprintln!("{old} Copy the new secret now: it isn't shown again.");
                    }
                    None => eprintln!("{} signs with its new key now. {old}", s(&app["name"])),
                }
                return Ok(());
            }
            app
        }
        Some(EnterpriseCmd::App(EnterpriseAppCmd::Remove { id })) => api.call(reqwest::Method::DELETE, &format!("/v1/access/enterprise/apps/{}", urlencoding::encode(&id)), None).await?,
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if out.get("revoked").is_some() {
        println!("removed; {} people's connections revoked", out["revoked"]);
    } else {
        for i in idps(&out) {
            let domains: Vec<&str> = i["domains"].as_array().into_iter().flatten().filter_map(|d| d.as_str()).collect();
            println!("trusts {} ({}; people by {}{}{})", s(&i["issuer"]), i["name"].as_str().unwrap_or("-"), s(&i["matching"]), if domains.is_empty() { String::new() } else { format!(" at {}", domains.join(", ")) }, if i["admins"] == true { "; admins too" } else { "" });
        }
        for app in out["apps"].as_array().into_iter().flatten() {
            let leaked = app["leaked"].as_str().map(|w| format!("  its secret leaked ({w}): rotate it")).unwrap_or_default();
            println!("app {}  {}  {}{leaked}", s(&app["client_id"]), s(&app["name"]), if app["signs_with_key"] == true { "signs with a key" } else { "a secret" });
        }
        if idps(&out).is_empty() && out["apps"].as_array().is_none_or(|a| a.is_empty()) {
            println!("no identity provider is trusted, and no app registered");
        }
    }
    Ok(())
}

async fn rules(api: &Api, c: &RuleArgs) -> anyhow::Result<Value> {
    let mut rules = Vec::new();
    let selectors = || {
        let mut sel: Vec<Value> = c.project.iter().map(|p| json!({ "project": p })).collect();
        sel.extend(c.resource.iter().map(|r| json!({ "id": r })));
        if sel.is_empty() { json!("*") } else { Value::Array(sel) }
    };
    if let Some(name) = &c.preset {
        let catalog = api.get("/v1/access/catalog").await?;
        let preset = catalog["presets"].as_array().and_then(|p| p.iter().find(|p| p["id"] == name.as_str())).cloned();
        let Some(preset) = preset else {
            let names: Vec<_> = catalog["presets"].as_array().into_iter().flatten().filter_map(|p| p["id"].as_str()).collect();
            bail!("no preset {name}: one of {}", names.join(", "));
        };
        for mut r in preset["rules"].as_array().cloned().unwrap_or_default() {
            if r["resources"] == "project" {
                r["resources"] = selectors();
            }
            if !c.environments.is_empty() {
                r["environments"] = json!(c.environments);
            }
            rules.push(r);
        }
    }
    if !c.allow.is_empty() || !c.ask.is_empty() {
        let mut actions = serde_json::Map::new();
        for a in &c.allow {
            actions.insert(a.trim().to_string(), json!("allow"));
        }
        for a in &c.ask {
            actions.insert(a.trim().to_string(), json!("ask"));
        }
        let products = if c.product.is_empty() { vec!["*".to_string()] } else { c.product.clone() };
        for p in products {
            let mut r = json!({ "product": p, "resources": selectors(), "actions": actions });
            if !c.environments.is_empty() {
                r["environments"] = json!(c.environments);
            }
            rules.push(r);
        }
    }
    if rules.is_empty() {
        bail!("say what the token may do: --preset read-only (or agent, project, …), or --allow read,write");
    }
    Ok(Value::Array(rules))
}

fn describe_rule(r: &Value) -> String {
    let resources = match &r["resources"] {
        Value::String(s) if s == "*" => "every resource".to_string(),
        Value::Array(a) => a
            .iter()
            .map(|x| match (x.get("project"), x.get("id"), x.get("index")) {
                (Some(p), ..) => format!("project {}", s(p)),
                (None, Some(id), Some(ix)) => format!("{} indices {}", s(id), s(ix)),
                (None, id, _) => id.map(s).unwrap_or_default(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => s(other),
    };
    let actions: Vec<String> = r["actions"].as_object().into_iter().flatten().map(|(a, e)| if e == "ask" { format!("{a} (ask)") } else { a.clone() }).collect();
    let env = r["environments"].as_array().map(|e| format!(" in {}", e.iter().map(s).collect::<Vec<_>>().join(", "))).unwrap_or_default();
    let product = if r["product"] == "*" { "every product".to_string() } else { s(&r["product"]) };
    format!("{product}: {} on {resources}{env}", actions.join(", "))
}

fn when(ms: &Value) -> String {
    match ms.as_i64() {
        Some(t) => ago(t / 1000),
        None => "never".into(),
    }
}

fn print_grant(g: &Value) {
    let state = if !g["revoked_at"].is_null() {
        "revoked"
    } else if g["pending"] == true {
        "waits for an admin"
    } else {
        "active"
    };
    let hint = g["hint"].as_str().map(|h| format!("  {h}")).unwrap_or_default();
    println!("{}{hint}  {}  ({}, {state})", s(&g["id"]), s(&g["name"]), s(&g["kind"]));
    if let Some(c) = g["client"].as_object() {
        println!("  client: {}", s(&c["name"]).as_str());
    }
    for r in g["rules"].as_array().into_iter().flatten() {
        println!("  {}", describe_rule(r));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    for t in g["temporary"].as_array().into_iter().flatten().filter(|t| t["until"].as_i64().is_some_and(|u| u > now)) {
        let by = t["approval"].as_str().map(|a| format!(", approved in {a}")).unwrap_or_default();
        println!("  for a while: {} until {}{by}", describe_rule(&t["rule"]), crate::access::date(t["until"].as_i64().unwrap_or(0)));
    }
    let expires = match g["expires_at"].as_i64() {
        Some(t) => format!("expires {}", crate::access::date(t)),
        None => "never expires".into(),
    };
    let ip = g["ip_allow"].as_array().filter(|a| !a.is_empty()).map(|a| format!(", from {}", a.iter().map(s).collect::<Vec<_>>().join(", "))).unwrap_or_default();
    let rate = g["rate_per_minute"].as_u64().map(|r| format!(", {r} requests a minute")).unwrap_or_default();
    println!("  {expires}{ip}{rate}; last used {}", when(&g["last_used_at"]));
}

/// A unix-ms time as a UTC date.
pub fn date(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// An admin's decision on a person's pending token.
async fn decide(api: &Api, json_out: bool, id: &str, approve: bool) -> anyhow::Result<()> {
    let verb = if approve { "approve" } else { "deny" };
    let g = api.post(&format!("/v1/access/tokens/{id}/{verb}"), json!({})).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&g)?);
    } else {
        println!("{} {}: {}", s(&g["id"]), s(&g["name"]), if approve { "approved, it works now" } else { "denied and revoked" });
    }
    Ok(())
}

/// Which credential a run uses, and the ones set that lose to it (`gh auth status`'s answer).
pub struct Credential {
    /// What it is and where it came from: `ROLL_TOKEN`, `roll login (~/.config/roll/logins.json)`, …
    pub source: Option<String>,
    /// Credentials also set, which this run doesn't use.
    pub unused: Vec<String>,
}

pub async fn token(api: &Api, json_out: bool, cmd: Token, credential: &Credential) -> anyhow::Result<()> {
    match cmd {
        Token::Create(c) => {
            let (not_before, expires_at) = window(c.starts.as_deref(), &c.expires)?;
            let body = json!({ "name": c.name, "rules": rules(api, &c.rules).await?, "not_before": not_before, "expires_at": expires_at, "ip_allow": c.ip_allow, "rate_per_minute": c.rate, "ask_otherwise": c.ask_otherwise });
            let made = api.post("/v1/access/tokens", body).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&made)?);
                return Ok(());
            }
            print_grant(&made["grant"]);
            println!("\n{}\n", s(&made["token"]));
            eprintln!("Copy it now: it isn't shown again. Use it as ROLL_TOKEN, or as Authorization: Bearer for the API and MCP.");
        }
        Token::Approve { id } => decide(api, json_out, &id, true).await?,
        Token::Deny { id } => decide(api, json_out, &id, false).await?,
        Token::List { product, resource, workloads, pending } => {
            let mut query: Vec<String> = Vec::new();
            if workloads {
                query.push("kind=workload".into());
            }
            if let Some(p) = &product {
                query.push(format!("product={}", urlencoding::encode(p)));
            }
            if let Some(r) = &resource {
                query.push(format!("resource={}", urlencoding::encode(r)));
            }
            let path = if query.is_empty() { "/v1/access/tokens".to_string() } else { format!("/v1/access/tokens?{}", query.join("&")) };
            let list = api.get(&path).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            let list: Vec<Value> = list.as_array().cloned().unwrap_or_default().into_iter().filter(|g| !pending || g["pending"] == true).collect();
            if list.is_empty() {
                if path.contains('?') || pending {
                    println!("none");
                } else {
                    println!("no tokens yet: roll token create <name> --preset read-only");
                }
            }
            for g in &list {
                print_grant(g);
            }
        }
        Token::Show { id } => {
            let g = api.get(&format!("/v1/access/tokens/{id}")).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&g)?);
            } else {
                print_grant(&g);
            }
        }
        Token::Revoke { id } => {
            let g = api.call(reqwest::Method::DELETE, &format!("/v1/access/tokens/{id}"), None).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&g)?);
            } else {
                println!("revoked {} ({})", s(&g["id"]), s(&g["name"]));
            }
        }
        Token::Request { product, allow, resource, environment, for_, reason, wait } => {
            let mut asked = api.post("/v1/access/requests", json!({ "product": product, "action": allow, "resource": resource, "environment": environment, "for": for_, "reason": reason })).await?;
            let id = s(&asked["id"]).to_string();
            if wait {
                eprintln!("waiting for a person to decide {id} (on roll's approvals page, in chat, or `roll mcp approve {id}`)…");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
                while asked["state"] == "pending" && std::time::Instant::now() < deadline {
                    asked = api.get(&format!("/v1/access/approvals/{}?wait=55", urlencoding::encode(&id))).await?;
                }
            }
            if json_out {
                println!("{}", serde_json::to_string_pretty(&asked)?);
            } else {
                match asked["state"].as_str() {
                    Some("approved") => println!("{id} approved: this token may {allow} on {product} for {for_}"),
                    Some("denied") => anyhow::bail!("{id} denied{}", asked["reason"].as_str().map(|r| format!(": {r}")).unwrap_or_default()),
                    Some("pending") => println!("{id} waits for a person: {}", s(&asked["summary"])),
                    other => anyhow::bail!("{id} is {}", other.unwrap_or("unknown")),
                }
            }
        }
        Token::Tools { id } => {
            let seen = api.get(&format!("/v1/access/tokens/{}/tools", urlencoding::encode(&id))).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&seen)?);
                return Ok(());
            }
            let tools = seen["tools"].as_array().cloned().unwrap_or_default();
            let changes = tools.iter().filter(|t| t["read_only"] != true).count();
            println!("{} tools ({changes} that change things), ~{} tokens of an agent's context", tools.len(), seen["context_tokens"].as_u64().unwrap_or(0));
            for t in &tools {
                let mark = if t["destructive"] == true { "!" } else if t["read_only"] == true { " " } else { "*" };
                println!("  {mark} {}", s(&t["name"]));
            }
            let prompts: Vec<String> = seen["prompts"].as_array().into_iter().flatten().map(|p| s(&p["name"])).collect();
            if !prompts.is_empty() {
                println!("prompts: {}", prompts.join(", "));
            }
        }
        Token::Tighten { id, days, apply } => {
            let t = api.get(&format!("/v1/access/tokens/{id}/tighten?days={days}")).await?;
            let days = t["days"].as_i64().unwrap_or(i64::from(days));
            if apply {
                // Applied over the rules it was made from; the grant as it is after.
                let g = api.post(&format!("/v1/access/tokens/{id}/tighten"), json!({ "from": t["from"], "days": days })).await?;
                if json_out {
                    println!("{}", serde_json::to_string_pretty(&g)?);
                } else if t["narrower"] == true {
                    print_grant(&g);
                } else {
                    println!("nothing to narrow: its rules are as they were");
                }
                return Ok(());
            }
            if json_out {
                println!("{}", serde_json::to_string_pretty(&t)?);
                return Ok(());
            }
            let n: i64 = t["used"].as_array().into_iter().flatten().filter_map(|u| u["count"].as_i64()).sum();
            if t["narrower"] != true {
                println!("{}", if n == 0 { "it made no requests in that time: revoke it if nothing needs it" } else { "it used everything its rules allow: nothing to narrow" });
                return Ok(());
            }
            println!("from {n} requests over {days} days, it would only be able to:");
            for r in t["rules"].as_array().into_iter().flatten() {
                println!("  {}", describe_rule(r));
            }
            eprintln!("roll token tighten {id} --apply to keep only that");
        }
        Token::Checkup => {
            let list = api.get("/v1/access/checkup").await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            let list = list.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("nothing needs attention");
            }
            for c in &list {
                println!("{}  {}", s(&c["grant"]["id"]), s(&c["grant"]["name"]));
                for f in c["findings"].as_array().into_iter().flatten() {
                    let fix = match f["fix"].as_str().unwrap_or("") {
                        "revoke" => format!("roll token revoke {}", s(&c["grant"]["id"])),
                        "tighten" => format!("roll token tighten {} --apply", s(&c["grant"]["id"])),
                        "expire" => format!("roll token edit {} --expires 30d", s(&c["grant"]["id"])),
                        "limit_addresses" => format!("roll token edit {} --ip <cidr>", s(&c["grant"]["id"])),
                        "rotate_client" => "roll token client list, then roll token client rotate <mcl_…> --github owner/repo (or --public-key key.pub.pem)".to_string(),
                        other => other.to_string(),
                    };
                    println!("  {}: {fix}", s(&f["why"]));
                }
            }
        }
        Token::TakeBack { id, approval } => {
            let g = api.call(reqwest::Method::DELETE, &format!("/v1/access/tokens/{id}/temporary/{approval}"), None).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&g)?);
            } else {
                print_grant(&g);
            }
        }
        Token::Rotate { id, keep_old } => {
            let keep = keep_old.as_deref().map(|k| span(k, "--keep-old")).transpose()?.flatten().map(|ms| ms / 1000);
            let made = api.post(&format!("/v1/access/tokens/{id}/rotate"), json!({ "keep_old_for_s": keep })).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&made)?);
                return Ok(());
            }
            println!("{}", s(&made["token"]));
            eprintln!("The old secret stopped working. Copy this one now: it isn't shown again.");
        }
        Token::Edit(e) => {
            let mut patch = serde_json::Map::new();
            if let Some(n) = &e.name {
                patch.insert("name".into(), json!(n));
            }
            if e.rules.given() {
                patch.insert("rules".into(), rules(api, &e.rules).await?);
            }
            if let Some(x) = &e.expires {
                patch.insert("expires_at".into(), json!(expiry(x)?));
            }
            if let Some(ips) = &e.ip_allow {
                let ips: Vec<&String> = ips.iter().filter(|i| i.as_str() != "none").collect();
                patch.insert("ip_allow".into(), json!(ips));
            }
            if let Some(r) = &e.rate {
                let rate = match r.as_str() {
                    "none" => Value::Null,
                    n => json!(n.parse::<u32>().map_err(|_| anyhow::anyhow!("--rate is a number of requests a minute, or none"))?),
                };
                patch.insert("rate_per_minute".into(), rate);
            }
            if let Some(a) = e.ask_otherwise {
                patch.insert("ask_otherwise".into(), json!(a));
            }
            if patch.is_empty() {
                bail!("nothing to change: --name, --expires, --ip, --rate, --ask-otherwise, or new rules (--preset, --allow, --ask)");
            }
            let g = api.call(reqwest::Method::PATCH, &format!("/v1/access/tokens/{}", e.id), Some(Value::Object(patch))).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&g)?);
            } else {
                print_grant(&g);
            }
        }
        Token::Whoami => {
            let Some(source) = &credential.source else {
                bail!("no credential for {}: run `roll login`, or set ROLL_TOKEN", api.base);
            };
            let mut me = api.get("/v1/access").await?;
            if json_out {
                me["credential"] = json!({ "source": source, "unused": credential.unused });
                println!("{}", serde_json::to_string_pretty(&me)?);
                return Ok(());
            }
            println!("Using {source} for {}.", api.base);
            if me["kind"] == "operator" {
                println!("the operator: everything");
            } else {
                print_grant(&me["grant"]);
            }
            for u in &credential.unused {
                println!("note: {u} is set too, and isn't used: {source} wins.");
            }
        }
        Token::Policy { max_days, require_expiry, no_limit, max_idle_days, approval_rules, no_approval_rules } => {
            let mut p = api.get("/v1/access/policy").await?;
            if let Some(d) = max_idle_days {
                p["max_idle_days"] = if d == 0 { Value::Null } else { json!(d) };
            }
            let rules_changed = no_approval_rules || !approval_rules.is_empty();
            if no_approval_rules {
                p["approval_rules"] = json!([]);
            }
            if !approval_rules.is_empty() {
                let mut all = p["approval_rules"].as_array().cloned().unwrap_or_default();
                all.extend(approval_rules);
                p["approval_rules"] = json!(all);
            }
            if max_days.is_some() || require_expiry.is_some() || no_limit || max_idle_days.is_some() || rules_changed {
                if let Some(d) = max_days {
                    p["max_days"] = json!(d);
                }
                if no_limit {
                    p["max_days"] = Value::Null;
                }
                if let Some(r) = require_expiry {
                    p["require_expiry"] = json!(r);
                }
                p = api.call(reqwest::Method::PUT, "/v1/access/policy", Some(p)).await?;
            }
            if json_out {
                println!("{}", serde_json::to_string_pretty(&p)?);
            } else {
                let days = p["max_days"].as_u64().map_or_else(|| "no limit".to_string(), |d| format!("at most {d} days"));
                let must = if p["require_expiry"].as_bool() == Some(true) { ", and must expire" } else { "" };
                let idle = p["max_idle_days"].as_u64().map(|d| format!("; unused for {d} days, they stop")).unwrap_or_default();
                println!("tokens live {days}{must}{idle}");
                for r in p["approval_rules"].as_array().into_iter().flatten() {
                    let names = |k: &str, none: &str| r[k].as_array().map_or_else(|| none.to_string(), |l| l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
                    println!(
                        "{} {} in {}: {} approvals from {}",
                        r["product"].as_str().unwrap_or("?"),
                        names("actions", "anything"),
                        names("environments", "every environment"),
                        r["approvals"],
                        names("approvers", "anyone who decides (never the token's own person)")
                    );
                }
            }
        }
        Token::Client(cmd) => machine(api, json_out, cmd).await?,
        Token::Enterprise { what } => enterprise(api, json_out, what).await?,
        Token::Presets => {
            let c = api.get("/v1/access/catalog").await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&c["presets"])?);
                return Ok(());
            }
            for p in c["presets"].as_array().into_iter().flatten() {
                println!("{:<12} {}\n             {}", s(&p["id"]), s(&p["name"]), s(&p["description"]));
            }
            println!("\nproducts: {}", c["products"].as_array().into_iter().flatten().map(s).collect::<Vec<_>>().join(", "));
            println!("actions:  {}", c["actions"].as_array().into_iter().flatten().map(s).collect::<Vec<_>>().join(", "));
        }
    }
    Ok(())
}

/// The MCP server's URL for an API base.
fn server(api: &str) -> String {
    format!("{}/v1/mcp", api.trim_end_matches('/'))
}

/// How many tools a client takes in one request, if it's known to cap them: VS Code refuses a
/// request with more than 128 (its own included); Cursor is reported to drop tools past about 40.
/// With every product on, roll lists about 300, so these clients get search mode by default.
pub fn tool_limit(client: Client) -> Option<(&'static str, u32)> {
    match client {
        Client::Vscode => Some(("VS Code refuses more than 128 tools a request", 128)),
        Client::Cursor => Some(("Cursor is reported to drop tools past about 40", 40)),
        _ => None,
    }
}

/// How a connection narrows what the server offers it (docs/mcp.md): the URL's query, in the
/// dashboard's order (`toolsets`, `readonly`, `tools`, `lockdown`).
/// A toolset's product must be one roll has (`postgress` is caught here, not when a client first
/// connects and the server answers 400); an area's own name (`apps.observe`) is the server's to know.
pub fn check_toolsets(toolsets: &[String]) -> anyhow::Result<()> {
    for t in toolsets.iter().map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()) {
        let product = t.split_once('.').map_or(t.as_str(), |(p, _)| p);
        if !roll_auth::scope::PRODUCTS.contains(&product) {
            anyhow::bail!("no toolset {t:?}: one of {} (or an area, apps.observe)", roll_auth::scope::PRODUCTS.join(", "));
        }
    }
    Ok(())
}

pub fn with_mode(url: &str, toolsets: &[String], readonly: bool, search: bool, lockdown: bool) -> String {
    let mut q = Vec::new();
    let mut sets: Vec<String> = toolsets.iter().map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()).collect();
    sets.sort();
    sets.dedup();
    if !sets.is_empty() {
        q.push(format!("toolsets={}", sets.join(",")));
    }
    if readonly {
        q.push("readonly=1".into());
    }
    if search {
        q.push("tools=search".into());
    }
    if lockdown {
        q.push("lockdown=1".into());
    }
    if q.is_empty() { url.to_string() } else { format!("{url}?{}", q.join("&")) }
}

/// The config for one client: OAuth by default, a bearer token header with one.
pub fn config(client: Client, url: &str, token: Option<&str>) -> String {
    let header = token.map(|t| format!("Bearer {t}"));
    match client {
        Client::ClaudeCode => {
            // One shell word: a mode's `&` would end the command and run it in the background.
            let url = if url.chars().all(|c| c.is_ascii_alphanumeric() || "_./:@%+=,-".contains(c)) { url.to_string() } else { format!("'{url}'") };
            match &header {
                Some(h) => format!("claude mcp add --transport http roll {url} --header \"Authorization: {h}\""),
                None => format!("claude mcp add --transport http roll {url}"),
            }
        }
        Client::Codex => match &header {
            Some(_) => format!("# ~/.codex/config.toml\n[mcp_servers.roll]\nurl = \"{url}\"\nbearer_token_env_var = \"ROLL_TOKEN\""),
            None => format!("# ~/.codex/config.toml (then: codex mcp login roll)\n[mcp_servers.roll]\nurl = \"{url}\""),
        },
        Client::Vscode => {
            let mut s = json!({ "type": "http", "url": url });
            if let Some(h) = &header {
                s["headers"] = json!({ "Authorization": h });
            }
            // VS Code 1.146's own files (its .vscode/mcp.json goes in February 2027): the user's, so a
            // token never lands in a repository's file.
            let file = if header.is_some() { "// ~/.copilot/mcp-config.json" } else { "// ~/.copilot/mcp-config.json, or .mcp.json at a workspace's root" };
            format!("{file}\n{}", serde_json::to_string_pretty(&json!({ "mcpServers": { "roll": s } })).unwrap_or_default())
        }
        Client::Cursor | Client::ClaudeDesktop | Client::Json => {
            let mut s = json!({ "url": url });
            if let Some(h) = &header {
                s["headers"] = json!({ "Authorization": h });
            }
            let file = match client {
                Client::Cursor => "// ~/.cursor/mcp.json\n",
                Client::ClaudeDesktop => "// Claude Desktop: Settings → Connectors → Add custom connector, or claude_desktop_config.json\n",
                _ => "",
            };
            format!("{file}{}", serde_json::to_string_pretty(&json!({ "mcpServers": { "roll": s } })).unwrap_or_default())
        }
    }
}

fn print_approval(a: &Value) {
    let tool = a["tool"].as_str().map(|t| format!(" via {t}")).unwrap_or_default();
    println!("{}  {}  {}{tool}  ({}, {})", s(&a["id"]), s(&a["state"]), s(&a["summary"]), s(&a["grant_name"]), when(&a["created_at"]));
    if !a["args"].is_null() {
        println!("  {}", a["args"]);
    }
}

pub async fn mcp(api: &Api, json_out: bool, cmd: Mcp) -> anyhow::Result<()> {
    match cmd {
        Mcp::Config { client, with_token, toolsets, readonly, lockdown, search, all_tools } => {
            let limit = tool_limit(client);
            let search = search || (limit.is_some() && !all_tools);
            if let (Some((why, _)), true, false) = (limit, search, json_out) {
                eprintln!("search mode: {why}, and roll lists every product's tools (--all-tools for the whole list)");
            }
            check_toolsets(&toolsets)?;
            println!("{}", config(client, &with_mode(&server(&api.base), &toolsets, readonly, search, lockdown), with_token.as_deref()));
        }
        Mcp::Approvals { all } => {
            let path = if all { "/v1/access/approvals" } else { "/v1/access/approvals?state=pending" };
            let list = api.get(path).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            let list = list.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("nothing waits for you");
            }
            for a in &list {
                print_approval(a);
            }
        }
        Mcp::Approve { id, remember } => {
            let body = remember.map_or_else(|| json!({}), |r| json!({ "remember": r }));
            let a = api.post(&format!("/v1/access/approvals/{id}/approve"), body).await?;
            if json_out { println!("{}", serde_json::to_string_pretty(&a)?) } else { println!("approved: {}", s(&a["summary"])) }
        }
        Mcp::Deny { id, reason } => {
            let a = api.post(&format!("/v1/access/approvals/{id}/deny"), json!({ "reason": reason })).await?;
            if json_out { println!("{}", serde_json::to_string_pretty(&a)?) } else { println!("denied: {}", s(&a["summary"])) }
        }
        Mcp::Sinks { what } => {
            let out = match what {
                Some(SinkCmd::Add { url, from_start }) => api.post("/v1/access/audit/sinks", json!({ "url": url, "from_start": from_start })).await?,
                Some(SinkCmd::Retry { id }) => api.post(&format!("/v1/access/audit/sinks/{id}/retry"), json!({})).await?,
                Some(SinkCmd::Rm { id }) => api.call(reqwest::Method::DELETE, &format!("/v1/access/audit/sinks/{id}"), None).await?,
                None => api.get("/v1/access/audit/sinks").await?,
            };
            if json_out {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if let Some(secret) = out["secret"].as_str() {
                println!("streaming the audit log to {} as {}", s(&out["sink"]["url"]), s(&out["sink"]["id"]));
                println!("signing secret (shown once): {secret}");
            } else if let Some(list) = out.as_array() {
                if list.is_empty() {
                    println!("the audit log isn't streamed anywhere (roll mcp sinks add https://…)");
                }
                for k in list {
                    let err = k["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default();
                    println!("{}  {:<6} {}  sent {}{err}", s(&k["id"]), s(&k["status"]), s(&k["url"]), when(&k["delivered_at"]));
                }
            } else {
                println!("done");
            }
        }
        Mcp::Notify { install, channel, approver, off } => {
            let v = match (install, channel, off) {
                (Some(i), Some(c), _) => {
                    let (ids, people) = chat_approvers(&approver);
                    let mut chat = json!({ "install": i, "channel": c.trim_start_matches('#'), "approvers": ids });
                    if !people.is_empty() {
                        chat["people"] = json!(people);
                    }
                    api.call(reqwest::Method::PUT, "/v1/access/notify", Some(json!({ "chat": chat }))).await?
                }
                (_, _, true) => api.call(reqwest::Method::PUT, "/v1/access/notify", Some(json!({ "chat": null }))).await?,
                _ => api.get("/v1/access/notify").await?,
            };
            if json_out {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else if v["chat"].is_null() {
                println!("approvals go only to the dashboard and `roll mcp approvals`");
            } else {
                let people = &v["chat"]["people"];
                let who: Vec<String> = v["chat"]["approvers"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).map(|u| people[u].as_str().map_or(u.to_string(), |p| format!("{u} ({p})"))).collect()).unwrap_or_default();
                println!("approvals are also posted to {} in {}; decided there by {}", s(&v["chat"]["channel"]), s(&v["chat"]["install"]), who.join(", "));
            }
        }
        Mcp::Audit { format, token, product, decision, resource, action, limit } => {
            let mut q = url::form_urlencoded::Serializer::new(String::new());
            q.append_pair("limit", &limit.to_string());
            for (k, v) in [("grant", token), ("product", product), ("decision", decision), ("resource", resource), ("action", action), ("format", format.clone())] {
                if let Some(v) = v {
                    q.append_pair(k, &v);
                }
            }
            if format.is_some() {
                print!("{}", api.get_text(&format!("/v1/access/audit?{}", q.finish())).await?);
                return Ok(());
            }
            let log = api.get(&format!("/v1/access/audit?{}", q.finish())).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&log)?);
                return Ok(());
            }
            for e in log.as_array().into_iter().flatten() {
                let who = e["grant_name"].as_str().unwrap_or("operator");
                let what = e["tool"].as_str().map(str::to_string).unwrap_or_else(|| format!("{} {}", s(&e["method"]), s(&e["route"])));
                let res = e["resource"].as_str().map(|r| format!(" {r}")).unwrap_or_default();
                println!("{:<8} {:<5} {}  {who}: {what}{res}  ({})", when(&e["at"]), s(&e["decision"]), s(&e["status"]), s(&e["action"]));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_approvers_can_name_their_account() {
        let (ids, people) = chat_approvers(&["U1=usr_ana".into(), "@U2".into()]);
        assert_eq!((ids, people.get("U1").map(String::as_str), people.len()), (vec!["U1".to_string(), "U2".to_string()], Some("usr_ana"), 1));
    }

    #[test]
    fn approval_rules_read_as_the_policy_takes_them() {
        assert_eq!(approval_rule("secrets:reveal@production=2").unwrap(), json!({ "product": "secrets", "actions": ["reveal"], "environments": ["production"], "approvals": 2 }));
        assert_eq!(approval_rule("postgres=1/usr_a, usr_b").unwrap(), json!({ "product": "postgres", "approvals": 1, "approvers": ["usr_a", "usr_b"] }));
        assert!(approval_rule("secrets:reveal").is_err() && approval_rule("=2").is_err() && approval_rule("apps=two").is_err());
    }

    #[test]
    fn expiry_and_dates() {
        assert!(expiry("never").unwrap().is_none());
        assert!(expiry("30d").unwrap().unwrap() > 0 && expiry("1y").is_err());
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_794_509_022_828), "2026-11-12");
        assert!(config(Client::ClaudeCode, "https://api.roll.sh/v1/mcp", Some("rlt_x")).contains("Authorization: Bearer rlt_x"));
        // A connection's mode is its URL's query, in the dashboard's order.
        let url = "https://api.roll.sh/v1/mcp";
        assert_eq!(with_mode(url, &[], false, false, false), url);
        assert_eq!(with_mode(url, &["redis".into(), "Postgres".into()], true, true, true), format!("{url}?toolsets=postgres,redis&readonly=1&tools=search&lockdown=1"));
        assert!(tool_limit(Client::Vscode).is_some() && tool_limit(Client::ClaudeCode).is_none());
        // An area goes through; a product roll doesn't have is refused before a config is written.
        assert_eq!(with_mode(url, &["apps.observe".into()], false, false, false), format!("{url}?toolsets=apps.observe"));
        assert!(check_toolsets(&["apps.observe".into(), "postgres".into()]).is_ok() && check_toolsets(&["postgress".into()]).is_err());
        assert_eq!(config(Client::ClaudeCode, &format!("{url}?tools=search&lockdown=1"), None), format!("claude mcp add --transport http roll '{url}?tools=search&lockdown=1'"));
    }
}
