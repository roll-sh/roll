//! MCP prompts: roll's workflows as reusable messages a client offers as slash commands
//! (`prompts/list`, `prompts/get`). A prompt is only text that steers the agent through roll's
//! tools: it calls nothing, and each tool it names is still decided by the gate. A connection is
//! offered only the prompts whose tools it has.

use serde_json::{Value, json};

struct Arg {
    name: &'static str,
    description: &'static str,
    required: bool,
}

/// A prompt's arguments by name, cleaned.
type Args<'a> = &'a dyn Fn(&str) -> String;

struct Prompt {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    args: &'static [Arg],
    /// Tools the workflow uses: offered only when the connection has every one.
    needs: &'static [&'static str],
    /// The message, from the arguments (each cleaned and bounded).
    text: fn(Args<'_>) -> String,
}

const PROMPTS: &[Prompt] = &[
    Prompt {
        name: "what_can_i_do",
        title: "What can this connection do?",
        description: "What roll lets this connection do, in words, and how to ask for more.",
        args: &[],
        needs: &["access_whoami"],
        text: |_| {
            "Read the resource roll://access/me (or call access_whoami) and tell me, in plain words, what this \
             connection may do in roll: which products and resources, which actions, what asks a person first, \
             and when it expires. If something I'll likely need is missing, say which rule would allow it, so \
             I can ask whoever manages my tokens."
                .into()
        },
    },
    Prompt {
        name: "check_diff_for_secrets",
        title: "Check a diff for secrets",
        description: "Before committing: whether any value in a diff is one of a project's stored secrets.",
        args: &[Arg { name: "project", description: "Workspace project whose secrets to check against", required: true }],
        needs: &["secrets_scan"],
        text: |a| {
            format!(
                "Before I commit, check my staged diff for secrets. Take every added line that looks like a value \
                 (assignments, connection strings, keys, tokens) and call secrets_scan for project {} with those \
                 values. For each match, name the file and line, the secret's key, environment and folder (never \
                 the value), and suggest reading it from roll secrets at runtime instead. If nothing matches, say \
                 so plainly. Don't print any value you found.",
                a("project")
            )
        },
    },
    Prompt {
        name: "fix_secrets_health",
        title: "Fix a project's secrets health",
        description: "What needs fixing in a project's secrets, most urgent first, with the fix for each.",
        args: &[Arg { name: "project", description: "Workspace project", required: true }],
        needs: &["secrets_health"],
        text: |a| {
            format!(
                "Call secrets_health for project {}. List what needs fixing, most urgent first (exposed, expired, \
                 missing, then weak, expiring, stale, unread), each with its key, environment and the fix. Ask me \
                 before changing or rotating anything, and never show a value.",
                a("project")
            )
        },
    },
    Prompt {
        name: "debug_failing_service",
        title: "Debug a failing service",
        description: "Why an app service's environment is failing: its last deploys, errors and metrics, then a fix or a rollback.",
        args: &[
            Arg { name: "service", description: "App service id", required: true },
            Arg { name: "env", description: "Environment (production, staging, …)", required: true },
        ],
        needs: &["apps_list_deploys", "apps_deploy_logs", "apps_logs", "apps_metrics"],
        text: |a| {
            format!(
                "Service {} in {} is failing. Call apps_list_deploys to see what changed and when; apps_deploy_logs \
                 for the newest deploy if it failed; apps_logs with http and a status of 500 for the failing \
                 requests, and without http for errors; apps_metrics for CPU and memory. Tell me the most likely \
                 cause with the evidence. If the newest deploy caused it, offer apps_rollback_previous; it puts back \
                 that deploy's variables too, and in production it may ask a person first. Don't roll back without \
                 asking me.",
                a("service"),
                a("env")
            )
        },
    },
];

/// An argument as a prompt shows it: one line of printable text, at most 200 characters.
fn clean(v: Option<&Value>) -> String {
    let s = v.and_then(Value::as_str).unwrap_or("");
    s.chars().filter(|c| !c.is_control()).take(200).collect()
}

fn offered<'a>(tools: &'a [String]) -> impl Iterator<Item = &'static Prompt> + 'a {
    PROMPTS.iter().filter(move |p| p.needs.iter().all(|n| tools.iter().any(|t| t == n)))
}

/// `prompts/list`, for a connection with `tools`.
pub fn list(tools: &[String]) -> Value {
    let prompts: Vec<Value> = offered(tools)
        .map(|p| {
            let args: Vec<Value> = p.args.iter().map(|a| json!({ "name": a.name, "description": a.description, "required": a.required })).collect();
            json!({ "name": p.name, "title": p.title, "description": p.description, "arguments": args })
        })
        .collect();
    json!({ "prompts": prompts })
}

/// `prompts/get`: the prompt's message with `args`, or why not (an unknown prompt, one this
/// connection isn't offered, or a required argument missing).
pub fn get(name: &str, args: &Value, tools: &[String]) -> Result<Value, String> {
    let p = offered(tools).find(|p| p.name == name).ok_or_else(|| format!("unknown prompt {name:?}"))?;
    if let Some(missing) = p.args.iter().find(|a| a.required && clean(args.get(a.name)).trim().is_empty()) {
        return Err(format!("{} needs {}", p.name, missing.name));
    }
    let arg = |k: &str| clean(args.get(k));
    Ok(json!({
        "description": p.description,
        "messages": [{ "role": "user", "content": { "type": "text", "text": (p.text)(&arg) } }],
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// Offered only with their tools, every named tool is one roll has, and arguments are bounded.
    #[test]
    fn prompts_follow_the_connections_tools() {
        let all: Vec<String> = crate::tools::all().iter().map(|t| t.definition()["name"].as_str().unwrap().to_string()).collect();
        for p in super::PROMPTS {
            for n in p.needs {
                assert!(all.iter().any(|t| t == n), "{} needs {n}, which isn't a tool", p.name);
            }
        }
        let some = vec!["access_whoami".to_string()];
        let listed = super::list(&some);
        assert_eq!(listed["prompts"].as_array().unwrap().len(), 1, "{listed}");
        assert!(super::get("fix_secrets_health", &json!({ "project": "p" }), &some).is_err(), "not offered without secrets_health");
        assert!(super::get("fix_secrets_health", &json!({}), &all).is_err(), "project is required");
        let got = super::get("fix_secrets_health", &json!({ "project": format!("prj_a\n{}", "x".repeat(500)) }), &all).unwrap();
        let text = got["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("prj_a") && !text.contains('\n') && text.len() < 800, "{text}");
    }
}
