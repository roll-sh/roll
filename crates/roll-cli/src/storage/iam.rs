//! `roll storage iam …`: IAM users, groups, policies and roles, and the policy simulator, through
//! the control plane's IAM endpoint (`POST /v1/storage/iam`, the IAM query API as JSON). It acts
//! in the account `roll storage` acts in, as its root user.

use super::duration;
use crate::Api;
use crate::queues::fmt::{Align::L, Table};
use crate::s;
use anyhow::{Context, bail};
use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum IamCmd {
    /// List users, or manage one.
    Users {
        #[command(subcommand)]
        action: Option<UsersCmd>,
    },
    /// List groups, or manage one.
    Groups {
        #[command(subcommand)]
        action: Option<GroupsCmd>,
    },
    /// List managed policies, or manage one.
    Policies {
        /// Which policies to list.
        #[arg(long, value_enum, default_value_t = Scope::Local)]
        scope: Scope,
        #[command(subcommand)]
        action: Option<PoliciesCmd>,
    },
    /// List IAM roles, or manage one.
    Roles {
        #[command(subcommand)]
        action: Option<IamRolesCmd>,
    },
    /// Simulate a user's, group's or role's policies (SimulatePrincipalPolicy).
    Simulate {
        /// The user, group or role (name or ARN; `user/alice`, `role/deploy`, `group/ops`).
        principal: String,
        /// An action to check (repeatable).
        #[arg(long = "action", required = true, value_name = "ACTION")]
        actions: Vec<String>,
        /// A resource ARN (repeatable; default `*`).
        #[arg(long = "resource", value_name = "ARN")]
        resources: Vec<String>,
        /// A condition key's value (`aws:SourceIp=10.0.0.1`, repeatable).
        #[arg(long = "context", value_name = "KEY=VALUE")]
        context: Vec<String>,
        /// A resource policy to evaluate too (JSON, or @file).
        #[arg(long)]
        resource_policy: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Scope {
    Local,
    Aws,
    All,
}

#[derive(Subcommand)]
pub enum UsersCmd {
    /// Create a user.
    Create {
        name: String,
        #[arg(long)]
        path: Option<String>,
        /// A tag (`key=value`, repeatable).
        #[arg(long = "tag", value_name = "KEY=VALUE")]
        tags: Vec<String>,
    },
    /// Show a user: groups, policies, access keys, MFA devices, console password.
    Show { name: String },
    /// Delete a user (its keys, policies, groups, password and MFA devices must be gone).
    Delete { name: String },
    /// List a user's access keys.
    Keys { name: String },
    /// Create an access key for a user (the secret is shown once).
    CreateKey { name: String },
    /// Deactivate (`--inactive`) or reactivate an access key.
    UpdateKey {
        name: String,
        key_id: String,
        #[arg(long)]
        inactive: bool,
    },
    /// Delete an access key.
    DeleteKey { name: String, key_id: String },
    /// Set (or create) the user's console password, for dashboard sign-in.
    SetPassword {
        name: String,
        /// The password (`--password-stdin` keeps it out of the shell's history).
        #[arg(long, conflicts_with = "password_stdin")]
        password: Option<String>,
        /// Read the password from standard input.
        #[arg(long)]
        password_stdin: bool,
        /// The user must choose a new password at the next sign-in.
        #[arg(long)]
        reset_required: bool,
    },
    /// Remove the user's console password.
    RemovePassword { name: String },
    /// Attach a managed policy (name or ARN).
    Attach { name: String, policy: String },
    /// Detach a managed policy.
    Detach { name: String, policy: String },
    /// Put an inline policy (JSON, or @file).
    PutPolicy { name: String, policy_name: String, document: String },
    /// Delete an inline policy.
    DeletePolicy { name: String, policy_name: String },
}

#[derive(Subcommand)]
pub enum GroupsCmd {
    Create {
        name: String,
        #[arg(long)]
        path: Option<String>,
    },
    /// Show a group and its members.
    Show { name: String },
    Delete { name: String },
    AddUser { name: String, user: String },
    RemoveUser { name: String, user: String },
    /// Attach a managed policy (name or ARN).
    Attach { name: String, policy: String },
    Detach { name: String, policy: String },
    /// Put an inline policy (JSON, or @file).
    PutPolicy { name: String, policy_name: String, document: String },
    DeletePolicy { name: String, policy_name: String },
}

#[derive(Subcommand)]
pub enum PoliciesCmd {
    /// Create a customer-managed policy (JSON, or @file).
    Create {
        name: String,
        document: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        path: Option<String>,
    },
    /// Show a policy and its default version's document.
    Show { policy: String },
    /// Replace a policy's document: a new default version (the oldest non-default one goes when
    /// there are five).
    Update { policy: String, document: String },
    Delete { policy: String },
}

#[derive(Subcommand)]
pub enum IamRolesCmd {
    /// Create a role with a trust policy (JSON, or @file).
    Create {
        name: String,
        #[arg(long)]
        trust: String,
        #[arg(long)]
        description: Option<String>,
        /// Longest session (15m to 12h; default 1h).
        #[arg(long, value_name = "DURATION")]
        max_session: Option<String>,
        #[arg(long)]
        path: Option<String>,
    },
    Show { name: String },
    Delete { name: String },
    /// Replace the trust policy (JSON, or @file).
    UpdateTrust { name: String, trust: String },
    Attach { name: String, policy: String },
    Detach { name: String, policy: String },
    PutPolicy { name: String, policy_name: String, document: String },
    DeletePolicy { name: String, policy_name: String },
}

/// One IAM call: the action's result as JSON.
pub async fn call(api: &Api, action: &str, params: Value) -> anyhow::Result<Value> {
    let mut body = match params {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    body.insert("Action".into(), json!(action));
    api.post("/v1/storage/iam", Value::Object(body)).await
}

/// A list in a result (`null` is empty).
fn items(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        x => vec![x.clone()],
    }
}

/// Every page of a list action.
async fn all(api: &Api, action: &str, params: Value, list: &str) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let mut p = params.clone();
        p["MaxItems"] = json!(1000);
        if let Some(m) = &marker {
            p["Marker"] = json!(m);
        }
        let v = call(api, action, p).await?;
        out.extend(items(&v[list]));
        match (v["IsTruncated"].as_str(), v["Marker"].as_str()) {
            (Some("true"), Some(m)) => marker = Some(m.to_string()),
            _ => return Ok(out),
        }
    }
}

/// A document given inline or as `@file`, as text (checked to be JSON).
fn document(v: &str) -> anyhow::Result<String> {
    let text = match v.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?,
        None => v.to_string(),
    };
    let parsed: Value = serde_json::from_str(&text).context("the policy document isn't JSON")?;
    Ok(parsed.to_string())
}

fn tag_list(tags: &[String]) -> anyhow::Result<Value> {
    let mut out = Vec::new();
    for t in tags {
        let (k, v) = t.split_once('=').with_context(|| format!("tags are key=value, got {t:?}"))?;
        out.push(json!({ "Key": k, "Value": v }));
    }
    Ok(json!(out))
}

/// The account `roll storage` acts in.
async fn account(api: &Api) -> anyhow::Result<String> {
    let v = api.get("/v1/storage").await?;
    v["account_id"].as_str().map(str::to_string).context("the control plane didn't say which account it acts in")
}

/// A managed policy's ARN from a name or ARN (`AmazonS3ReadOnlyAccess` is AWS's).
async fn policy_arn(api: &Api, p: &str) -> anyhow::Result<String> {
    if p.starts_with("arn:") {
        return Ok(p.to_string());
    }
    if p.starts_with("AmazonS3") {
        return Ok(format!("arn:aws:iam::aws:policy/{p}"));
    }
    Ok(format!("arn:aws:iam::{}:policy/{p}", account(api).await?))
}

/// A user, group or role ARN from a name or ARN.
async fn principal_arn(api: &Api, p: &str) -> anyhow::Result<String> {
    if p.starts_with("arn:") {
        return Ok(p.to_string());
    }
    let (kind, name) = p.split_once('/').unwrap_or(("user", p));
    if !matches!(kind, "user" | "group" | "role") {
        bail!("give user/<name>, group/<name>, role/<name> or an ARN");
    }
    Ok(format!("arn:aws:iam::{}:{kind}/{name}", account(api).await?))
}

fn done(as_json: bool, what: String) -> anyhow::Result<Option<Value>> {
    if as_json {
        return Ok(Some(json!({ "ok": true })));
    }
    println!("{what}");
    Ok(None)
}

fn print_doc(doc: &Value) {
    let text = doc.as_str().unwrap_or_default();
    match serde_json::from_str::<Value>(text) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(_) => println!("{text}"),
    }
}

pub async fn iam(api: &Api, as_json: bool, cmd: IamCmd) -> anyhow::Result<Option<Value>> {
    match cmd {
        IamCmd::Users { action } => users(api, as_json, action).await,
        IamCmd::Groups { action } => groups(api, as_json, action).await,
        IamCmd::Policies { scope, action } => policies(api, as_json, scope, action).await,
        IamCmd::Roles { action } => roles(api, as_json, action).await,
        IamCmd::Simulate { principal, actions, resources, context, resource_policy } => {
            let mut p = json!({ "PolicySourceArn": principal_arn(api, &principal).await?, "ActionNames": actions });
            if !resources.is_empty() {
                p["ResourceArns"] = json!(resources);
            }
            if !context.is_empty() {
                let mut entries = Vec::new();
                for c in &context {
                    let (k, v) = c.split_once('=').with_context(|| format!("context values are key=value, got {c:?}"))?;
                    entries.push(json!({ "ContextKeyName": k, "ContextKeyType": "string", "ContextKeyValues": [v] }));
                }
                p["ContextEntries"] = json!(entries);
            }
            if let Some(rp) = resource_policy {
                p["ResourcePolicy"] = json!(document(&rp)?);
            }
            let v = call(api, "SimulatePrincipalPolicy", p).await?;
            if as_json {
                return Ok(Some(v));
            }
            let mut t = Table::new(&[("ACTION", L), ("RESOURCE", L), ("DECISION", L), ("BECAUSE OF", L), ("MISSING CONTEXT", L)]);
            for r in items(&v["EvaluationResults"]) {
                let why: Vec<String> = items(&r["MatchedStatements"]).iter().map(|m| s(&m["SourcePolicyId"])).collect();
                let missing: Vec<String> = items(&r["MissingContextValues"]).iter().map(s).collect();
                t.row(vec![s(&r["EvalActionName"]), s(&r["EvalResourceName"]), s(&r["EvalDecision"]), why.join(", "), missing.join(", ")]);
            }
            t.print();
            Ok(None)
        }
    }
}

async fn users(api: &Api, as_json: bool, cmd: Option<UsersCmd>) -> anyhow::Result<Option<Value>> {
    let Some(cmd) = cmd else {
        let users = all(api, "ListUsers", json!({}), "Users").await?;
        if as_json {
            return Ok(Some(json!(users)));
        }
        if users.is_empty() {
            println!("no users; create one with `roll storage iam users create <name>`");
            return Ok(None);
        }
        let mut t = Table::new(&[("NAME", L), ("ID", L), ("ARN", L), ("CREATED", L)]);
        for u in users {
            t.row(vec![s(&u["UserName"]), s(&u["UserId"]), s(&u["Arn"]), s(&u["CreateDate"])]);
        }
        t.print();
        return Ok(None);
    };
    match cmd {
        UsersCmd::Create { name, path, tags } => {
            let mut p = json!({ "UserName": name });
            if let Some(path) = path {
                p["Path"] = json!(path);
            }
            if !tags.is_empty() {
                p["Tags"] = tag_list(&tags)?;
            }
            let v = call(api, "CreateUser", p).await?;
            if as_json {
                return Ok(Some(v));
            }
            println!("created user {}  {}", s(&v["User"]["UserName"]), s(&v["User"]["Arn"]));
            Ok(None)
        }
        UsersCmd::Show { name } => {
            let u = call(api, "GetUser", json!({ "UserName": name })).await?;
            let groups = all(api, "ListGroupsForUser", json!({ "UserName": name }), "Groups").await?;
            let attached = all(api, "ListAttachedUserPolicies", json!({ "UserName": name }), "AttachedPolicies").await?;
            let inline = all(api, "ListUserPolicies", json!({ "UserName": name }), "PolicyNames").await?;
            let keys = all(api, "ListAccessKeys", json!({ "UserName": name }), "AccessKeyMetadata").await?;
            let mfa = all(api, "ListMFADevices", json!({ "UserName": name }), "MFADevices").await?;
            let login = match call(api, "GetLoginProfile", json!({ "UserName": name })).await {
                Ok(v) => v["LoginProfile"].clone(),
                Err(e) if e.to_string().contains("cannot be found") => Value::Null,
                Err(e) => return Err(e),
            };
            let v = json!({ "user": u["User"], "groups": groups, "attached_policies": attached, "inline_policies": inline, "access_keys": keys, "mfa_devices": mfa, "login_profile": login });
            if as_json {
                return Ok(Some(v));
            }
            let user = &v["user"];
            println!("{}  {}", s(&user["UserName"]), s(&user["Arn"]));
            println!("  id         {}", s(&user["UserId"]));
            println!("  created    {}", s(&user["CreateDate"]));
            println!("  password   {}", if login.is_null() { "none".to_string() } else if login["PasswordResetRequired"] == "true" { "set (must change at next sign-in)".into() } else { "set".into() });
            println!("  groups     {}", groups.iter().map(|g| s(&g["GroupName"])).collect::<Vec<_>>().join(", "));
            println!("  attached   {}", attached.iter().map(|p| s(&p["PolicyName"])).collect::<Vec<_>>().join(", "));
            println!("  inline     {}", inline.iter().map(s).collect::<Vec<_>>().join(", "));
            println!("  keys       {}", keys.iter().map(|k| format!("{} ({})", s(&k["AccessKeyId"]), s(&k["Status"]))).collect::<Vec<_>>().join(", "));
            println!("  mfa        {}", mfa.iter().map(|d| s(&d["SerialNumber"])).collect::<Vec<_>>().join(", "));
            Ok(None)
        }
        UsersCmd::Delete { name } => {
            call(api, "DeleteUser", json!({ "UserName": name })).await?;
            done(as_json, format!("deleted user {name}"))
        }
        UsersCmd::Keys { name } => {
            let keys = all(api, "ListAccessKeys", json!({ "UserName": name }), "AccessKeyMetadata").await?;
            if as_json {
                return Ok(Some(json!(keys)));
            }
            let mut t = Table::new(&[("ACCESS KEY ID", L), ("STATUS", L), ("CREATED", L)]);
            for k in keys {
                t.row(vec![s(&k["AccessKeyId"]), s(&k["Status"]), s(&k["CreateDate"])]);
            }
            t.print();
            Ok(None)
        }
        UsersCmd::CreateKey { name } => {
            let v = call(api, "CreateAccessKey", json!({ "UserName": name })).await?;
            if as_json {
                return Ok(Some(v));
            }
            let k = &v["AccessKey"];
            println!("access key id      {}", s(&k["AccessKeyId"]));
            println!("secret access key  {}", s(&k["SecretAccessKey"]));
            eprintln!("the secret is shown only now");
            Ok(None)
        }
        UsersCmd::UpdateKey { name, key_id, inactive } => {
            let status = if inactive { "Inactive" } else { "Active" };
            call(api, "UpdateAccessKey", json!({ "UserName": name, "AccessKeyId": key_id, "Status": status })).await?;
            done(as_json, format!("{key_id} is {}", status.to_lowercase()))
        }
        UsersCmd::DeleteKey { name, key_id } => {
            call(api, "DeleteAccessKey", json!({ "UserName": name, "AccessKeyId": key_id })).await?;
            done(as_json, format!("deleted access key {key_id}"))
        }
        UsersCmd::SetPassword { name, password, password_stdin, reset_required } => {
            let pw = match (password, password_stdin) {
                (Some(p), _) => p,
                (None, true) => {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim_end_matches(['\r', '\n']).to_string()
                }
                (None, false) => bail!("give --password or --password-stdin"),
            };
            let p = json!({ "UserName": name, "Password": pw, "PasswordResetRequired": reset_required });
            let have = call(api, "GetLoginProfile", json!({ "UserName": name })).await;
            match have {
                Ok(_) => call(api, "UpdateLoginProfile", p).await?,
                Err(e) if e.to_string().contains("cannot be found") => call(api, "CreateLoginProfile", p).await?,
                Err(e) => return Err(e),
            };
            done(as_json, format!("set {name}'s console password{}", if reset_required { " (to change at the next sign-in)" } else { "" }))
        }
        UsersCmd::RemovePassword { name } => {
            call(api, "DeleteLoginProfile", json!({ "UserName": name })).await?;
            done(as_json, format!("removed {name}'s console password"))
        }
        UsersCmd::Attach { name, policy } => {
            let arn = policy_arn(api, &policy).await?;
            call(api, "AttachUserPolicy", json!({ "UserName": name, "PolicyArn": arn })).await?;
            done(as_json, format!("attached {arn} to {name}"))
        }
        UsersCmd::Detach { name, policy } => {
            let arn = policy_arn(api, &policy).await?;
            call(api, "DetachUserPolicy", json!({ "UserName": name, "PolicyArn": arn })).await?;
            done(as_json, format!("detached {arn} from {name}"))
        }
        UsersCmd::PutPolicy { name, policy_name, document: d } => {
            call(api, "PutUserPolicy", json!({ "UserName": name, "PolicyName": policy_name, "PolicyDocument": document(&d)? })).await?;
            done(as_json, format!("put inline policy {policy_name} on {name}"))
        }
        UsersCmd::DeletePolicy { name, policy_name } => {
            call(api, "DeleteUserPolicy", json!({ "UserName": name, "PolicyName": policy_name })).await?;
            done(as_json, format!("deleted inline policy {policy_name} of {name}"))
        }
    }
}

async fn groups(api: &Api, as_json: bool, cmd: Option<GroupsCmd>) -> anyhow::Result<Option<Value>> {
    let Some(cmd) = cmd else {
        let groups = all(api, "ListGroups", json!({}), "Groups").await?;
        if as_json {
            return Ok(Some(json!(groups)));
        }
        let mut t = Table::new(&[("NAME", L), ("ID", L), ("ARN", L)]);
        for g in groups {
            t.row(vec![s(&g["GroupName"]), s(&g["GroupId"]), s(&g["Arn"])]);
        }
        t.print();
        return Ok(None);
    };
    match cmd {
        GroupsCmd::Create { name, path } => {
            let mut p = json!({ "GroupName": name });
            if let Some(path) = path {
                p["Path"] = json!(path);
            }
            let v = call(api, "CreateGroup", p).await?;
            if as_json {
                return Ok(Some(v));
            }
            println!("created group {}  {}", s(&v["Group"]["GroupName"]), s(&v["Group"]["Arn"]));
            Ok(None)
        }
        GroupsCmd::Show { name } => {
            let v = call(api, "GetGroup", json!({ "GroupName": name })).await?;
            if as_json {
                return Ok(Some(v));
            }
            println!("{}  {}", s(&v["Group"]["GroupName"]), s(&v["Group"]["Arn"]));
            for u in items(&v["Users"]) {
                println!("  {}", s(&u["UserName"]));
            }
            Ok(None)
        }
        GroupsCmd::Delete { name } => {
            call(api, "DeleteGroup", json!({ "GroupName": name })).await?;
            done(as_json, format!("deleted group {name}"))
        }
        GroupsCmd::AddUser { name, user } => {
            call(api, "AddUserToGroup", json!({ "GroupName": name, "UserName": user })).await?;
            done(as_json, format!("added {user} to {name}"))
        }
        GroupsCmd::RemoveUser { name, user } => {
            call(api, "RemoveUserFromGroup", json!({ "GroupName": name, "UserName": user })).await?;
            done(as_json, format!("removed {user} from {name}"))
        }
        GroupsCmd::Attach { name, policy } => {
            let arn = policy_arn(api, &policy).await?;
            call(api, "AttachGroupPolicy", json!({ "GroupName": name, "PolicyArn": arn })).await?;
            done(as_json, format!("attached {arn} to {name}"))
        }
        GroupsCmd::Detach { name, policy } => {
            let arn = policy_arn(api, &policy).await?;
            call(api, "DetachGroupPolicy", json!({ "GroupName": name, "PolicyArn": arn })).await?;
            done(as_json, format!("detached {arn} from {name}"))
        }
        GroupsCmd::PutPolicy { name, policy_name, document: d } => {
            call(api, "PutGroupPolicy", json!({ "GroupName": name, "PolicyName": policy_name, "PolicyDocument": document(&d)? })).await?;
            done(as_json, format!("put inline policy {policy_name} on {name}"))
        }
        GroupsCmd::DeletePolicy { name, policy_name } => {
            call(api, "DeleteGroupPolicy", json!({ "GroupName": name, "PolicyName": policy_name })).await?;
            done(as_json, format!("deleted inline policy {policy_name} of {name}"))
        }
    }
}

async fn policies(api: &Api, as_json: bool, scope: Scope, cmd: Option<PoliciesCmd>) -> anyhow::Result<Option<Value>> {
    let Some(cmd) = cmd else {
        let scope = match scope {
            Scope::Local => "Local",
            Scope::Aws => "AWS",
            Scope::All => "All",
        };
        let ps = all(api, "ListPolicies", json!({ "Scope": scope }), "Policies").await?;
        if as_json {
            return Ok(Some(json!(ps)));
        }
        let mut t = Table::new(&[("NAME", L), ("ARN", L), ("VERSION", L), ("ATTACHED", L)]);
        for p in ps {
            t.row(vec![s(&p["PolicyName"]), s(&p["Arn"]), s(&p["DefaultVersionId"]), s(&p["AttachmentCount"])]);
        }
        t.print();
        return Ok(None);
    };
    match cmd {
        PoliciesCmd::Create { name, document: d, description, path } => {
            let mut p = json!({ "PolicyName": name, "PolicyDocument": document(&d)? });
            if let Some(x) = description {
                p["Description"] = json!(x);
            }
            if let Some(x) = path {
                p["Path"] = json!(x);
            }
            let v = call(api, "CreatePolicy", p).await?;
            if as_json {
                return Ok(Some(v));
            }
            println!("created policy {}", s(&v["Policy"]["Arn"]));
            Ok(None)
        }
        PoliciesCmd::Show { policy } => {
            let arn = policy_arn(api, &policy).await?;
            let p = call(api, "GetPolicy", json!({ "PolicyArn": arn })).await?;
            let ver = call(api, "GetPolicyVersion", json!({ "PolicyArn": arn, "VersionId": p["Policy"]["DefaultVersionId"] })).await?;
            if as_json {
                return Ok(Some(json!({ "policy": p["Policy"], "version": ver["PolicyVersion"] })));
            }
            println!("{}  {} ({})", s(&p["Policy"]["PolicyName"]), s(&p["Policy"]["Arn"]), s(&p["Policy"]["DefaultVersionId"]));
            print_doc(&ver["PolicyVersion"]["Document"]);
            Ok(None)
        }
        PoliciesCmd::Update { policy, document: d } => {
            let arn = policy_arn(api, &policy).await?;
            let doc = document(&d)?;
            let versions = all(api, "ListPolicyVersions", json!({ "PolicyArn": arn }), "Versions").await?;
            if versions.len() >= 5
                && let Some(oldest) = versions.iter().filter(|v| v["IsDefaultVersion"] != "true").min_by_key(|v| s(&v["CreateDate"]))
            {
                call(api, "DeletePolicyVersion", json!({ "PolicyArn": arn, "VersionId": oldest["VersionId"] })).await?;
            }
            let v = call(api, "CreatePolicyVersion", json!({ "PolicyArn": arn, "PolicyDocument": doc, "SetAsDefault": true })).await?;
            if as_json {
                return Ok(Some(v));
            }
            println!("{arn} is now {}", s(&v["PolicyVersion"]["VersionId"]));
            Ok(None)
        }
        PoliciesCmd::Delete { policy } => {
            let arn = policy_arn(api, &policy).await?;
            // IAM deletes a policy with its default version only: the others go first.
            for v in all(api, "ListPolicyVersions", json!({ "PolicyArn": arn }), "Versions").await? {
                if v["IsDefaultVersion"] != "true" {
                    call(api, "DeletePolicyVersion", json!({ "PolicyArn": arn, "VersionId": v["VersionId"] })).await?;
                }
            }
            call(api, "DeletePolicy", json!({ "PolicyArn": arn })).await?;
            done(as_json, format!("deleted policy {arn}"))
        }
    }
}

async fn roles(api: &Api, as_json: bool, cmd: Option<IamRolesCmd>) -> anyhow::Result<Option<Value>> {
    let Some(cmd) = cmd else {
        let rs = all(api, "ListRoles", json!({}), "Roles").await?;
        if as_json {
            return Ok(Some(json!(rs)));
        }
        let mut t = Table::new(&[("NAME", L), ("ARN", L), ("MAX SESSION", L)]);
        for r in rs {
            t.row(vec![s(&r["RoleName"]), s(&r["Arn"]), format!("{}s", s(&r["MaxSessionDuration"]))]);
        }
        t.print();
        return Ok(None);
    };
    match cmd {
        IamRolesCmd::Create { name, trust, description, max_session, path } => {
            let mut p = json!({ "RoleName": name, "AssumeRolePolicyDocument": document(&trust)? });
            if let Some(d) = description {
                p["Description"] = json!(d);
            }
            if let Some(m) = max_session {
                p["MaxSessionDuration"] = json!(duration(&m)?);
            }
            if let Some(x) = path {
                p["Path"] = json!(x);
            }
            let v = call(api, "CreateRole", p).await?;
            if as_json {
                return Ok(Some(v));
            }
            println!("created role {}", s(&v["Role"]["Arn"]));
            Ok(None)
        }
        IamRolesCmd::Show { name } => {
            let r = call(api, "GetRole", json!({ "RoleName": name })).await?;
            let attached = all(api, "ListAttachedRolePolicies", json!({ "RoleName": name }), "AttachedPolicies").await?;
            let inline = all(api, "ListRolePolicies", json!({ "RoleName": name }), "PolicyNames").await?;
            if as_json {
                return Ok(Some(json!({ "role": r["Role"], "attached_policies": attached, "inline_policies": inline })));
            }
            let role = &r["Role"];
            println!("{}  {}", s(&role["RoleName"]), s(&role["Arn"]));
            println!("  max session  {}s", s(&role["MaxSessionDuration"]));
            println!("  attached     {}", attached.iter().map(|p| s(&p["PolicyName"])).collect::<Vec<_>>().join(", "));
            println!("  inline       {}", inline.iter().map(s).collect::<Vec<_>>().join(", "));
            println!("  trust");
            print_doc(&role["AssumeRolePolicyDocument"]);
            Ok(None)
        }
        IamRolesCmd::Delete { name } => {
            call(api, "DeleteRole", json!({ "RoleName": name })).await?;
            done(as_json, format!("deleted role {name}"))
        }
        IamRolesCmd::UpdateTrust { name, trust } => {
            call(api, "UpdateAssumeRolePolicy", json!({ "RoleName": name, "PolicyDocument": document(&trust)? })).await?;
            done(as_json, format!("replaced {name}'s trust policy"))
        }
        IamRolesCmd::Attach { name, policy } => {
            let arn = policy_arn(api, &policy).await?;
            call(api, "AttachRolePolicy", json!({ "RoleName": name, "PolicyArn": arn })).await?;
            done(as_json, format!("attached {arn} to {name}"))
        }
        IamRolesCmd::Detach { name, policy } => {
            let arn = policy_arn(api, &policy).await?;
            call(api, "DetachRolePolicy", json!({ "RoleName": name, "PolicyArn": arn })).await?;
            done(as_json, format!("detached {arn} from {name}"))
        }
        IamRolesCmd::PutPolicy { name, policy_name, document: d } => {
            call(api, "PutRolePolicy", json!({ "RoleName": name, "PolicyName": policy_name, "PolicyDocument": document(&d)? })).await?;
            done(as_json, format!("put inline policy {policy_name} on {name}"))
        }
        IamRolesCmd::DeletePolicy { name, policy_name } => {
            call(api, "DeleteRolePolicy", json!({ "RoleName": name, "PolicyName": policy_name })).await?;
            done(as_json, format!("deleted inline policy {policy_name} of {name}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_and_documents() {
        assert_eq!(items(&Value::Null), Vec::<Value>::new());
        assert_eq!(items(&json!([1, 2])).len(), 2);
        assert_eq!(document(r#"{ "Version": "2012-10-17" }"#).unwrap(), r#"{"Version":"2012-10-17"}"#);
        assert!(document("not json").is_err());
        assert_eq!(tag_list(&["team=web".into()]).unwrap(), json!([{"Key": "team", "Value": "web"}]));
        assert!(tag_list(&["nope".into()]).is_err());
    }
}
