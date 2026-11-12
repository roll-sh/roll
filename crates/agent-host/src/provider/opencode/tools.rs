//! OpenCode tools → roll tool calls, by name (T3's `toolItems.ts`): shell → command, edit /
//! write / patch → file change, web / codesearch → web search, glob / grep / search / lsp →
//! search, `read` with OpenCode's `<path>`/`<content>` envelope taken off, `todowrite` → a todo
//! list, roll's own MCP tools → MCP.

use agent_proto::{ChangeKind, FileChange, NodeBody, Todo, TodoStatus, ToolCall, ToolKind};
use serde_json::Value;

/// Tool output is capped at this many bytes with the middle cut (agent-proto `ToolCall`).
pub const OUTPUT_CAP: usize = 64 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Command,
    Edit,
    Web,
    Read,
    Search,
    Other,
}

/// T3's `openCodeToolProjectionKind`.
fn kind(name: &str) -> Kind {
    let n = name.to_lowercase();
    if n == "todowrite" {
        Kind::Other
    } else if n.contains("bash") || n.contains("shell") {
        Kind::Command
    } else if n.contains("edit") || n.contains("write") || n.contains("patch") {
        Kind::Edit
    } else if n.contains("web") || n == "codesearch" || n == "code_search" {
        Kind::Web
    } else if n == "read" {
        Kind::Read
    } else if n.contains("glob") || n.contains("grep") || n.contains("search") || n.contains("lsp") {
        Kind::Search
    } else {
        Kind::Other
    }
}

fn field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().filter_map(|k| v.get(*k).and_then(Value::as_str)).find(|s| !s.trim().is_empty())
}

/// What a tool item shows. `metadata` is the completed call's (`exit`, `diff`); `cwd` makes
/// paths relative.
pub fn body(name: &str, input: &Value, output: Option<&str>, metadata: Option<&Value>, mcp_prefix: Option<&str>, cwd: &str) -> NodeBody {
    if name.eq_ignore_ascii_case("todowrite")
        && let Some(todos) = todos(input)
    {
        return NodeBody::Todos { items: todos };
    }
    let none = Value::Null;
    let meta = metadata.unwrap_or(&none);
    let output = output.map(|o| if name == "read" { strip_read(o) } else { o.to_string() });
    let mut call = ToolCall { name: name.into(), kind: ToolKind::Other, title: name.into(), input: input.clone(), output: None, exit_code: None, files: Vec::new() };
    match kind(name) {
        Kind::Command => {
            call.kind = ToolKind::Command;
            call.title = field(input, &["command", "cmd"]).map(one_line).unwrap_or_else(|| name.into());
            call.exit_code = ["exit", "exitCode"].iter().find_map(|k| meta.get(*k).and_then(Value::as_i64)).map(|c| c as i32);
        }
        Kind::Edit => {
            call.kind = ToolKind::Edit;
            if let Some(path) = field(input, &["filePath", "path", "file"]) {
                let path = relative(path, cwd);
                call.title = path.clone();
                let diff = field(meta, &["diff", "patch"]).map(str::to_string);
                let change = if name.eq_ignore_ascii_case("write") && diff.is_none() { ChangeKind::Add } else { ChangeKind::Modify };
                call.files.push(FileChange { path, change, diff });
            }
        }
        Kind::Web => {
            call.kind = ToolKind::Web;
            call.title = field(input, &["query", "url", "pattern"]).map(one_line).unwrap_or_else(|| name.into());
        }
        Kind::Read => {
            call.kind = ToolKind::Read;
            if let Some(path) = field(input, &["filePath", "path", "file"]) {
                call.title = relative(path, cwd);
            }
        }
        Kind::Search => {
            call.kind = ToolKind::Search;
            call.title = field(input, &["pattern", "query", "path", "filePath"]).map(|p| one_line(&relative(p, cwd))).unwrap_or_else(|| name.into());
        }
        Kind::Other => {
            if mcp_prefix.is_some_and(|p| name.starts_with(p)) {
                call.kind = ToolKind::Mcp;
            }
        }
    }
    call.output = output.filter(|o| !o.is_empty()).map(|o| cap_middle(&o));
    NodeBody::Tool(call)
}

/// `todowrite`'s `{todos: [{content, status}]}`.
fn todos(input: &Value) -> Option<Vec<Todo>> {
    let list = input.get("todos")?.as_array()?;
    Some(
        list.iter()
            .filter_map(|t| {
                let text = t.get("content").and_then(Value::as_str)?.to_string();
                let status = match t.get("status").and_then(Value::as_str).unwrap_or("") {
                    "completed" | "cancelled" => TodoStatus::Completed,
                    "in_progress" => TodoStatus::InProgress,
                    _ => TodoStatus::Pending,
                };
                Some(Todo { text, status })
            })
            .collect(),
    )
}

/// T3's `nativeReadOutput`: `<path>…</path>\n<type>file|directory</type>\n` then the body in
/// `<content>` or `<entries>`; anything after the envelope stays.
pub fn strip_read(output: &str) -> String {
    let Some(rest) = output.strip_prefix("<path>") else { return output.to_string() };
    let Some((path, rest)) = rest.split_once("</path>") else { return output.to_string() };
    if path.contains('\n') {
        return output.to_string();
    }
    let rest = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix('\n')).unwrap_or(rest);
    let (open, close, rest) = if let Some(r) = rest.strip_prefix("<type>file</type>") {
        ("<content>", "</content>", r)
    } else if let Some(r) = rest.strip_prefix("<type>directory</type>") {
        ("<entries>", "</entries>", r)
    } else {
        return output.to_string();
    };
    let rest = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix('\n')).unwrap_or(rest);
    let Some(body) = rest.strip_prefix(open) else { return output.to_string() };
    let body = body.strip_prefix("\r\n").or_else(|| body.strip_prefix('\n')).unwrap_or(body);
    // Only a closing tag on a line of its own ends the envelope (file lines are numbered).
    let mut at = 0;
    while let Some(i) = body[at..].find(close) {
        let i = at + i;
        let before = &body[..i];
        let after = &body[i + close.len()..];
        if (before.ends_with('\n') || before.is_empty()) && (after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n")) {
            let inner = before.strip_suffix('\n').map(|b| b.strip_suffix('\r').unwrap_or(b)).unwrap_or(before);
            return format!("{inner}{after}");
        }
        at = i + close.len();
    }
    output.to_string()
}

pub fn relative(path: &str, cwd: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    match path.strip_prefix(cwd).and_then(|p| p.strip_prefix('/')) {
        Some(rest) if !rest.is_empty() && !cwd.is_empty() => rest.to_string(),
        _ => path.to_string(),
    }
}

fn one_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let mut out: String = line.chars().take(200).collect();
    if out.len() < line.len() {
        out.push('…');
    }
    out
}

/// Keep the head and tail of a long output.
pub fn cap_middle(text: &str) -> String {
    if text.len() <= OUTPUT_CAP {
        return text.to_string();
    }
    let half = OUTPUT_CAP / 2;
    let mut head = half;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - half;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n… {} bytes cut …\n{}", &text[..head], tail - head, &text[tail..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(b: NodeBody) -> ToolCall {
        match b {
            NodeBody::Tool(t) => t,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tools_map_by_name() {
        let t = tool(body("shell", &json!({"command": "sleep 1\necho"}), Some("A\n"), Some(&json!({"exit": 2})), None, "/w"));
        assert_eq!((t.kind, t.title.as_str(), t.exit_code), (ToolKind::Command, "sleep 1", Some(2)));
        let t = tool(body("edit", &json!({"filePath": "/w/src/a.rs"}), None, Some(&json!({"diff": "@@"})), None, "/w"));
        assert_eq!((t.kind, t.title.as_str()), (ToolKind::Edit, "src/a.rs"));
        assert_eq!(t.files, vec![FileChange { path: "src/a.rs".into(), change: ChangeKind::Modify, diff: Some("@@".into()) }]);
        assert_eq!(tool(body("glob", &json!({"pattern": "*"}), None, None, None, "/w")).kind, ToolKind::Search);
        assert_eq!(tool(body("webfetch", &json!({"url": "https://x"}), None, None, None, "/w")).title, "https://x");
        assert_eq!(tool(body("roll-1-roll_land", &json!({}), None, None, Some("roll-1-"), "/w")).kind, ToolKind::Mcp);
        let todo = body("todowrite", &json!({"todos": [{"content": "a", "status": "in_progress"}]}), None, None, None, "/w");
        assert_eq!(todo, NodeBody::Todos { items: vec![Todo { text: "a".into(), status: TodoStatus::InProgress }] });
    }

    #[test]
    fn read_envelopes_come_off() {
        assert_eq!(strip_read("<path>/w/a</path>\n<type>file</type>\n<content>\n1: x\n</content>\n(more)"), "1: x\n(more)");
        assert_eq!(strip_read("<path>/w</path>\n<type>directory</type>\n<entries>\na\nb\n</entries>"), "a\nb");
        assert_eq!(strip_read("Read directory /w"), "Read directory /w");
    }
}
