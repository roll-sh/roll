//! `roll projects …` and `roll invites …`: a workspace project's members, invites and roles
//! (docs/members.md). Accepting an invite is a signed-in person's own, so `roll invites` opens the
//! dashboard for it.

use crate::{Api, ago, s};
use anyhow::bail;
use clap::Subcommand;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum Projects {
    /// A project's members (`list`, `invite`, `set-role`, `remove`).
    #[command(subcommand)]
    Members(Members),
    /// Leave a project (its last owner can't).
    Leave {
        /// A workspace project (id or name); default: your one project.
        #[arg(long, short)]
        project: Option<String>,
    },
    /// A project's pending invites (`list`, `revoke`).
    #[command(subcommand)]
    Invites(ProjectInvites),
    /// The built-in roles and what each may do.
    Roles,
}

#[derive(Subcommand)]
pub enum Members {
    /// Who is in the project, with their roles.
    #[command(alias = "ls")]
    List {
        #[arg(long, short)]
        project: Option<String>,
    },
    /// Invite an email address: they accept by signing in (the link is printed once).
    Invite {
        email: String,
        /// viewer, developer, admin or owner (never more than your own role).
        #[arg(long, default_value = "developer")]
        role: String,
        /// A developer's environments, where the role applies in full (default: every environment
        /// that isn't protected).
        #[arg(long = "env", value_delimiter = ',')]
        environments: Vec<String>,
        #[arg(long, short)]
        project: Option<String>,
    },
    /// Change a member's role (a `usr_…` id or a handle).
    SetRole {
        user: String,
        #[arg(long)]
        role: String,
        #[arg(long = "env", value_delimiter = ',')]
        environments: Vec<String>,
        #[arg(long, short)]
        project: Option<String>,
    },
    /// Take a member out of the project: their tokens confined to it are revoked.
    #[command(alias = "rm")]
    Remove {
        user: String,
        #[arg(long, short)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ProjectInvites {
    #[command(alias = "ls")]
    List {
        #[arg(long, short)]
        project: Option<String>,
    },
    /// Revoke a pending invite (`pin_…`): its link stops working.
    Revoke {
        id: String,
        #[arg(long, short)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum Invites {
    /// Where your invites wait: the dashboard (they're your session's, never a token's).
    #[command(alias = "ls")]
    List,
    /// Accept an invite: opens it in the dashboard, where you sign in.
    Accept { token: String },
    /// Decline an invite: opens it in the dashboard.
    Decline { token: String },
}

/// A project by id or name; none named: the caller's only project.
async fn project_id(api: &Api, p: Option<&str>) -> anyhow::Result<String> {
    if let Some(p) = p.filter(|p| p.starts_with("prj_")) {
        return Ok(p.to_string());
    }
    let all = api.get("/v1/workspace/projects").await?;
    let list: Vec<&Value> = all.as_array().into_iter().flatten().collect();
    match (p, list.as_slice()) {
        (Some(name), _) => match list.iter().find(|x| x["name"] == name) {
            Some(x) => Ok(s(&x["id"])),
            None => bail!("no workspace project {name}"),
        },
        (None, [one]) => Ok(s(&one["id"])),
        (None, []) => bail!("you're in no workspace project"),
        (None, many) => bail!("say which project with --project: {}", many.iter().map(|p| s(&p["name"])).collect::<Vec<_>>().join(", ")),
    }
}

/// A member by `usr_…` id or handle.
async fn user_id(api: &Api, project: &str, user: &str) -> anyhow::Result<String> {
    if user.starts_with("usr_") {
        return Ok(user.to_string());
    }
    let ms = api.get(&format!("/v1/workspace/projects/{project}/members")).await?;
    match ms.as_array().into_iter().flatten().find(|m| m["handle"] == user) {
        Some(m) => Ok(s(&m["user"])),
        None => bail!("{user} isn't a member of this project"),
    }
}

fn envs(e: Vec<String>) -> Value {
    if e.is_empty() { Value::Null } else { json!(e) }
}

fn print_member(m: &Value) {
    let envs = m["environments"].as_array().map(|e| e.iter().map(s).collect::<Vec<_>>().join(",")).unwrap_or_default();
    let note = if m["inactive"].as_bool() == Some(true) { "  (can't sign in)" } else { "" };
    println!("{:<20} {:<10} {:<24} {} {}{note}", s(&m["handle"]), s(&m["role"]), envs, s(&m["user"]), ago(m["added_at"].as_i64().unwrap_or(0)));
}

/// The dashboard's invites page for `token` (`ROLL_WEB_URL`, else the API's own origin).
fn invites_page(api: &Api, token: Option<&str>) -> String {
    let web = std::env::var("ROLL_WEB_URL").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| api.base.clone());
    let web = web.trim_end_matches('/');
    match token {
        Some(t) => format!("{web}/dashboard/invites?token={t}"),
        None => format!("{web}/dashboard/invites"),
    }
}

pub async fn projects(api: &Api, as_json: bool, cmd: Projects) -> anyhow::Result<()> {
    let out = match cmd {
        Projects::Members(Members::List { project }) => {
            let p = project_id(api, project.as_deref()).await?;
            let v = api.get(&format!("/v1/workspace/projects/{p}/members")).await?;
            if !as_json {
                v.as_array().into_iter().flatten().for_each(print_member);
                return Ok(());
            }
            v
        }
        Projects::Members(Members::Invite { email, role, environments, project }) => {
            let p = project_id(api, project.as_deref()).await?;
            let v = api.post(&format!("/v1/workspace/projects/{p}/invites"), json!({ "email": email, "role": role, "environments": envs(environments) })).await?;
            if !as_json {
                println!("invited {} as {} ({}): the link works once, for 7 days, for that address only:\n{}", s(&v["email"]), s(&v["role"]), s(&v["id"]), s(&v["url"]));
                return Ok(());
            }
            v
        }
        Projects::Members(Members::SetRole { user, role, environments, project }) => {
            let p = project_id(api, project.as_deref()).await?;
            let u = user_id(api, &p, &user).await?;
            let v = api.call(reqwest::Method::PATCH, &format!("/v1/workspace/projects/{p}/members/{u}"), Some(json!({ "role": role, "environments": envs(environments) }))).await?;
            if !as_json {
                print_member(&v);
                return Ok(());
            }
            v
        }
        Projects::Members(Members::Remove { user, project }) => {
            let p = project_id(api, project.as_deref()).await?;
            let u = user_id(api, &p, &user).await?;
            api.call(reqwest::Method::DELETE, &format!("/v1/workspace/projects/{p}/members/{u}"), None).await?;
            if !as_json {
                println!("removed {user}");
                return Ok(());
            }
            json!({ "removed": u })
        }
        Projects::Leave { project } => {
            let p = project_id(api, project.as_deref()).await?;
            api.call(reqwest::Method::DELETE, &format!("/v1/workspace/projects/{p}/members/me"), None).await?;
            if !as_json {
                println!("left {p}");
                return Ok(());
            }
            json!({ "left": p })
        }
        Projects::Invites(ProjectInvites::List { project }) => {
            let p = project_id(api, project.as_deref()).await?;
            let v = api.get(&format!("/v1/workspace/projects/{p}/invites")).await?;
            if !as_json {
                for i in v.as_array().into_iter().flatten() {
                    println!("{:<18} {:<32} {:<10} {}", s(&i["id"]), s(&i["email"]), s(&i["role"]), ago(i["created_at"].as_i64().unwrap_or(0)));
                }
                return Ok(());
            }
            v
        }
        Projects::Invites(ProjectInvites::Revoke { id, project }) => {
            let p = project_id(api, project.as_deref()).await?;
            api.call(reqwest::Method::DELETE, &format!("/v1/workspace/projects/{p}/invites/{id}"), None).await?;
            if !as_json {
                println!("revoked {id}");
                return Ok(());
            }
            json!({ "revoked": id })
        }
        Projects::Roles => {
            let v = api.get("/v1/workspace/roles").await?;
            if !as_json {
                for r in v["roles"].as_array().into_iter().flatten() {
                    let what: Vec<String> = r["rules"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|rule| {
                            let actions: Vec<String> = rule["actions"].as_object().map(|a| a.keys().cloned().collect()).unwrap_or_default();
                            let only = match (&rule["environments"], rule["unprotected"].as_bool()) {
                                (Value::Array(e), _) => format!(" in {}", e.iter().map(s).collect::<Vec<_>>().join(",")),
                                (_, Some(true)) => " where not protected".into(),
                                _ => String::new(),
                            };
                            format!("{}: {}{only}", s(&rule["product"]), actions.join(" "))
                        })
                        .collect();
                    println!("{:<10} {}", s(&r["role"]), what.join("; "));
                }
                return Ok(());
            }
            v
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

pub async fn invites(api: &Api, as_json: bool, cmd: Invites) -> anyhow::Result<()> {
    let (url, what) = match &cmd {
        Invites::List => (invites_page(api, None), "your invites"),
        Invites::Accept { token } => (invites_page(api, Some(token)), "the invite (accepting it takes signing in as its address)"),
        Invites::Decline { token } => (invites_page(api, Some(token)), "the invite, to decline it"),
    };
    if as_json {
        println!("{}", json!({ "url": url }));
    } else {
        println!("Open {what}:\n{url}");
    }
    Ok(())
}
