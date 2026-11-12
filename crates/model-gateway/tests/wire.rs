//! The translation layer through its public API: SSE parsing, request vectors in every
//! direction, whole responses, streams fed as recorded upstream bytes (modelled on the
//! providers' documented streams and CLIProxyAPI's fixtures), and usage.

use agent_proto::WireFormat::{self, Chat, Messages, Responses};
use model_gateway::Usage;
use model_gateway::wire::sse::{Event, Parser};
use model_gateway::wire::{self, StreamTranslator, UsageTap, WireError};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Helpers.

/// Parses `raw` SSE bytes fed in chunks of `size` bytes.
fn parse(raw: &[u8], size: usize) -> Vec<Event> {
    let mut p = Parser::new(1 << 20);
    let mut out = Vec::new();
    for chunk in raw.chunks(size.max(1)) {
        out.extend(p.feed(chunk).expect("parse"));
    }
    out.extend(p.finish());
    out
}

/// An upstream stream (SSE text) through a translator, fed in awkward chunk sizes; the
/// client's events with their data parsed (`[DONE]` as a string).
fn translate_stream(from: WireFormat, to: WireFormat, model: &str, client_request: Option<&Value>, raw: &str) -> (Vec<(Option<String>, Value)>, Usage) {
    let mut results = Vec::new();
    for size in [7, 1, raw.len()] {
        let mut t = StreamTranslator::new(from, to, model);
        if let Some(r) = client_request {
            t = t.with_request(r);
        }
        let mut out = Vec::new();
        for e in parse(raw.as_bytes(), size) {
            out.extend(t.push(&e).expect("translate"));
        }
        out.extend(t.finish());
        let parsed: Vec<(Option<String>, Value)> =
            out.iter().map(|e| (e.event.clone(), serde_json::from_str(&e.data).unwrap_or_else(|_| Value::String(e.data.clone())))).collect();
        results.push((parsed, t.usage()));
    }
    // However the bytes were split, the same events (ids aside) come out.
    let shape = |v: &Vec<(Option<String>, Value)>| v.iter().map(|(e, d)| (e.clone(), d.get("type").cloned(), d.get("delta").cloned())).collect::<Vec<_>>();
    assert_eq!(shape(&results[0].0), shape(&results[1].0));
    assert_eq!(shape(&results[0].0), shape(&results[2].0));
    results.pop().expect("three runs")
}

fn sse(events: &[(Option<&str>, Value)]) -> String {
    let mut s = String::new();
    for (name, data) in events {
        if let Some(n) = name {
            s.push_str(&format!("event: {n}\n"));
        }
        s.push_str(&format!("data: {data}\n\n"));
    }
    s
}

fn names(events: &[(Option<String>, Value)]) -> Vec<String> {
    events.iter().map(|(e, d)| e.clone().or_else(|| d.get("type").and_then(Value::as_str).map(str::to_string)).unwrap_or_default()).collect()
}

/// Removes keys whose values are random or the clock's.
fn strip(mut v: Value, keys: &[&str]) -> Value {
    fn walk(v: &mut Value, keys: &[&str]) {
        match v {
            Value::Object(o) => {
                for k in keys {
                    o.remove(*k);
                }
                for (_, c) in o.iter_mut() {
                    walk(c, keys);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|c| walk(c, keys)),
            _ => {}
        }
    }
    walk(&mut v, keys);
    v
}

fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({ "id": "chatcmpl-abc", "object": "chat.completion.chunk", "created": 1, "model": "gpt-5", "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
}

// ---------------------------------------------------------------------------------------------
// SSE.

#[test]
fn sse_parses_fields_comments_and_multiline_data() {
    let raw = b": a comment\nevent: message_start\nid: 7\nretry: 1000\ndata: {\"a\":\ndata:1}\n\ndata:no space\n\nunknown: x\ndata\n\n";
    let events = parse(raw, 1000);
    assert_eq!(
        events,
        vec![Event::named("message_start", "{\"a\":\n1}"), Event::data("no space"), Event::data("")],
        "multiple data lines join with \\n, one space after the colon is dropped, a bare `data` is empty"
    );
    // What the gateway writes parses back the same.
    for e in [Event::named("content_block_delta", "{\"x\":1}"), Event::data("a\nb"), Event::data("[DONE]")] {
        assert_eq!(parse(&e.encode(), 3), vec![e]);
    }
}

#[test]
fn sse_handles_every_line_ending_and_any_chunk_boundary() {
    let lf = "event: a\ndata: héllo 🦀\n\ndata: second\n\n";
    let crlf = lf.replace('\n', "\r\n");
    let cr = lf.replace('\n', "\r");
    let want = vec![Event::named("a", "héllo 🦀"), Event::data("second")];
    for raw in [lf.to_string(), crlf, cr] {
        for size in 1..=raw.len() {
            assert_eq!(parse(raw.as_bytes(), size), want, "{raw:?} in chunks of {size}");
        }
    }
}

#[test]
fn sse_refuses_an_event_over_the_limit() {
    let mut p = Parser::new(64);
    assert!(p.feed(b"data: small\n\n").unwrap().len() == 1);
    // One long line with no end in sight.
    let err = p.feed(&[b'x'; 100]).unwrap_err();
    assert!(matches!(err, WireError::TooLarge(_)), "{err:?}");
    // Many short data lines that add up.
    let mut p = Parser::new(64);
    let mut lines = Vec::new();
    for _ in 0..20 {
        lines.extend_from_slice(b"data: 12345\n");
    }
    assert!(matches!(p.feed(&lines), Err(WireError::TooLarge(_))));
}

#[test]
fn sse_finish_returns_an_unterminated_event() {
    let mut p = Parser::new(1024);
    assert!(p.feed(b"event: e\ndata: last").unwrap().is_empty());
    assert_eq!(p.finish(), Some(Event::named("e", "last")));
    assert_eq!(p.finish(), None);
    let mut p = Parser::new(1024);
    p.feed(b"\xEF\xBB\xBFdata: bom\n\n").map(|e| assert_eq!(e, vec![Event::data("bom")])).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Requests.

fn claude_code_request() -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 32000,
        "stream": true,
        "system": [{ "type": "text", "text": "You are Claude Code.", "cache_control": { "type": "ephemeral" } }],
        "metadata": { "user_id": "user_1" },
        "thinking": { "type": "enabled", "budget_tokens": 10000 },
        "tools": [
            { "name": "Read", "description": "Reads a file", "input_schema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } },
            { "type": "web_search_20250305", "name": "web_search" }
        ],
        "tool_choice": { "type": "auto", "disable_parallel_tool_use": true },
        "stop_sequences": ["END"],
        "messages": [
            { "role": "user", "content": [
                { "type": "text", "text": "What's in a.txt?" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0K" } }
            ]},
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "I should read it.", "signature": "sig-anthropic" },
                { "type": "text", "text": "Reading." },
                { "type": "tool_use", "id": "toolu_1", "name": "Read", "input": { "path": "a.txt" } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": [{ "type": "text", "text": "hello" }] },
                { "type": "text", "text": "Thanks" }
            ]}
        ]
    })
}

#[test]
fn messages_to_chat_request() {
    let out = wire::translate_request(Messages, Chat, claude_code_request(), "gpt-4.1").unwrap();
    assert_eq!(
        out,
        json!({
            "model": "gpt-4.1",
            "messages": [
                { "role": "system", "content": "You are Claude Code." },
                { "role": "user", "content": [
                    { "type": "text", "text": "What's in a.txt?" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0K" } }
                ]},
                { "role": "assistant", "content": "Reading.", "tool_calls": [
                    { "id": "toolu_1", "type": "function", "function": { "name": "Read", "arguments": "{\"path\":\"a.txt\"}" } }
                ]},
                { "role": "tool", "tool_call_id": "toolu_1", "content": "hello" },
                { "role": "user", "content": "Thanks" }
            ],
            "tools": [{ "type": "function", "function": {
                "name": "Read", "description": "Reads a file",
                "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }
            }}],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "max_tokens": 32000,
            "stop": ["END"],
            "stream": true,
            "stream_options": { "include_usage": true },
            "user": "user_1"
        }),
        "GPT-4.1 takes max_tokens and no reasoning effort; the web search server tool is dropped"
    );
    let out = wire::translate_request(Messages, Chat, claude_code_request(), "gpt-5").unwrap();
    assert_eq!(out["max_completion_tokens"], 32000);
    assert_eq!(out["reasoning_effort"], "high", "a 10000-token budget is high effort");
    assert!(out.get("max_tokens").is_none() && out.get("temperature").is_none());
}

#[test]
fn messages_to_responses_request() {
    let mut req = claude_code_request();
    // A turn that an OpenAI upstream answered earlier: its reasoning came back to Claude Code
    // as a prefixed signature.
    req["messages"][1]["content"][0]["signature"] = json!("roll-oai:rs_9:gAAAA-encrypted");
    let out = wire::translate_request(Messages, Responses, req, "gpt-5-codex").unwrap();
    assert_eq!(
        out,
        json!({
            "model": "gpt-5-codex",
            "instructions": "You are Claude Code.",
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "What's in a.txt?" },
                    { "type": "input_image", "image_url": "data:image/png;base64,iVBORw0K", "detail": "auto" }
                ]},
                { "type": "reasoning", "id": "rs_9", "summary": [{ "type": "summary_text", "text": "I should read it." }], "encrypted_content": "gAAAA-encrypted" },
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Reading.", "annotations": [] }] },
                { "type": "function_call", "call_id": "toolu_1", "name": "Read", "arguments": "{\"path\":\"a.txt\"}" },
                { "type": "function_call_output", "call_id": "toolu_1", "output": "hello" },
                { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "Thanks" }] }
            ],
            "tools": [{ "type": "function", "name": "Read", "description": "Reads a file", "strict": false,
                "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } }],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "max_output_tokens": 32000,
            "stream": true,
            "store": false,
            "user": "user_1",
            "reasoning": { "effort": "high", "summary": "auto" },
            "include": ["reasoning.encrypted_content"]
        })
    );
    // An Anthropic signature means nothing to OpenAI: that reasoning stays behind.
    let out = wire::translate_request(Messages, Responses, claude_code_request(), "gpt-5").unwrap();
    assert!(!out["input"].as_array().unwrap().iter().any(|i| i["type"] == "reasoning"));
}

fn chat_request() -> Value {
    json!({
        "model": "my-claude",
        "stream": true,
        "stream_options": { "include_usage": true },
        "temperature": 0.2,
        "reasoning_effort": "high",
        "messages": [
            { "role": "system", "content": "Be brief." },
            { "role": "developer", "content": [{ "type": "text", "text": "Use tools." }] },
            { "role": "user", "content": [
                { "type": "text", "text": "Look:" },
                { "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,/9j/4AAQ" } },
                { "type": "image_url", "image_url": { "url": "https://example.com/cat.png" } }
            ]},
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "call_a", "type": "function", "function": { "name": "ls", "arguments": "{\"dir\":\".\"}" } },
                { "id": "call.b", "type": "function", "function": { "name": "cat", "arguments": "{\"f\":\"x\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "call_a", "content": "x" },
            { "role": "tool", "tool_call_id": "call.b", "content": [{ "type": "text", "text": "data" }] }
        ],
        "tools": [
            { "type": "function", "function": { "name": "ls", "parameters": { "type": "object", "properties": { "dir": { "type": "string" } } } } },
            { "type": "function", "function": { "name": "cat", "description": "Print", "parameters": { "type": "object" } } }
        ],
        "tool_choice": "required",
        "max_tokens": 4096,
        "stop": "STOP"
    })
}

#[test]
fn chat_to_messages_request_on_an_older_claude() {
    let out = wire::translate_request(Chat, Messages, chat_request(), "claude-sonnet-4-5").unwrap();
    assert_eq!(
        out,
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 4096,
            "system": [
                { "type": "text", "text": "Be brief." },
                { "type": "text", "text": "Use tools.", "cache_control": { "type": "ephemeral" } }
            ],
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "Look:" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "/9j/4AAQ" } },
                    { "type": "image", "source": { "type": "url", "url": "https://example.com/cat.png" } }
                ]},
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "call_a", "name": "ls", "input": { "dir": "." } },
                    { "type": "tool_use", "id": "call_b", "name": "cat", "input": { "f": "x" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "call_a", "content": [{ "type": "text", "text": "x" }] },
                    { "type": "tool_result", "tool_use_id": "call_b", "content": [{ "type": "text", "text": "data" }], "cache_control": { "type": "ephemeral" } }
                ]}
            ],
            "tools": [
                { "name": "ls", "input_schema": { "type": "object", "properties": { "dir": { "type": "string" } } } },
                { "name": "cat", "description": "Print", "input_schema": { "type": "object" } }
            ],
            "tool_choice": { "type": "any" },
            "temperature": 0.2,
            "stop_sequences": ["STOP"],
            "stream": true
        }),
        "the last assistant turn called tools without a Claude thinking block, so thinking stays off (and sampling and \
         a forced choice with it); tool results share one user turn; ids get Anthropic's characters; caching marks \
         the system and the last turn"
    );

    // Without unsigned tool calls in the way, high effort is a thinking budget, and sampling
    // goes (thinking requires it).
    let mut req = chat_request();
    req["messages"].as_array_mut().unwrap().truncate(3);
    let out = wire::translate_request(Chat, Messages, req, "claude-sonnet-4-5").unwrap();
    assert_eq!(out["thinking"], json!({ "type": "enabled", "budget_tokens": 3072 }), "capped below max_tokens");
    assert_eq!(out["tool_choice"], json!({ "type": "auto" }), "thinking allows no forced tool choice");
    assert!(out.get("temperature").is_none());
}

#[test]
fn chat_to_messages_request_follows_each_claude_generation() {
    let mut req = chat_request();
    req["messages"].as_array_mut().unwrap().truncate(3);

    let out = wire::translate_request(Chat, Messages, req.clone(), "claude-opus-4-6").unwrap();
    assert_eq!(out["thinking"], json!({ "type": "adaptive" }));
    assert_eq!(out["output_config"], json!({ "effort": "high" }));
    assert!(out.get("temperature").is_none());

    let mut xhigh = req.clone();
    xhigh["reasoning_effort"] = json!("xhigh");
    let out = wire::translate_request(Chat, Messages, xhigh.clone(), "claude-opus-4-6").unwrap();
    assert_eq!(out["output_config"]["effort"], "high", "4.6 has no xhigh");
    let out = wire::translate_request(Chat, Messages, xhigh, "us.anthropic.claude-opus-4-8-v1:0").unwrap();
    assert_eq!(out["output_config"]["effort"], "xhigh");

    let mut off = req.clone();
    off["reasoning_effort"] = json!("none");
    off.as_object_mut().unwrap().remove("temperature");
    let out = wire::translate_request(Chat, Messages, off.clone(), "claude-opus-4-7").unwrap();
    assert_eq!(out["thinking"], json!({ "type": "disabled" }));
    assert_eq!(out["tool_choice"], json!({ "type": "any" }), "required is `any` where forced choices are allowed");
    let out = wire::translate_request(Chat, Messages, off, "claude-opus-5-5").unwrap();
    assert!(out.get("thinking").is_none(), "Opus 5.5 can't turn thinking off: lowest effort instead");
    assert_eq!(out["output_config"], json!({ "effort": "low" }));
    assert_eq!(out["tool_choice"], json!({ "type": "auto" }), "Opus 5.5 refuses forced tool choice");

    // No max_tokens from the client: Messages needs one.
    let mut bare = req.clone();
    bare.as_object_mut().unwrap().remove("max_tokens");
    bare.as_object_mut().unwrap().remove("reasoning_effort");
    let out = wire::translate_request(Chat, Messages, bare.clone(), "claude-haiku-4-5").unwrap();
    assert_eq!(out["max_tokens"], 32000);
    assert_eq!(out["temperature"], 0.2, "sampling stays for models that take it");
    let out = wire::translate_request(Chat, Messages, bare, "deepseek-chat").unwrap();
    assert_eq!(out["max_tokens"], 8192, "an Anthropic-compatible provider's own model");
}

fn codex_request() -> Value {
    json!({
        "model": "gpt-5-codex",
        "instructions": "You are Codex.",
        "stream": true,
        "store": false,
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "thread-1",
        "reasoning": { "effort": "medium", "summary": "auto" },
        "tools": [
            { "type": "function", "name": "shell", "description": "Runs a command", "strict": false,
              "parameters": { "type": "object", "properties": { "command": { "type": "array", "items": { "type": "string" } } }, "required": ["command"] } },
            { "type": "custom", "name": "apply_patch", "description": "Edit files",
              "format": { "type": "grammar", "syntax": "lark", "definition": "start: patch" } },
            { "type": "namespace", "name": "mcp__docs", "tools": [
                { "type": "function", "name": "search", "parameters": { "type": "object", "properties": { "q": { "type": "string" } } } }
            ]},
            { "type": "web_search" }
        ],
        "input": [
            { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "Sandbox: workspace-write" }] },
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "Fix the bug" }] },
            { "type": "reasoning", "id": "rs_1", "summary": [{ "type": "summary_text", "text": "Look first." }], "encrypted_content": "roll-ant:sig-1" },
            { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}" },
            { "type": "function_call_output", "call_id": "call_1", "output": "main.rs" },
            { "type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch" },
            { "type": "custom_tool_call_output", "call_id": "call_2", "output": "Done" },
            { "type": "function_call", "call_id": "call_3", "namespace": "mcp__docs", "name": "search", "arguments": "{\"q\":\"x\"}" },
            { "type": "function_call_output", "call_id": "call_3", "output": [{ "type": "input_text", "text": "found" }] },
            { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Fixed." }] },
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "Thanks" }] }
        ]
    })
}

#[test]
fn responses_to_messages_request() {
    let out = wire::translate_request(Responses, Messages, codex_request(), "claude-opus-4-8").unwrap();
    let desc = "The tool's raw input text (not JSON). It must match this lark grammar:\nstart: patch";
    assert_eq!(
        out,
        json!({
            "model": "claude-opus-4-8",
            "max_tokens": 32000,
            "system": [
                { "type": "text", "text": "You are Codex." },
                { "type": "text", "text": "Sandbox: workspace-write", "cache_control": { "type": "ephemeral" } }
            ],
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "Fix the bug" }] },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "Look first.", "signature": "sig-1" },
                    { "type": "tool_use", "id": "call_1", "name": "shell", "input": { "command": ["ls"] } }
                ]},
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "call_1", "content": [{ "type": "text", "text": "main.rs" }] }] },
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "call_2", "name": "apply_patch", "input": { "input": "*** Begin Patch\n*** End Patch" } }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "call_2", "content": [{ "type": "text", "text": "Done" }] }] },
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "call_3", "name": "mcp__docs__search", "input": { "q": "x" } }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "call_3", "content": [{ "type": "text", "text": "found" }] }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "Fixed." }] },
                { "role": "user", "content": [{ "type": "text", "text": "Thanks", "cache_control": { "type": "ephemeral" } }] }
            ],
            "tools": [
                { "name": "shell", "description": "Runs a command",
                  "input_schema": { "type": "object", "properties": { "command": { "type": "array", "items": { "type": "string" } } }, "required": ["command"] } },
                { "name": "apply_patch", "description": "Edit files\n\nCall this tool with its raw input in the `input` field.",
                  "input_schema": { "type": "object", "properties": { "input": { "type": "string", "description": desc } }, "required": ["input"], "additionalProperties": false } },
                { "name": "mcp__docs__search", "input_schema": { "type": "object", "properties": { "q": { "type": "string" } } } }
            ],
            "thinking": { "type": "adaptive", "display": "summarized" },
            "output_config": { "effort": "medium" },
            "stream": true
        }),
        "developer text joins the system prompt; the Claude signature carried in encrypted_content comes back; \
         a freeform tool takes its input in a wrapper; a namespace's tools are flat names; web search is dropped"
    );
}

#[test]
fn responses_and_chat_translate_both_ways() {
    let out = wire::translate_request(Responses, Chat, codex_request(), "deepseek/deepseek-chat").unwrap();
    let messages = out["messages"].as_array().unwrap();
    assert_eq!(messages[0], json!({ "role": "system", "content": "You are Codex.\n\nSandbox: workspace-write" }));
    assert_eq!(messages[3], json!({ "role": "tool", "tool_call_id": "call_1", "content": "main.rs" }));
    assert_eq!(messages[4]["tool_calls"][0]["function"], json!({ "name": "apply_patch", "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}" }));
    assert_eq!(out["prompt_cache_key"], "thread-1");
    assert_eq!(out["reasoning_effort"], "medium");
    assert_eq!(out["parallel_tool_calls"], true);
    assert_eq!(out["stream_options"], json!({ "include_usage": true }));

    let out = wire::translate_request(Chat, Responses, chat_request(), "gpt-5").unwrap();
    assert_eq!(out["instructions"], "Be brief.\n\nUse tools.");
    assert_eq!(out["input"][1], json!({ "type": "function_call", "call_id": "call_a", "name": "ls", "arguments": "{\"dir\":\".\"}" }));
    assert_eq!(out["input"][3], json!({ "type": "function_call_output", "call_id": "call_a", "output": "x" }));
    assert_eq!(out["tool_choice"], "required");
    assert_eq!(out["max_output_tokens"], 4096);
    assert_eq!(out["reasoning"], json!({ "effort": "high", "summary": "auto" }));
    assert!(out.get("temperature").is_none(), "gpt-5 takes no sampling");
    assert!(out.get("stop").is_none(), "Responses has no stop sequences");
}

#[test]
fn same_format_only_changes_the_model() {
    for (format, body) in [(Messages, claude_code_request()), (Chat, chat_request()), (Responses, codex_request())] {
        let out = wire::translate_request(format, format, body.clone(), "upstream-model").unwrap();
        let mut want = body;
        want["model"] = json!("upstream-model");
        assert_eq!(out, want);
    }
}

#[test]
fn hosted_only_tools_and_bad_bodies_are_refused() {
    let req = json!({ "model": "x", "input": "search the web", "tools": [{ "type": "web_search" }] });
    assert!(matches!(wire::translate_request(Responses, Messages, req, "claude-opus-4-8"), Err(WireError::Unsupported(_))));
    assert!(matches!(wire::translate_request(Chat, Messages, json!([1]), "m"), Err(WireError::Invalid(_))));
    assert!(matches!(wire::translate_request(Messages, Chat, json!({ "model": "x" }), "m"), Err(WireError::Invalid(_))));
    let n2 = json!({ "model": "x", "n": 2, "messages": [] });
    assert!(matches!(wire::translate_request(Chat, Messages, n2, "m"), Err(WireError::Unsupported(_))));
    assert_eq!(wire::request_model(&chat_request()), Some("my-claude"));
    assert!(wire::is_stream(Chat, &chat_request()));
    assert!(!wire::is_stream(Messages, &json!({ "model": "x" })));
}

#[test]
fn json_output_formats_carry_over() {
    let mut req = chat_request();
    req["response_format"] = json!({ "type": "json_schema", "json_schema": { "name": "answer", "schema": { "type": "object" }, "strict": true } });
    let out = wire::translate_request(Chat, Responses, req.clone(), "gpt-5").unwrap();
    assert_eq!(out["text"], json!({ "format": { "type": "json_schema", "name": "answer", "schema": { "type": "object" }, "strict": true } }));
    let out = wire::translate_request(Chat, Messages, req, "claude-opus-4-8").unwrap();
    assert_eq!(out["output_config"]["format"], json!({ "type": "json_schema", "schema": { "type": "object" } }));
}

// ---------------------------------------------------------------------------------------------
// Whole responses.

fn claude_response() -> Value {
    json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-opus-4-8",
        "content": [
            { "type": "thinking", "thinking": "Hmm.", "signature": "sig-x" },
            { "type": "text", "text": "Calling." },
            { "type": "tool_use", "id": "toolu_9", "name": "ls", "input": { "dir": "." } }
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": { "input_tokens": 10, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 100, "output_tokens": 20 }
    })
}

#[test]
fn messages_response_for_chat_and_responses_clients() {
    let out = wire::translate_response(Messages, Chat, claude_response(), "my-claude").unwrap();
    assert_eq!(
        strip(out, &["created"]),
        json!({
            "id": "chatcmpl-01", "object": "chat.completion", "model": "my-claude",
            "choices": [{ "index": 0, "logprobs": null, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": "Calling.", "refusal": null, "reasoning_content": "Hmm.",
                "tool_calls": [{ "id": "toolu_9", "type": "function", "function": { "name": "ls", "arguments": "{\"dir\":\".\"}" } }]
            }}],
            "usage": { "prompt_tokens": 115, "completion_tokens": 20, "total_tokens": 135, "prompt_tokens_details": { "cached_tokens": 100 } }
        })
    );
    let out = wire::translate_response(Messages, Responses, claude_response(), "my-claude").unwrap();
    let out = strip(out, &["created_at"]);
    assert_eq!(out["id"], "resp_01");
    assert_eq!(out["status"], "completed");
    assert_eq!(out["output"][0]["type"], "reasoning");
    assert_eq!(out["output"][0]["encrypted_content"], "roll-ant:sig-x", "the signature rides in encrypted_content");
    assert_eq!(out["output"][0]["summary"], json!([{ "type": "summary_text", "text": "Hmm." }]));
    assert_eq!(strip(out["output"][1].clone(), &["id"]), json!({ "type": "message", "status": "completed", "role": "assistant", "content": [{ "type": "output_text", "text": "Calling.", "annotations": [] }] }));
    assert_eq!(out["output"][2], json!({ "id": "fc_9", "type": "function_call", "status": "completed", "call_id": "toolu_9", "name": "ls", "arguments": "{\"dir\":\".\"}" }));
    assert_eq!(out["usage"], json!({ "input_tokens": 115, "input_tokens_details": { "cached_tokens": 100 }, "output_tokens": 20, "output_tokens_details": { "reasoning_tokens": 0 }, "total_tokens": 135 }));
    assert_eq!(out["model"], "my-claude");
}

#[test]
fn openai_responses_for_a_messages_client() {
    let chat = json!({
        "id": "chatcmpl-7", "object": "chat.completion", "created": 1, "model": "gpt-4.1",
        "choices": [{ "index": 0, "finish_reason": "tool_calls", "message": { "role": "assistant", "content": "Sure.",
            "tool_calls": [{ "id": "call_1", "type": "function", "function": { "name": "ls", "arguments": "{\"dir\":\"/\"}" } }] } }],
        "usage": { "prompt_tokens": 1000, "completion_tokens": 50, "total_tokens": 1050, "prompt_tokens_details": { "cached_tokens": 800 } }
    });
    assert_eq!(
        wire::translate_response(Chat, Messages, chat, "claude-sonnet-4-5").unwrap(),
        json!({
            "id": "msg_7", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
            "content": [{ "type": "text", "text": "Sure." }, { "type": "tool_use", "id": "call_1", "name": "ls", "input": { "dir": "/" } }],
            "stop_reason": "tool_use", "stop_sequence": null,
            "usage": { "input_tokens": 200, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 800, "output_tokens": 50 }
        }),
        "OpenAI's prompt count includes the cached tokens; Anthropic's doesn't"
    );
    let resp = json!({
        "id": "resp_5", "object": "response", "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" },
        "output": [
            { "id": "rs_5", "type": "reasoning", "summary": [{ "type": "summary_text", "text": "A" }, { "type": "summary_text", "text": "B" }], "encrypted_content": "enc5" },
            { "id": "msg_x", "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Partial" }] }
        ],
        "usage": { "input_tokens": 10, "input_tokens_details": { "cached_tokens": 0 }, "output_tokens": 99 }
    });
    assert_eq!(
        wire::translate_response(Responses, Messages, resp, "claude-x").unwrap(),
        json!({
            "id": "msg_5", "type": "message", "role": "assistant", "model": "claude-x",
            "content": [{ "type": "thinking", "thinking": "A\n\nB", "signature": "roll-oai:rs_5:enc5" }, { "type": "text", "text": "Partial" }],
            "stop_reason": "max_tokens", "stop_sequence": null,
            "usage": { "input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 99 }
        })
    );
}

#[test]
fn responses_and_chat_responses_both_ways() {
    let chat = json!({ "id": "chatcmpl-1", "choices": [{ "index": 0, "finish_reason": "length", "message": { "role": "assistant", "content": "cut" } }], "usage": { "prompt_tokens": 3, "completion_tokens": 4 } });
    let out = wire::translate_response(Chat, Responses, chat, "m").unwrap();
    assert_eq!(out["status"], "incomplete");
    assert_eq!(out["incomplete_details"], json!({ "reason": "max_output_tokens" }));
    assert_eq!(out["output"][0]["content"][0]["text"], "cut");

    let resp = json!({ "id": "resp_1", "status": "completed", "output": [
        { "type": "function_call", "call_id": "call_z", "name": "f", "arguments": "{}" }
    ], "usage": { "input_tokens": 5, "output_tokens": 1 } });
    let out = wire::translate_response(Responses, Chat, resp, "m").unwrap();
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(out["choices"][0]["message"]["content"], Value::Null);
    assert_eq!(out["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "f");

    // Same format: the model the client asked for, nothing else.
    let out = wire::translate_response(Messages, Messages, claude_response(), "alias").unwrap();
    let mut want = claude_response();
    want["model"] = json!("alias");
    assert_eq!(out, want);

    assert!(matches!(wire::translate_response(Chat, Messages, json!({ "error": { "message": "boom" } }), "m"), Err(WireError::Invalid(_))));
}

#[test]
fn tool_calls_get_the_clients_own_names_back() {
    // Codex's freeform and namespaced tools, answered by Claude.
    let claude = json!({
        "id": "msg_2", "type": "message", "role": "assistant", "content": [
            { "type": "tool_use", "id": "toolu_a", "name": "apply_patch", "input": { "input": "*** Begin Patch" } },
            { "type": "tool_use", "id": "toolu_b", "name": "mcp__docs__search", "input": { "q": "y" } }
        ],
        "stop_reason": "tool_use", "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    let out = wire::translate_response_with(Messages, Responses, claude, "gpt-5-codex", &codex_request()).unwrap();
    assert_eq!(out["output"][0], json!({ "id": "ctc_a", "type": "custom_tool_call", "status": "completed", "call_id": "toolu_a", "name": "apply_patch", "input": "*** Begin Patch" }));
    assert_eq!(
        out["output"][1],
        json!({ "id": "fc_b", "type": "function_call", "status": "completed", "call_id": "toolu_b", "name": "search", "namespace": "mcp__docs", "arguments": "{\"q\":\"y\"}" })
    );

    // A name the upstream's API refuses goes up hashed and comes back as it was.
    let long = "a.tool.with.dots.and.a.name.far.longer.than.sixty.four.characters.allowed";
    let req = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }],
        "tools": [{ "type": "function", "function": { "name": long, "parameters": { "type": "object" } } }] });
    let up = wire::translate_request(Chat, Messages, req.clone(), "claude-opus-4-8").unwrap();
    let sent = up["tools"][0]["name"].as_str().unwrap().to_string();
    assert!(sent.len() <= 64 && sent.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'), "{sent}");
    let answer = json!({ "id": "msg_3", "type": "message", "content": [{ "type": "tool_use", "id": "toolu_c", "name": sent, "input": {} }], "stop_reason": "tool_use", "usage": {} });
    let out = wire::translate_response_with(Messages, Chat, answer, "m", &req).unwrap();
    assert_eq!(out["choices"][0]["message"]["tool_calls"][0]["function"]["name"], long);
}

// ---------------------------------------------------------------------------------------------
// Streams.

/// A Chat upstream (OpenAI's documented chunk shapes) streaming text, then two tool calls with
/// the first one's arguments split over many deltas, a finish chunk, a usage-only chunk and
/// `[DONE]`.
fn openai_tool_stream() -> String {
    let mut events: Vec<(Option<&str>, Value)> = vec![
        (None, chunk(json!({ "role": "assistant", "content": "" }), None)),
        (None, chunk(json!({ "content": "Let me " }), None)),
        (None, chunk(json!({ "content": "check." }), None)),
        (None, chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function", "function": { "name": "read_file", "arguments": "" } }] }), None)),
    ];
    for piece in ["{\"pa", "th\": \"", "src/ma", "in.rs\"", "}"] {
        events.push((None, chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": piece } }] }), None)));
    }
    events.push((None, chunk(json!({ "tool_calls": [{ "index": 1, "id": "call_2", "type": "function", "function": { "name": "list", "arguments": "{\"d\":" } }] }), None)));
    events.push((None, chunk(json!({ "tool_calls": [{ "index": 1, "function": { "arguments": "1}" } }] }), None)));
    events.push((None, chunk(json!({}), Some("tool_calls"))));
    events.push((
        None,
        json!({ "id": "chatcmpl-abc", "object": "chat.completion.chunk", "created": 1, "model": "gpt-5", "choices": [],
            "usage": { "prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150, "prompt_tokens_details": { "cached_tokens": 100 } } }),
    ));
    let mut s = sse(&events);
    s.push_str("data: [DONE]\n\n");
    s
}

#[test]
fn chat_stream_for_a_messages_client() {
    let (events, usage) = translate_stream(Chat, Messages, "claude-sonnet-4-5", None, &openai_tool_stream());
    let ev = |n: &str, d: Value| (Some(n.to_string()), d);
    let delta = |i: u64, d: Value| ev("content_block_delta", json!({ "type": "content_block_delta", "index": i, "delta": d }));
    assert_eq!(
        events,
        vec![
            ev("message_start", json!({ "type": "message_start", "message": {
                "id": "msg_abc", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [],
                "stop_reason": null, "stop_sequence": null,
                "usage": { "input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 0 } } })),
            ev("ping", json!({ "type": "ping" })),
            ev("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            delta(0, json!({ "type": "text_delta", "text": "Let me " })),
            delta(0, json!({ "type": "text_delta", "text": "check." })),
            ev("content_block_stop", json!({ "type": "content_block_stop", "index": 0 })),
            ev("content_block_start", json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "call_1", "name": "read_file", "input": {} } })),
            delta(1, json!({ "type": "input_json_delta", "partial_json": "{\"pa" })),
            delta(1, json!({ "type": "input_json_delta", "partial_json": "th\": \"" })),
            delta(1, json!({ "type": "input_json_delta", "partial_json": "src/ma" })),
            delta(1, json!({ "type": "input_json_delta", "partial_json": "in.rs\"" })),
            delta(1, json!({ "type": "input_json_delta", "partial_json": "}" })),
            ev("content_block_stop", json!({ "type": "content_block_stop", "index": 1 })),
            ev("content_block_start", json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "call_2", "name": "list", "input": {} } })),
            delta(2, json!({ "type": "input_json_delta", "partial_json": "{\"d\":1}" })),
            ev("content_block_stop", json!({ "type": "content_block_stop", "index": 2 })),
            ev("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use", "stop_sequence": null },
                "usage": { "input_tokens": 20, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 100, "output_tokens": 30 } })),
            ev("message_stop", json!({ "type": "message_stop" })),
        ]
    );
    assert_eq!(usage, Usage { input: 20, output: 30, cache_read: 100, cache_write: 0 });
}

#[test]
fn interleaved_chat_tool_calls_stay_sequential_blocks() {
    let mut events: Vec<(Option<&str>, Value)> = vec![
        (None, chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_a", "function": { "name": "a", "arguments": "{\"x\"" } }] }), None)),
        (None, chunk(json!({ "tool_calls": [{ "index": 1, "id": "call_b", "function": { "name": "b", "arguments": "{\"y\"" } }] }), None)),
        (None, chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": ":1}" } }] }), None)),
        (None, chunk(json!({ "tool_calls": [{ "index": 1, "function": { "arguments": ":2}" } }] }), None)),
        (None, chunk(json!({ "content": "after" }), None)),
    ];
    events.push((None, chunk(json!({}), Some("stop"))));
    let mut raw = sse(&events);
    raw.push_str("data: [DONE]\n\n");
    let (events, _) = translate_stream(Chat, Messages, "c", None, &raw);
    let mut args: Vec<(u64, String)> = Vec::new();
    let mut open: Option<u64> = None;
    for (name, d) in &events {
        match name.as_deref() {
            Some("content_block_start") => {
                assert!(open.is_none(), "one block open at a time");
                open = d["index"].as_u64();
            }
            Some("content_block_stop") => open = None,
            Some("content_block_delta") if d["delta"]["type"] == "input_json_delta" => {
                let i = d["index"].as_u64().unwrap();
                match args.iter_mut().find(|(k, _)| *k == i) {
                    Some((_, s)) => s.push_str(d["delta"]["partial_json"].as_str().unwrap()),
                    None => args.push((i, d["delta"]["partial_json"].as_str().unwrap().to_string())),
                }
            }
            _ => {}
        }
    }
    assert_eq!(args, vec![(0, "{\"x\":1}".to_string()), (1, "{\"y\":2}".to_string())]);
    let n = names(&events);
    assert_eq!(n.iter().filter(|n| *n == "content_block_start").count(), 3, "two tools, then the text that came after them");
    assert_eq!(events[events.len() - 2].1["delta"]["stop_reason"], "tool_use", "tools were called, whatever the finish reason said");
}

/// A Messages upstream (Anthropic's documented stream) with thinking, text and a tool call
/// whose input arrives in pieces.
fn anthropic_stream() -> String {
    let d = |i: u64, delta: Value| (Some("content_block_delta"), json!({ "type": "content_block_delta", "index": i, "delta": delta }));
    sse(&[
        (Some("message_start"), json!({ "type": "message_start", "message": { "id": "msg_01XY", "type": "message", "role": "assistant", "model": "claude-opus-4-8", "content": [],
            "stop_reason": null, "stop_sequence": null, "usage": { "input_tokens": 50, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 2000, "output_tokens": 1 } } })),
        (Some("content_block_start"), json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } })),
        (Some("ping"), json!({ "type": "ping" })),
        d(0, json!({ "type": "thinking_delta", "thinking": "Plan: " })),
        d(0, json!({ "type": "thinking_delta", "thinking": "read it." })),
        d(0, json!({ "type": "signature_delta", "signature": "EqQBCgIYAh" })),
        (Some("content_block_stop"), json!({ "type": "content_block_stop", "index": 0 })),
        (Some("content_block_start"), json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } })),
        d(1, json!({ "type": "text_delta", "text": "Reading" })),
        d(1, json!({ "type": "text_delta", "text": " now." })),
        (Some("content_block_stop"), json!({ "type": "content_block_stop", "index": 1 })),
        (Some("content_block_start"), json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "toolu_01A", "name": "shell", "input": {} } })),
        d(2, json!({ "type": "input_json_delta", "partial_json": "" })),
        d(2, json!({ "type": "input_json_delta", "partial_json": "{\"command\": [\"c" })),
        d(2, json!({ "type": "input_json_delta", "partial_json": "at\", \"a.txt\"]}" })),
        (Some("content_block_stop"), json!({ "type": "content_block_stop", "index": 2 })),
        (Some("message_delta"), json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use", "stop_sequence": null }, "usage": { "output_tokens": 77 } })),
        (Some("message_stop"), json!({ "type": "message_stop" })),
    ])
}

#[test]
fn messages_stream_for_a_chat_client() {
    let client = json!({ "model": "c", "messages": [], "stream": true, "stream_options": { "include_usage": true } });
    let (events, usage) = translate_stream(Messages, Chat, "my-model", Some(&client), &anthropic_stream());
    let data: Vec<Value> = events.into_iter().map(|(e, d)| {
        assert_eq!(e, None, "Chat chunks are unnamed");
        strip(d, &["created"])
    }).collect();
    let c = |delta: Value, finish: Value| json!({ "id": "chatcmpl-01XY", "object": "chat.completion.chunk", "model": "my-model", "choices": [{ "index": 0, "delta": delta, "logprobs": null, "finish_reason": finish }] });
    assert_eq!(
        data,
        vec![
            c(json!({ "role": "assistant", "content": "" }), Value::Null),
            c(json!({ "reasoning_content": "Plan: " }), Value::Null),
            c(json!({ "reasoning_content": "read it." }), Value::Null),
            c(json!({ "content": "Reading" }), Value::Null),
            c(json!({ "content": " now." }), Value::Null),
            c(json!({ "tool_calls": [{ "index": 0, "id": "toolu_01A", "type": "function", "function": { "name": "shell", "arguments": "" } }] }), Value::Null),
            c(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "{\"command\": [\"c" } }] }), Value::Null),
            c(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "at\", \"a.txt\"]}" } }] }), Value::Null),
            c(json!({}), json!("tool_calls")),
            json!({ "id": "chatcmpl-01XY", "object": "chat.completion.chunk", "model": "my-model", "choices": [],
                "usage": { "prompt_tokens": 2060, "completion_tokens": 77, "total_tokens": 2137, "prompt_tokens_details": { "cached_tokens": 2000 } } }),
            json!("[DONE]"),
        ]
    );
    assert_eq!(usage, Usage { input: 50, output: 77, cache_read: 2000, cache_write: 10 });

    // A client that didn't ask for usage gets no usage chunk.
    let (events, _) = translate_stream(Messages, Chat, "m", None, &anthropic_stream());
    assert!(events.iter().all(|(_, d)| d.get("usage").is_none()));
    assert_eq!(events.last().unwrap().1, json!("[DONE]"));
}

/// Checks a Responses event stream the way Codex reads it, and returns the finished response.
fn check_responses_stream(events: &[(Option<String>, Value)]) -> Value {
    for (i, (name, d)) in events.iter().enumerate() {
        assert_eq!(name.as_deref(), d["type"].as_str(), "the event name is the type");
        assert_eq!(d["sequence_number"], i as u64, "sequence numbers count from 0");
    }
    let n = names(events);
    assert_eq!(&n[..2], ["response.created", "response.in_progress"]);
    let mut added = Vec::new();
    let mut done = Vec::new();
    for (_, d) in events {
        match d["type"].as_str().unwrap() {
            "response.output_item.added" => added.push(d.clone()),
            "response.output_item.done" => done.push(d.clone()),
            _ => {}
        }
    }
    assert_eq!(added.len(), done.len());
    for (i, (a, d)) in added.iter().zip(&done).enumerate() {
        assert_eq!(a["output_index"], i as u64);
        assert_eq!(d["output_index"], i as u64);
        assert_eq!(a["item"]["id"], d["item"]["id"], "an item keeps its id");
        assert!(d["item"]["id"].as_str().is_some_and(|s| !s.is_empty()), "Codex needs every item's id");
    }
    let last = &events.last().unwrap().1;
    let r = last["response"].clone();
    let items: Vec<Value> = done.iter().map(|d| d["item"].clone()).collect();
    assert_eq!(r["output"], Value::Array(items), "the finished response repeats every finished item");
    r
}

#[test]
fn messages_stream_for_a_responses_client() {
    let (events, usage) = translate_stream(Messages, Responses, "gpt-5-codex", Some(&codex_request()), &anthropic_stream());
    let r = check_responses_stream(&events);
    assert_eq!(
        names(&events),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(events[0].1["response"]["id"], "resp_01XY");
    assert_eq!(events[0].1["response"]["model"], "gpt-5-codex");
    assert_eq!(events[0].1["response"]["status"], "in_progress");
    let reasoning = &r["output"][0];
    assert_eq!(reasoning["summary"], json!([{ "type": "summary_text", "text": "Plan: read it." }]));
    assert_eq!(reasoning["encrypted_content"], "roll-ant:EqQBCgIYAh");
    assert_eq!(r["output"][1]["content"], json!([{ "type": "output_text", "text": "Reading now.", "annotations": [] }]));
    assert_eq!(r["output"][2], json!({ "id": "fc_01A", "type": "function_call", "status": "completed", "call_id": "toolu_01A", "name": "shell", "arguments": "{\"command\": [\"cat\", \"a.txt\"]}" }));
    assert_eq!(events[19].1["arguments"], "{\"command\": [\"cat\", \"a.txt\"]}");
    assert_eq!(r["status"], "completed");
    assert_eq!(r["usage"], json!({ "input_tokens": 2060, "input_tokens_details": { "cached_tokens": 2000 }, "output_tokens": 77, "output_tokens_details": { "reasoning_tokens": 0 }, "total_tokens": 2137 }));
    assert_eq!(usage, Usage { input: 50, output: 77, cache_read: 2000, cache_write: 10 });
}

/// An OpenAI Responses upstream (the documented event sequence) with a reasoning item, a
/// message and a function call.
fn openai_responses_stream(call_name: &str, namespace: Option<&str>) -> String {
    let resp = |status: &str| json!({ "id": "resp_77", "object": "response", "created_at": 1, "status": status, "model": "gpt-5", "output": [] });
    let mut call = json!({ "id": "fc_77", "type": "function_call", "status": "in_progress", "call_id": "call_77", "name": call_name, "arguments": "" });
    if let Some(ns) = namespace {
        call["namespace"] = json!(ns);
    }
    let mut call_done = call.clone();
    call_done["status"] = json!("completed");
    call_done["arguments"] = json!("{\"q\":\"rust\"}");
    let mut completed = resp("completed");
    completed["usage"] = json!({ "input_tokens": 900, "input_tokens_details": { "cached_tokens": 600 }, "output_tokens": 40, "output_tokens_details": { "reasoning_tokens": 12 }, "total_tokens": 940 });
    let ev = |t: &str, mut v: Value| {
        v["type"] = json!(t);
        (None::<&str>, v)
    };
    let mut events = vec![
        ev("response.created", json!({ "sequence_number": 0, "response": resp("in_progress") })),
        ev("response.in_progress", json!({ "sequence_number": 1, "response": resp("in_progress") })),
        ev("response.output_item.added", json!({ "output_index": 0, "item": { "id": "rs_77", "type": "reasoning", "summary": [] } })),
        ev("response.reasoning_summary_part.added", json!({ "item_id": "rs_77", "output_index": 0, "summary_index": 0, "part": { "type": "summary_text", "text": "" } })),
        ev("response.reasoning_summary_text.delta", json!({ "item_id": "rs_77", "output_index": 0, "summary_index": 0, "delta": "**Searching**" })),
        ev("response.reasoning_summary_text.done", json!({ "item_id": "rs_77", "output_index": 0, "summary_index": 0, "text": "**Searching**" })),
        ev("response.reasoning_summary_part.added", json!({ "item_id": "rs_77", "output_index": 0, "summary_index": 1, "part": { "type": "summary_text", "text": "" } })),
        ev("response.reasoning_summary_text.delta", json!({ "item_id": "rs_77", "output_index": 0, "summary_index": 1, "delta": "Then answer." })),
        ev("response.output_item.done", json!({ "output_index": 0, "item": { "id": "rs_77", "type": "reasoning", "summary": [], "encrypted_content": "gAAAAB-enc" } })),
        ev("response.output_item.added", json!({ "output_index": 1, "item": { "id": "msg_77", "type": "message", "status": "in_progress", "role": "assistant", "content": [] } })),
        ev("response.content_part.added", json!({ "item_id": "msg_77", "output_index": 1, "content_index": 0, "part": { "type": "output_text", "text": "", "annotations": [] } })),
        ev("response.output_text.delta", json!({ "item_id": "msg_77", "output_index": 1, "content_index": 0, "delta": "Let me " })),
        ev("response.output_text.delta", json!({ "item_id": "msg_77", "output_index": 1, "content_index": 0, "delta": "search." })),
        ev("response.output_text.done", json!({ "item_id": "msg_77", "output_index": 1, "content_index": 0, "text": "Let me search." })),
        ev("response.content_part.done", json!({ "item_id": "msg_77", "output_index": 1, "content_index": 0, "part": { "type": "output_text", "text": "Let me search." } })),
        ev("response.output_item.done", json!({ "output_index": 1, "item": { "id": "msg_77", "type": "message", "status": "completed", "role": "assistant", "content": [] } })),
        ev("response.output_item.added", json!({ "output_index": 2, "item": call })),
    ];
    for piece in ["{\"q\"", ":\"ru", "st\"}"] {
        events.push(ev("response.function_call_arguments.delta", json!({ "item_id": "fc_77", "output_index": 2, "delta": piece })));
    }
    events.push(ev("response.function_call_arguments.done", json!({ "item_id": "fc_77", "output_index": 2, "arguments": "{\"q\":\"rust\"}" })));
    events.push(ev("response.output_item.done", json!({ "output_index": 2, "item": call_done })));
    events.push(ev("response.completed", json!({ "response": completed })));
    // OpenAI names every event; the parser keeps the name, the decoder reads `type`.
    let mut s = String::new();
    for (_, v) in events {
        s.push_str(&format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()));
    }
    s
}

#[test]
fn responses_stream_for_a_messages_client() {
    let (events, usage) = translate_stream(Responses, Messages, "claude-opus-4-8", None, &openai_responses_stream("web_lookup", None));
    let ev = |n: &str, d: Value| (Some(n.to_string()), d);
    let delta = |i: u64, d: Value| ev("content_block_delta", json!({ "type": "content_block_delta", "index": i, "delta": d }));
    let stop = |i: u64| ev("content_block_stop", json!({ "type": "content_block_stop", "index": i }));
    assert_eq!(
        events,
        vec![
            ev("message_start", json!({ "type": "message_start", "message": {
                "id": "msg_77", "type": "message", "role": "assistant", "model": "claude-opus-4-8", "content": [],
                "stop_reason": null, "stop_sequence": null,
                "usage": { "input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 0 } } })),
            ev("ping", json!({ "type": "ping" })),
            ev("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } })),
            delta(0, json!({ "type": "thinking_delta", "thinking": "**Searching**" })),
            delta(0, json!({ "type": "thinking_delta", "thinking": "\n\n" })),
            delta(0, json!({ "type": "thinking_delta", "thinking": "Then answer." })),
            delta(0, json!({ "type": "signature_delta", "signature": "roll-oai:rs_77:gAAAAB-enc" })),
            stop(0),
            ev("content_block_start", json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } })),
            delta(1, json!({ "type": "text_delta", "text": "Let me " })),
            delta(1, json!({ "type": "text_delta", "text": "search." })),
            stop(1),
            ev("content_block_start", json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "call_77", "name": "web_lookup", "input": {} } })),
            delta(2, json!({ "type": "input_json_delta", "partial_json": "{\"q\"" })),
            delta(2, json!({ "type": "input_json_delta", "partial_json": ":\"ru" })),
            delta(2, json!({ "type": "input_json_delta", "partial_json": "st\"}" })),
            stop(2),
            ev("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use", "stop_sequence": null },
                "usage": { "input_tokens": 300, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 600, "output_tokens": 40 } })),
            ev("message_stop", json!({ "type": "message_stop" })),
        ]
    );
    assert_eq!(usage, Usage { input: 300, output: 40, cache_read: 600, cache_write: 0 });

    // The signature Claude Code keeps goes back to OpenAI as the reasoning item it was.
    let next_turn = json!({ "model": "m", "max_tokens": 100, "messages": [
        { "role": "user", "content": "q" },
        { "role": "assistant", "content": [
            { "type": "thinking", "thinking": "**Searching**\n\nThen answer.", "signature": "roll-oai:rs_77:gAAAAB-enc" },
            { "type": "tool_use", "id": "call_77", "name": "web_lookup", "input": { "q": "rust" } }
        ]},
        { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "call_77", "content": "ok" }] }
    ]});
    let up = wire::translate_request(Messages, Responses, next_turn, "gpt-5").unwrap();
    assert_eq!(up["input"][1], json!({ "type": "reasoning", "id": "rs_77", "summary": [{ "type": "summary_text", "text": "**Searching**\n\nThen answer." }], "encrypted_content": "gAAAAB-enc" }));
}

#[test]
fn responses_stream_for_a_chat_client() {
    let (events, _) = translate_stream(Responses, Chat, "m", None, &openai_responses_stream("f", None));
    let deltas: Vec<Value> = events.iter().filter_map(|(_, d)| d.get("choices").and_then(|c| c.get(0)).map(|c| c["delta"].clone())).collect();
    assert_eq!(deltas[1], json!({ "reasoning_content": "**Searching**" }));
    assert!(deltas.contains(&json!({ "content": "Let me " })));
    assert!(deltas.contains(&json!({ "tool_calls": [{ "index": 0, "id": "call_77", "type": "function", "function": { "name": "f", "arguments": "" } }] })));
    let finish = events.iter().find_map(|(_, d)| d["choices"].get(0).and_then(|c| c["finish_reason"].as_str()).map(str::to_string));
    assert_eq!(finish.as_deref(), Some("tool_calls"));
    assert_eq!(events.last().unwrap().1, json!("[DONE]"));
}

#[test]
fn freeform_and_namespaced_calls_stream_back_as_codex_declared_them() {
    // Codex on a Chat upstream: apply_patch went up as a function with an `input` wrapper.
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch";
    let args = json!({ "input": patch }).to_string();
    let mut events: Vec<(Option<&str>, Value)> =
        vec![(None, chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_p", "type": "function", "function": { "name": "apply_patch", "arguments": "" } }] }), None))];
    for piece in args.as_bytes().chunks(9) {
        events.push((None, chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": String::from_utf8_lossy(piece) } }] }), None)));
    }
    events.push((None, chunk(json!({}), Some("tool_calls"))));
    let mut raw = sse(&events);
    raw.push_str("data: [DONE]\n\n");
    let (out, _) = translate_stream(Chat, Responses, "gpt-5-codex", Some(&codex_request()), &raw);
    let r = check_responses_stream(&out);
    assert_eq!(r["output"][0], json!({ "id": "ctc_p", "type": "custom_tool_call", "status": "completed", "call_id": "call_p", "name": "apply_patch", "input": patch }));
    assert!(names(&out).contains(&"response.custom_tool_call_input.done".to_string()));
    assert!(!names(&out).contains(&"response.function_call_arguments.delta".to_string()));

    // A namespaced MCP tool, answered by a Responses upstream under its flat name.
    let (out, _) = translate_stream(Responses, Responses, "x", None, &openai_responses_stream("search", Some("mcp__docs")));
    assert_eq!(out.len(), 23, "same format: every event passes through");
    let claude_call = sse(&[
        (Some("message_start"), json!({ "type": "message_start", "message": { "id": "msg_n", "usage": { "input_tokens": 1, "output_tokens": 0 } } })),
        (Some("content_block_start"), json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu_n", "name": "mcp__docs__search", "input": {} } })),
        (Some("content_block_delta"), json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"q\":\"a\"}" } })),
        (Some("content_block_stop"), json!({ "type": "content_block_stop", "index": 0 })),
        (Some("message_delta"), json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } })),
        (Some("message_stop"), json!({ "type": "message_stop" })),
    ]);
    let (out, _) = translate_stream(Messages, Responses, "gpt-5-codex", Some(&codex_request()), &claude_call);
    let r = check_responses_stream(&out);
    assert_eq!(r["output"][0]["name"], "search");
    assert_eq!(r["output"][0]["namespace"], "mcp__docs");
    assert_eq!(out[2].1["item"]["namespace"], "mcp__docs", "output_item.added names the namespace too");
}

#[test]
fn stop_reasons_map_both_ways() {
    let claude = |reason: &str| {
        sse(&[
            (Some("message_start"), json!({ "type": "message_start", "message": { "id": "msg_s", "usage": { "input_tokens": 1, "output_tokens": 0 } } })),
            (Some("content_block_start"), json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            (Some("content_block_delta"), json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "x" } })),
            (Some("content_block_stop"), json!({ "type": "content_block_stop", "index": 0 })),
            (Some("message_delta"), json!({ "type": "message_delta", "delta": { "stop_reason": reason, "stop_sequence": null }, "usage": { "output_tokens": 1 } })),
            (Some("message_stop"), json!({ "type": "message_stop" })),
        ])
    };
    for (reason, chat_finish, status, incomplete) in [
        ("end_turn", "stop", "completed", Value::Null),
        ("max_tokens", "length", "incomplete", json!({ "reason": "max_output_tokens" })),
        ("stop_sequence", "stop", "completed", Value::Null),
        ("refusal", "content_filter", "incomplete", json!({ "reason": "content_filter" })),
    ] {
        let (out, _) = translate_stream(Messages, Chat, "m", None, &claude(reason));
        let finish = out.iter().find_map(|(_, d)| d["choices"].get(0).and_then(|c| c["finish_reason"].as_str()).map(str::to_string));
        assert_eq!(finish.as_deref(), Some(chat_finish), "{reason}");
        let (out, _) = translate_stream(Messages, Responses, "m", None, &claude(reason));
        let r = check_responses_stream(&out);
        assert_eq!(r["status"], status, "{reason}");
        assert_eq!(r["incomplete_details"], incomplete, "{reason}");
        assert_eq!(out.last().unwrap().0.as_deref(), Some(if status == "completed" { "response.completed" } else { "response.incomplete" }));
    }
    for (finish, reason) in [("stop", "end_turn"), ("length", "max_tokens"), ("content_filter", "refusal")] {
        let mut raw = sse(&[(None, chunk(json!({ "content": "x" }), None)), (None, chunk(json!({}), Some(finish)))]);
        raw.push_str("data: [DONE]\n\n");
        let (out, _) = translate_stream(Chat, Messages, "m", None, &raw);
        let delta = out.iter().find(|(e, _)| e.as_deref() == Some("message_delta")).unwrap();
        assert_eq!(delta.1["delta"]["stop_reason"], reason, "{finish}");
    }
}

#[test]
fn upstream_errors_and_cut_streams_reach_the_client_as_errors() {
    // Anthropic's overloaded error mid-stream, for Codex.
    let raw = sse(&[
        (Some("message_start"), json!({ "type": "message_start", "message": { "id": "msg_e", "usage": { "input_tokens": 1, "output_tokens": 0 } } })),
        (Some("content_block_start"), json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
        (Some("content_block_delta"), json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "par" } })),
        (Some("error"), json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } })),
    ]);
    let (out, _) = translate_stream(Messages, Responses, "m", None, &raw);
    let last = &out.last().unwrap().1;
    assert_eq!(last["type"], "response.failed");
    assert_eq!(last["response"]["status"], "failed");
    assert_eq!(last["response"]["error"], json!({ "code": "server_is_overloaded", "message": "Overloaded" }));
    assert_eq!(last["response"]["output"][0]["content"][0]["text"], "par", "what was said before the error is kept");

    // OpenAI's response.failed, for Claude Code.
    let raw = sse(&[
        (Some("response.created"), json!({ "type": "response.created", "response": { "id": "resp_f" } })),
        (Some("response.failed"), json!({ "type": "response.failed", "response": { "id": "resp_f", "status": "failed", "error": { "code": "rate_limit_exceeded", "message": "Slow down" } } })),
    ]);
    let (out, _) = translate_stream(Responses, Messages, "m", None, &raw);
    assert_eq!(out.last().unwrap(), &(Some("error".to_string()), json!({ "type": "error", "error": { "type": "rate_limit_error", "message": "Slow down" } })));

    // A Chat stream cut before its finish chunk is an error, never a clean end.
    let raw = sse(&[(None, chunk(json!({ "content": "half" }), None))]);
    let (out, _) = translate_stream(Chat, Messages, "m", None, &raw);
    assert_eq!(out.last().unwrap().0.as_deref(), Some("error"));
    assert!(!names(&out).contains(&"message_stop".to_string()));
    // …and a Messages stream cut before message_delta, for a Chat client.
    let raw = sse(&[(Some("message_start"), json!({ "type": "message_start", "message": { "id": "msg_c" } }))]);
    let (out, _) = translate_stream(Messages, Chat, "m", None, &raw);
    assert!(out[out.len() - 2].1.get("error").is_some());
    assert_eq!(out.last().unwrap().1, json!("[DONE]"));
    // An error chunk from a Chat upstream (as OpenRouter sends).
    let raw = "data: {\"error\":{\"message\":\"Provider returned error\",\"code\":502}}\n\n";
    let (out, _) = translate_stream(Chat, Messages, "m", None, raw);
    assert_eq!(out, vec![(Some("error".to_string()), json!({ "type": "error", "error": { "type": "api_error", "message": "Provider returned error" } }))]);
    // Data that isn't JSON is the upstream's fault, reported as such.
    let mut t = StreamTranslator::new(Chat, Messages, "m");
    assert!(matches!(t.push(&Event::data("{not json")), Err(WireError::Invalid(_))));
}

#[test]
fn redacted_thinking_survives_a_round_trip_through_codex() {
    let raw = sse(&[
        (Some("message_start"), json!({ "type": "message_start", "message": { "id": "msg_r", "usage": { "input_tokens": 1, "output_tokens": 0 } } })),
        (Some("content_block_start"), json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "redacted_thinking", "data": "EmwKAhgB" } })),
        (Some("content_block_stop"), json!({ "type": "content_block_stop", "index": 0 })),
        (Some("message_delta"), json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 3 } })),
    ]);
    let (out, _) = translate_stream(Messages, Responses, "m", None, &raw);
    let r = check_responses_stream(&out);
    assert_eq!(r["output"][0]["encrypted_content"], "roll-ant-r:EmwKAhgB");
    let codex_next = json!({ "model": "m", "input": [
        { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "q" }] },
        r["output"][0].clone(),
        { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "again" }] }
    ]});
    let up = wire::translate_request(Responses, Messages, codex_next, "claude-opus-4-8").unwrap();
    assert_eq!(up["messages"][1]["content"][0], json!({ "type": "redacted_thinking", "data": "EmwKAhgB" }));
}

#[test]
fn pass_through_streams_are_untouched_and_tapped() {
    for (format, raw) in [(Messages, anthropic_stream()), (Chat, openai_tool_stream()), (Responses, openai_responses_stream("f", None))] {
        let input = parse(raw.as_bytes(), 1000);
        let mut t = StreamTranslator::new(format, format, "alias");
        let mut out = Vec::new();
        for e in &input {
            out.extend(t.push(e).unwrap());
        }
        out.extend(t.finish());
        assert_eq!(out, input, "{format:?}: byte-for-byte the upstream's events");
        let mut tap = UsageTap::new(format);
        input.iter().for_each(|e| tap.push(e));
        assert_eq!(tap.usage(), t.usage());
        let want = match format {
            Messages => Usage { input: 50, output: 77, cache_read: 2000, cache_write: 10 },
            Chat => Usage { input: 20, output: 30, cache_read: 100, cache_write: 0 },
            Responses => Usage { input: 300, output: 40, cache_read: 600, cache_write: 0 },
        };
        assert_eq!(tap.usage(), want, "{format:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Usage and errors.

#[test]
fn usage_of_whole_responses() {
    assert_eq!(wire::usage_of(Messages, &claude_response()), Usage { input: 10, output: 20, cache_read: 100, cache_write: 5 });
    let chat = json!({ "usage": { "prompt_tokens": 500, "completion_tokens": 7, "prompt_tokens_details": { "cached_tokens": 450, "cache_write_tokens": 20 } } });
    assert_eq!(wire::usage_of(Chat, &chat), Usage { input: 30, output: 7, cache_read: 450, cache_write: 20 }, "OpenRouter's cache writes are in its prompt count too");
    let resp = json!({ "usage": { "input_tokens": 9, "input_tokens_details": { "cached_tokens": 4 }, "output_tokens": 2 } });
    assert_eq!(wire::usage_of(Responses, &resp), Usage { input: 5, output: 2, cache_read: 4, cache_write: 0 });
    assert_eq!(wire::usage_of(Chat, &json!({ "usage": null })), Usage::default());
}

#[test]
fn error_bodies_in_each_clients_shape() {
    assert_eq!(
        wire::error_body(Messages, 429, "rate_limit_error", "slow"),
        json!({ "type": "error", "error": { "type": "rate_limit_error", "message": "slow" } })
    );
    assert_eq!(
        wire::error_body(Chat, 401, "authentication_error", "bad key"),
        json!({ "error": { "type": "authentication_error", "message": "bad key", "param": null, "code": "invalid_api_key" } })
    );
    assert_eq!(wire::error_body(Responses, 529, "overloaded_error", "busy")["error"]["code"], "server_is_overloaded");
}

#[test]
fn a_stream_that_never_stops_opening_blocks_is_refused() {
    let mut t = StreamTranslator::new(Chat, Messages, "m");
    let mut refused = None;
    for i in 0..10_000 {
        let delta = if i % 2 == 0 { json!({ "reasoning_content": "a" }) } else { json!({ "content": "b" }) };
        if let Err(e) = t.push(&Event::data(chunk(delta, None).to_string())) {
            refused = Some((i, e));
            break;
        }
    }
    let (at, err) = refused.expect("refused");
    assert!(matches!(err, WireError::TooLarge(_)), "{err:?}");
    assert!(at < 5000, "refused at block {at}");
}

#[test]
fn a_responses_message_without_part_events_still_streams_its_text() {
    let raw = sse(&[
        (None, json!({ "type": "response.created", "response": { "id": "resp_q" } })),
        (None, json!({ "type": "response.output_item.added", "output_index": 0, "item": { "id": "msg_q", "type": "message", "role": "assistant", "content": [] } })),
        (None, json!({ "type": "response.output_item.done", "output_index": 0, "item": { "id": "msg_q", "type": "message", "role": "assistant",
            "content": [{ "type": "output_text", "text": "whole" }] } })),
        (None, json!({ "type": "response.completed", "response": { "id": "resp_q", "status": "completed", "usage": { "input_tokens": 1, "output_tokens": 1 } } })),
    ]);
    let (out, _) = translate_stream(Responses, Messages, "m", None, &raw);
    let text: Vec<&str> = out.iter().filter_map(|(_, d)| d["delta"]["text"].as_str()).collect();
    assert_eq!(text, ["whole"]);
    assert_eq!(out.last().unwrap().0.as_deref(), Some("message_stop"));
}
