//! Skills over MCP (`io.modelcontextprotocol/skills`, SEP-2640; docs/mcp.md, "Skills"): roll's
//! workflows and its guide as Agent Skills a host discovers (`skills/list`, `skills/get`) and
//! reads as resources (`skill://roll/<name>/SKILL.md`). Each is one static file from roll's own
//! binary, never tenant text: instructions that steer an agent through roll's tools, each still
//! decided by the gate. A connection gets the skills whose tools it has, as with prompts.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const EXT: &str = "io.modelcontextprotocol/skills";
const PREFIX: &str = "skill://roll/";
/// The listing follows the connection's tools, as `tools/list` does.
const TTL_MS: u64 = 60_000;

/// One skill: its URI and its SKILL.md.
struct Skill {
    uri: String,
    name: String,
    description: String,
    text: String,
}

/// A SKILL.md: YAML frontmatter (`name`, then `description` as a JSON string, which YAML reads
/// as the same text), then the body.
fn skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {}\n---\n\n{}\n", Value::String(description.into()), body.trim_end())
}

fn skill(name: String, description: String, body: &str) -> Skill {
    Skill { uri: format!("{PREFIX}{name}/SKILL.md"), text: skill_md(&name, &description, body), name, description }
}

/// The skills a connection with `tools` is offered: the guide's, then a workflow's each.
fn offered(tools: &[String]) -> Vec<Skill> {
    let mut out = vec![skill(
        "using-roll".into(),
        "How to use roll over MCP: ids, permissions, approvals and what to do when a call is refused. Use it before acting in roll.".into(),
        include_str!("guide.md"),
    )];
    out.push(skill(
        "set-up-ci".into(),
        "How to set up a repository's CI (GitHub Actions, GitLab CI) to deploy, migrate or read through roll with CI's own identity, nothing stored in CI. Use when the user wants CI to use roll.".into(),
        include_str!("skills/set-up-ci.md"),
    ));
    for crate::prompts::Workflow { name, title, description, text, args, tools: needs } in crate::prompts::workflows(tools) {
        let mut body = format!("# {title}\n\n{text}\n");
        if !args.is_empty() {
            body.push_str("\n## What to ask the user for\n\n");
            for (arg, what) in args {
                body.push_str(&format!("- `<{arg}>`: {what}\n"));
            }
        }
        body.push_str("\n## roll tools it uses\n\n");
        for t in needs {
            body.push_str(&format!("- `{t}`\n"));
        }
        body.push_str("\nroll's gate decides each call as it would any other: this skill grants nothing.\n");
        out.push(skill(name.replace('_', "-"), format!("{description} For: {title}"), &body));
    }
    out
}

fn entry(s: &Skill) -> Value {
    let digest = format!("sha256:{:x}", Sha256::digest(s.text.as_bytes()));
    json!({
        "uri": s.uri,
        "frontmatter": { "name": s.name, "description": s.description },
        "resources": [{ "uri": s.uri, "digest": digest, "size": s.text.len() }],
    })
}

/// `skills/list`: every skill this connection is offered, in one page.
pub fn list(tools: &[String]) -> Value {
    json!({ "skills": offered(tools).iter().map(entry).collect::<Vec<_>>(), "ttlMs": TTL_MS, "cacheScope": "private" })
}

/// `skills/get`: the entry for a SKILL.md URI this connection is offered, or why not.
pub fn get(uri: &str, tools: &[String]) -> Result<Value, String> {
    let s = offered(tools).into_iter().find(|s| s.uri == uri).ok_or_else(|| format!("No skill is served at {uri}"))?;
    Ok(json!({ "skill": entry(&s), "ttlMs": TTL_MS, "cacheScope": "private" }))
}

/// `resources/read` of a skill's file, when this connection is offered it.
pub fn read(uri: &str, tools: &[String]) -> Option<Value> {
    if !uri.starts_with(PREFIX) {
        return None;
    }
    let s = offered(tools).into_iter().find(|s| s.uri == uri)?;
    Some(json!({ "contents": [{ "uri": s.uri, "mimeType": "text/markdown", "text": s.text }] }))
}

/// The SKILL.md resources, for `resources/list`.
pub fn resources(tools: &[String]) -> Vec<Value> {
    offered(tools).iter().map(|s| json!({ "uri": s.uri, "name": s.name, "description": s.description, "mimeType": "text/markdown" })).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_skill_follows_the_agent_skills_rules() {
        let all: Vec<String> = crate::tools::all().iter().map(|t| t.definition()["name"].as_str().unwrap().to_string()).collect();
        let skills = offered(&all);
        assert!(skills.len() > 3);
        for s in &skills {
            // A name is lowercase letters, digits and hyphens, at most 64, and the URI's last segment.
            assert!(s.name.len() <= 64 && s.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'), "{}", s.name);
            assert!(!s.name.starts_with('-') && !s.name.ends_with('-') && !s.name.contains("--"), "{}", s.name);
            assert_eq!(s.uri, format!("skill://roll/{}/SKILL.md", s.name));
            assert!(!s.description.is_empty() && s.description.len() <= 1024, "{}", s.name);
            assert!(s.text.starts_with(&format!("---\nname: {}\ndescription: ", s.name)), "{}", s.text);
        }
    }
}
