//! The Slack and Discord clients against mock APIs on a local axum server: what they send
//! (tokens, JSON bodies, OAuth forms) and how they read answers (errors, rate limits, size caps).
//! Tokens must never reach an error's text.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use code_chat::{Error, discord, slack};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const SLACK_TOKEN: &str = "xoxb-secret-slack-token";
const DISCORD_TOKEN: &str = "secret.discord.bot-token";

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    auth: Option<String>,
    content_type: Option<String>,
    body: String,
}

impl Seen {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("json body")
    }

    fn form(&self) -> BTreeMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

/// What the mock answers: status, extra headers, body.
type Answer = (u16, Vec<(&'static str, String)>, String);
type Route = dyn Fn(&Seen) -> Answer + Send + Sync;

struct Mock {
    seen: Mutex<Vec<Seen>>,
    route: Box<Route>,
}

async fn handle(
    State(mock): State<Arc<Mock>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let header = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let seen = Seen {
        method: method.to_string(),
        path: uri.path().to_owned(),
        query: url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect(),
        auth: header("authorization"),
        content_type: header("content-type"),
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    let (status, extra, body) = (mock.route)(&seen);
    mock.seen.lock().unwrap().push(seen);
    let mut res = (StatusCode::from_u16(status).unwrap(), body).into_response();
    res.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    for (k, v) in extra {
        res.headers_mut().insert(k, v.parse().unwrap());
    }
    res
}

/// Start a mock API; returns its base URL and the shared record of requests.
async fn serve(route: impl Fn(&Seen) -> Answer + Send + Sync + 'static) -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock {
        seen: Mutex::new(Vec::new()),
        route: Box::new(route),
    });
    let app = Router::new().fallback(handle).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mock)
}

fn ok(v: Value) -> Answer {
    (200, vec![], v.to_string())
}

fn seen(mock: &Mock) -> Vec<Seen> {
    mock.seen.lock().unwrap().clone()
}

fn assert_secret_free(e: &Error) {
    let text = format!("{e} {e:?}");
    for secret in [SLACK_TOKEN, DISCORD_TOKEN, "client-secret-value"] {
        assert!(!text.contains(secret), "{text}");
    }
}

/// A body over the 1 MiB cap.
fn huge() -> String {
    format!("{{\"ok\":true,\"pad\":\"{}\"}}", "x".repeat(2 << 20))
}

// ── Slack ──

fn slack_route(s: &Seen) -> Answer {
    match s.path.as_str() {
        "/api/chat.postMessage" => {
            ok(json!({"ok": true, "channel": "C1", "ts": "1700000000.000100", "message": {}}))
        }
        "/api/chat.update" => {
            ok(json!({"ok": true, "channel": "C1", "ts": "1700000000.000100", "text": "x"}))
        }
        "/api/chat.postEphemeral" => ok(json!({"ok": true, "message_ts": "1.2"})),
        "/api/conversations.open" => ok(json!({"ok": true, "channel": {"id": "D42"}})),
        "/api/views.open" => ok(json!({"ok": true, "view": {"id": "V1"}})),
        "/api/reactions.add" => ok(json!({"ok": false, "error": "already_reacted"})),
        "/api/chat.getPermalink" => ok(
            json!({"ok": true, "channel": "C1", "permalink": "https://x.slack.com/archives/C1/p1700000000000100"}),
        ),
        "/api/oauth.v2.access" => match s.form().get("code").map(String::as_str) {
            Some("org") => ok(json!({
                "ok": true, "access_token": "xoxb-org", "token_type": "bot", "scope": "chat:write",
                "bot_user_id": "UBOT", "app_id": "A1", "team": null,
                "enterprise": {"id": "E1", "name": "Org"}, "is_enterprise_install": true,
                "authed_user": {"id": "U1"}
            })),
            Some("good") => ok(json!({
                "ok": true, "access_token": SLACK_TOKEN, "token_type": "bot",
                "scope": "chat:write,commands", "bot_user_id": "UBOT", "app_id": "A0KRD7HC3",
                "team": {"name": "Slack Softball Team", "id": "T9TK3CUKW"},
                "enterprise": null, "is_enterprise_install": false,
                "authed_user": {"id": "U1234", "scope": "", "access_token": "xoxp-user", "token_type": "user"}
            })),
            _ => ok(json!({"ok": false, "error": "invalid_code"})),
        },
        "/api/conversations.history" => {
            // Only a top-level message at 100.0 exists; anything else is the message before.
            let ts = if s.query.get("latest").map(String::as_str) == Some("100.0") {
                "100.0"
            } else {
                "99.0"
            };
            ok(
                json!({"ok": true, "messages": [{"type": "message", "user": "U1", "text": "top", "ts": ts}], "has_more": true}),
            )
        }
        "/api/conversations.replies" => ok(json!({"ok": true, "messages": [
            {"type": "message", "user": "U1", "text": "root", "ts": "200.0", "thread_ts": "200.0"},
            {"type": "message", "bot_id": "B1", "text": "reply", "ts": "200.5", "thread_ts": "200.0"}
        ]})),
        "/api/fail.notFound" => ok(json!({"ok": false, "error": "channel_not_found"})),
        "/api/fail.internal" => ok(json!({"ok": false, "error": "internal_error"})),
        _ => (404, vec![], "not found".into()),
    }
}

#[tokio::test]
async fn slack_methods_send_the_bearer_token_and_json() {
    let (base, mock) = serve(slack_route).await;
    let client = slack::Client::new(&format!("{base}/api"));
    let m = slack::Message {
        channel: "C1".into(),
        text: "fallback".into(),
        blocks: Some(json!([{"type": "section", "text": {"type": "mrkdwn", "text": "hi"}}])),
        thread_ts: Some("1699999999.000001".into()),
    };
    let posted = client.post_message(SLACK_TOKEN, &m).await.unwrap();
    assert_eq!(
        posted,
        slack::Posted {
            channel: "C1".into(),
            ts: "1700000000.000100".into()
        }
    );
    client
        .update_message(
            SLACK_TOKEN,
            "C1",
            &posted.ts,
            &slack::Message {
                blocks: None,
                ..m.clone()
            },
        )
        .await
        .unwrap();
    client
        .post_ephemeral(SLACK_TOKEN, "C1", "U1", "only you")
        .await
        .unwrap();
    assert_eq!(client.open_dm(SLACK_TOKEN, "U1").await.unwrap(), "D42");
    client
        .open_view(SLACK_TOKEN, "trig", &json!({"type": "modal"}))
        .await
        .unwrap();
    client
        .add_reaction(SLACK_TOKEN, "C1", "1.2", "eyes")
        .await
        .unwrap();
    let link = client
        .permalink(SLACK_TOKEN, "C1", "1700000000.000100")
        .await
        .unwrap();
    assert!(link.ends_with("p1700000000000100"));

    let seen = seen(&mock);
    assert_eq!(seen.len(), 7);
    for s in &seen {
        assert_eq!(
            s.auth.as_deref(),
            Some(&*format!("Bearer {SLACK_TOKEN}")),
            "{}",
            s.path
        );
    }
    for s in &seen[..6] {
        assert_eq!(s.method, "POST");
        assert_eq!(
            s.content_type.as_deref(),
            Some("application/json; charset=utf-8")
        );
    }
    let post = seen[0].json();
    assert_eq!(post["channel"], "C1");
    assert_eq!(post["text"], "fallback");
    assert_eq!(post["thread_ts"], "1699999999.000001");
    assert_eq!(post["blocks"][0]["type"], "section");
    let update = seen[1].json();
    assert_eq!(
        (update["channel"].as_str(), update["ts"].as_str()),
        (Some("C1"), Some("1700000000.000100"))
    );
    assert_eq!(update["blocks"], json!([]));
    assert_eq!(seen[2].json()["user"], "U1");
    assert_eq!(seen[3].json(), json!({"users": "U1"}));
    assert_eq!(seen[4].json()["trigger_id"], "trig");
    assert_eq!(
        seen[5].json(),
        json!({"channel": "C1", "timestamp": "1.2", "name": "eyes"})
    );
    assert_eq!(seen[6].method, "GET");
    assert_eq!(seen[6].query["message_ts"], "1700000000.000100");
}

#[tokio::test]
async fn slack_oauth_sends_a_form_and_reads_the_install() {
    let (base, mock) = serve(slack_route).await;
    let client = slack::Client::new(&format!("{base}/api"));
    let access = client
        .oauth_access(
            "123.456",
            "client-secret-value",
            "good",
            "https://roll.sh/cb",
        )
        .await
        .unwrap();
    assert_eq!(access.access_token, SLACK_TOKEN);
    assert_eq!(
        (access.team_id.as_str(), access.team_name.as_str()),
        ("T9TK3CUKW", "Slack Softball Team")
    );
    assert_eq!(
        (access.bot_user_id.as_str(), access.app_id.as_str()),
        ("UBOT", "A0KRD7HC3")
    );
    assert_eq!(
        (access.installer.as_str(), access.enterprise_id.as_deref()),
        ("U1234", None)
    );
    assert!(!format!("{access:?}").contains(SLACK_TOKEN));

    let form = &seen(&mock)[0];
    assert_eq!(
        form.content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(form.auth, None);
    let f = form.form();
    assert_eq!(f["code"], "good");
    assert_eq!(f["client_id"], "123.456");
    assert_eq!(f["client_secret"], "client-secret-value");
    assert_eq!(f["redirect_uri"], "https://roll.sh/cb");

    let org = client
        .oauth_access("1", "client-secret-value", "org", "x")
        .await
        .unwrap_err();
    assert_eq!(org, Error::Api("org_install_unsupported".into()));
    let bad = client
        .oauth_access("1", "client-secret-value", "nope", "x")
        .await
        .unwrap_err();
    assert_eq!(bad, Error::Api("invalid_code".into()));
    assert_secret_free(&bad);
}

#[tokio::test]
async fn slack_fetch_message_from_history_then_replies() {
    let (base, mock) = serve(slack_route).await;
    let client = slack::Client::new(&format!("{base}/api"));
    let top = client
        .fetch_message(SLACK_TOKEN, "C1", "100.0")
        .await
        .unwrap();
    assert_eq!(
        top,
        Some(slack::Fetched {
            user: Some("U1".into()),
            text: "top".into(),
            thread_ts: None
        })
    );
    let reply = client
        .fetch_message(SLACK_TOKEN, "C1", "200.5")
        .await
        .unwrap();
    assert_eq!(
        reply,
        Some(slack::Fetched {
            user: None,
            text: "reply".into(),
            thread_ts: Some("200.0".into())
        })
    );
    assert_eq!(
        client
            .fetch_message(SLACK_TOKEN, "C1", "300.0")
            .await
            .unwrap(),
        None
    );

    let seen = seen(&mock);
    let paths: Vec<&str> = seen.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/api/conversations.history",
            "/api/conversations.history",
            "/api/conversations.replies",
            "/api/conversations.history",
            "/api/conversations.replies"
        ]
    );
    let h = &seen[0];
    assert_eq!(h.method, "GET");
    assert_eq!(h.auth.as_deref(), Some(&*format!("Bearer {SLACK_TOKEN}")));
    assert_eq!(
        (h.query["channel"].as_str(), h.query["latest"].as_str()),
        ("C1", "100.0")
    );
    assert_eq!(
        (h.query["inclusive"].as_str(), h.query["limit"].as_str()),
        ("true", "1")
    );
    assert_eq!(
        (
            seen[2].query["channel"].as_str(),
            seen[2].query["ts"].as_str()
        ),
        ("C1", "200.5")
    );
}

#[tokio::test]
async fn slack_errors_rate_limits_and_size_cap() {
    let (base, _mock) = serve(|s: &Seen| match s.path.as_str() {
        "/api/chat.postMessage" => (429, vec![("retry-after", "7".into())], "{}".into()),
        "/api/chat.update" => (503, vec![], "".into()),
        "/api/conversations.open" => (200, vec![], huge()),
        "/api/views.open" => (200, vec![], "<html>".into()),
        "/api/reactions.add" => ok(json!({"ok": false, "error": "channel_not_found"})),
        "/api/chat.postEphemeral" => ok(json!({"ok": false, "error": "internal_error"})),
        _ => (404, vec![], "".into()),
    })
    .await;
    let client = slack::Client::new(&format!("{base}/api"));
    let m = slack::Message {
        channel: "C1".into(),
        text: "x".into(),
        blocks: None,
        thread_ts: None,
    };
    let errors = [
        client.post_message(SLACK_TOKEN, &m).await.unwrap_err(),
        client
            .update_message(SLACK_TOKEN, "C1", "1.1", &m)
            .await
            .unwrap_err(),
        client.open_dm(SLACK_TOKEN, "U1").await.unwrap_err(),
        client
            .open_view(SLACK_TOKEN, "t", &json!({}))
            .await
            .unwrap_err(),
        client
            .add_reaction(SLACK_TOKEN, "C1", "1.1", "eyes")
            .await
            .unwrap_err(),
        client
            .post_ephemeral(SLACK_TOKEN, "C1", "U1", "x")
            .await
            .unwrap_err(),
        client
            .permalink(SLACK_TOKEN, "C1", "1.1")
            .await
            .unwrap_err(),
    ];
    assert_eq!(errors[0], Error::RateLimited { retry_after_s: 7 });
    assert!(matches!(&errors[1], Error::Http(m) if m.contains("503")));
    assert!(
        matches!(&errors[2], Error::Decode(m) if m.contains("too large")),
        "{:?}",
        errors[2]
    );
    assert!(matches!(errors[3], Error::Decode(_)));
    assert_eq!(errors[4], Error::Api("channel_not_found".into()));
    assert_eq!(errors[5], Error::Api("internal_error".into()));
    assert_eq!(errors[6], Error::Api("http_404".into()));
    let transient: Vec<bool> = errors.iter().map(Error::transient).collect();
    assert_eq!(transient, [true, true, false, false, false, true, false]);
    errors.iter().for_each(assert_secret_free);

    // Nothing listening: a transport error, still without the token.
    let closed = slack::Client::new("http://127.0.0.1:1/api");
    let e = closed.post_message(SLACK_TOKEN, &m).await.unwrap_err();
    assert!(matches!(e, Error::Http(_)) && e.transient());
    assert_secret_free(&e);
}

// ── Discord ──

fn discord_route(s: &Seen) -> Answer {
    match (s.method.as_str(), s.path.as_str()) {
        ("POST", "/api/v10/channels/600/messages") => {
            ok(json!({"id": "900", "channel_id": "600", "content": ""}))
        }
        ("PATCH", "/api/v10/channels/600/messages/900") => {
            ok(json!({"id": "900", "channel_id": "600"}))
        }
        ("POST", "/api/v10/users/@me/channels") => ok(json!({"id": "4242", "type": 1})),
        ("PUT", "/api/v10/applications/99/commands")
        | ("PUT", "/api/v10/applications/99/guilds/500/commands") => {
            ok(json!([{"id": "1", "name": "roll"}]))
        }
        ("POST", "/api/v10/oauth2/token") => match s.form().get("code").map(String::as_str) {
            Some("good") => ok(json!({
                "access_token": "user-access", "token_type": "Bearer", "expires_in": 604800,
                "refresh_token": "r", "scope": "bot applications.commands",
                "guild": {"id": "500", "name": "roll friends", "owner_id": "7"}
            })),
            Some("noguild") => {
                ok(json!({"access_token": "a", "token_type": "Bearer", "scope": "identify"}))
            }
            _ => (
                400,
                vec![],
                json!({"error": "invalid_grant", "error_description": "Invalid \"code\""})
                    .to_string(),
            ),
        },
        ("POST", "/api/v10/channels/429/messages") => (
            429,
            vec![("retry-after", "9".into())],
            json!({"message": "You are being rate limited.", "retry_after": 1.5, "global": false})
                .to_string(),
        ),
        ("POST", "/api/v10/channels/403/messages") => (
            403,
            vec![],
            json!({"message": "Missing Access", "code": 50001}).to_string(),
        ),
        ("POST", "/api/v10/channels/502/messages") => (502, vec![], "".into()),
        ("POST", "/api/v10/channels/huge/messages") => (200, vec![], huge()),
        _ => (
            404,
            vec![],
            json!({"message": "Unknown", "code": 0}).to_string(),
        ),
    }
}

#[tokio::test]
async fn discord_calls_send_the_bot_token_and_json() {
    let (base, mock) = serve(discord_route).await;
    let client = discord::Client::new(&format!("{base}/api/v10"), DISCORD_TOKEN);
    let payload = json!({"content": "hi", "components": []});
    let posted = client.create_message("600", &payload).await.unwrap();
    assert_eq!(
        posted,
        discord::Posted {
            channel: "600".into(),
            id: "900".into()
        }
    );
    client.edit_message("600", "900", &payload).await.unwrap();
    assert_eq!(client.dm_channel("7").await.unwrap(), "4242");
    client.register_commands("99", None).await.unwrap();
    client.register_commands("99", Some("500")).await.unwrap();
    assert!(!format!("{client:?}").contains(DISCORD_TOKEN));

    let seen = seen(&mock);
    for s in &seen {
        assert_eq!(
            s.auth.as_deref(),
            Some(&*format!("Bot {DISCORD_TOKEN}")),
            "{}",
            s.path
        );
        assert_eq!(s.content_type.as_deref(), Some("application/json"));
    }
    assert_eq!(seen[0].json(), payload);
    assert_eq!(seen[1].method, "PATCH");
    assert_eq!(seen[2].json(), json!({"recipient_id": "7"}));
    let commands = seen[3].json();
    assert_eq!(seen[3].method, "PUT");
    assert_eq!(commands[0]["name"], "roll");
    assert_eq!(commands[0]["options"][0]["name"], "prompt");
    assert_eq!(commands[0]["options"][0]["required"], true);
    assert_eq!(seen[4].path, "/api/v10/applications/99/guilds/500/commands");
}

#[tokio::test]
async fn discord_oauth_learns_the_guild_from_the_exchange() {
    let (base, mock) = serve(discord_route).await;
    let client = discord::Client::new(&format!("{base}/api/v10"), DISCORD_TOKEN);
    let guild = client
        .oauth_guild("99", "client-secret-value", "good", "https://roll.sh/cb")
        .await
        .unwrap();
    assert_eq!(guild, ("500".into(), "roll friends".into()));
    let form = &seen(&mock)[0];
    assert_eq!(
        form.content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        form.auth, None,
        "the exchange authenticates with the client secret, not the bot"
    );
    let f = form.form();
    assert_eq!(f["grant_type"], "authorization_code");
    assert_eq!(f["code"], "good");
    assert_eq!(f["client_id"], "99");
    assert_eq!(f["client_secret"], "client-secret-value");
    assert_eq!(f["redirect_uri"], "https://roll.sh/cb");

    let none = client
        .oauth_guild("99", "client-secret-value", "noguild", "x")
        .await
        .unwrap_err();
    assert_eq!(none, Error::Api("no_guild".into()));
    let bad = client
        .oauth_guild("99", "client-secret-value", "bad", "x")
        .await
        .unwrap_err();
    assert_eq!(bad, Error::Api("invalid_grant".into()));
    assert_secret_free(&bad);
}

#[tokio::test]
async fn discord_errors_rate_limits_and_size_cap() {
    let (base, _mock) = serve(discord_route).await;
    let client = discord::Client::new(&format!("{base}/api/v10"), DISCORD_TOKEN);
    let p = json!({"content": "x"});
    let errors = [
        client.create_message("429", &p).await.unwrap_err(),
        client.create_message("403", &p).await.unwrap_err(),
        client.create_message("502", &p).await.unwrap_err(),
        client.create_message("huge", &p).await.unwrap_err(),
        client.edit_message("600", "missing", &p).await.unwrap_err(),
    ];
    // The body's `retry_after` (1.5s) wins over the header, rounded up.
    assert_eq!(errors[0], Error::RateLimited { retry_after_s: 2 });
    assert_eq!(errors[1], Error::Api("50001: Missing Access".into()));
    assert!(matches!(&errors[2], Error::Http(m) if m.contains("502")));
    assert!(matches!(&errors[3], Error::Decode(m) if m.contains("too large")));
    assert_eq!(errors[4], Error::Api("0: Unknown".into()));
    let transient: Vec<bool> = errors.iter().map(Error::transient).collect();
    assert_eq!(transient, [true, false, true, false, false]);
    errors.iter().for_each(assert_secret_free);

    let closed = discord::Client::new("http://127.0.0.1:1/api/v10", DISCORD_TOKEN);
    let e = closed.dm_channel("7").await.unwrap_err();
    assert!(matches!(e, Error::Http(_)));
    assert_secret_free(&e);
}

// ── lists (the dashboard's pickers) ──

#[tokio::test]
async fn slack_lists_page_with_cursors_and_bounded_limits() {
    let (base, mock) = serve(|s: &Seen| match s.path.as_str() {
        "/api/conversations.list" => ok(json!({
            "ok": true,
            "channels": [
                {"id": "C1", "name": "general", "is_private": false, "is_member": true, "num_members": 12, "topic": {"value": "x"}},
                {"id": "G2", "name": "ops", "is_private": true, "is_member": true}
            ],
            "response_metadata": {"next_cursor": if s.query.contains_key("cursor") { "" } else { "dGVhbTpDMDYx" }}
        })),
        "/api/users.list" => ok(json!({
            "ok": true,
            "members": [{"id": "U1", "name": "ada", "real_name": "Ada Lovelace", "deleted": false, "is_bot": false,
                "profile": {"display_name": "ada", "image_48": "https://avatars.slack-edge.com/ada_48.png"}}],
            "response_metadata": {"next_cursor": ""}
        })),
        "/api/usergroups.list" => ok(json!({
            "ok": true,
            "usergroups": [{"id": "S1", "name": "On call", "handle": "oncall", "user_count": 3, "prefs": {}}]
        })),
        _ => (404, vec![], "".into()),
    })
    .await;
    let client = slack::Client::new(&format!("{base}/api"));
    let first = client.list_channels(SLACK_TOKEN, None, 5_000).await.unwrap();
    assert_eq!(first.next.as_deref(), Some("dGVhbTpDMDYx"));
    assert_eq!(
        first.items[0],
        slack::Channel {
            id: "C1".into(),
            name: "general".into(),
            is_private: false,
            is_member: true,
            num_members: Some(12)
        }
    );
    assert!(first.items[1].is_private && first.items[1].num_members.is_none());
    let last = client
        .list_channels(SLACK_TOKEN, first.next.as_deref(), 0)
        .await
        .unwrap();
    assert_eq!(last.next, None, "an empty next_cursor is the end");
    let users = client.list_users(SLACK_TOKEN, None, 50).await.unwrap();
    assert_eq!(users.items[0].real_name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(
        users.items[0].profile.image_48.as_deref(),
        Some("https://avatars.slack-edge.com/ada_48.png")
    );
    let groups = client.list_usergroups(SLACK_TOKEN).await.unwrap();
    assert_eq!(
        (groups[0].handle.as_str(), groups[0].user_count),
        ("oncall", Some(3))
    );

    let seen = seen(&mock);
    for s in &seen {
        assert_eq!(s.method, "GET");
        assert_eq!(s.auth.as_deref(), Some(&*format!("Bearer {SLACK_TOKEN}")));
    }
    assert_eq!(seen[0].query["types"], "public_channel,private_channel");
    assert_eq!(seen[0].query["exclude_archived"], "true");
    assert_eq!(seen[0].query["limit"], "200", "a limit is at most 200");
    assert!(!seen[0].query.contains_key("cursor"));
    assert_eq!(
        (seen[1].query["cursor"].as_str(), seen[1].query["limit"].as_str()),
        ("dGVhbTpDMDYx", "1")
    );
    assert_eq!(seen[2].query["limit"], "50");
    assert_eq!(seen[3].query["include_disabled"], "false");

    // A list over the size cap is refused, not read.
    let (base, _) = serve(|_: &Seen| (200, vec![], huge())).await;
    let e = slack::Client::new(&format!("{base}/api"))
        .list_users(SLACK_TOKEN, None, 200)
        .await
        .unwrap_err();
    assert!(matches!(&e, Error::Decode(m) if m.contains("too large")), "{e:?}");
}

#[tokio::test]
async fn discord_lists_a_guilds_channels_roles_and_members() {
    let (base, mock) = serve(|s: &Seen| match s.path.as_str() {
        "/api/v10/guilds/500/channels" => ok(json!([
            {"id": "10", "type": 4, "name": "Work", "position": 0},
            {"id": "11", "type": 0, "name": "general", "parent_id": "10", "position": 1, "topic": null}
        ])),
        "/api/v10/guilds/500/roles" => ok(json!([
            {"id": "500", "name": "@everyone", "color": 0, "position": 0, "managed": false},
            {"id": "21", "name": "Maintainers", "color": 5_017_087, "position": 2, "managed": false}
        ])),
        "/api/v10/guilds/500/members" | "/api/v10/guilds/500/members/search" => ok(json!([
            {"user": {"id": "7", "username": "ada", "global_name": "Ada", "avatar": "abc", "bot": false}, "nick": "ada (ops)", "roles": []}
        ])),
        "/api/v10/guilds/500/members/7" => ok(json!({"user": {"id": "7", "username": "ada"}})),
        _ => (
            404,
            vec![],
            json!({"message": "Unknown Member", "code": 10007}).to_string(),
        ),
    })
    .await;
    let client = discord::Client::new(&format!("{base}/api/v10"), DISCORD_TOKEN);
    let channels = client.guild_channels("500").await.unwrap();
    assert_eq!(
        (channels[1].kind, channels[1].parent_id.as_deref()),
        (0, Some("10"))
    );
    let roles = client.guild_roles("500").await.unwrap();
    assert_eq!(
        (roles[1].name.as_str(), roles[1].color),
        ("Maintainers", 5_017_087)
    );
    let found = client.search_guild_members("500", "ad a", 0).await.unwrap();
    assert_eq!(found[0].nick.as_deref(), Some("ada (ops)"));
    assert_eq!(found[0].user.avatar.as_deref(), Some("abc"));
    client
        .list_guild_members("500", Some("6"), 50_000)
        .await
        .unwrap();
    client.list_guild_members("500", None, 10).await.unwrap();
    assert!(client.guild_member("500", "7").await.unwrap().is_some());
    assert_eq!(
        client.guild_member("500", "8").await.unwrap(),
        None,
        "not a member"
    );

    let seen = seen(&mock);
    for s in &seen {
        assert_eq!(
            (s.method.as_str(), s.auth.as_deref()),
            ("GET", Some(&*format!("Bot {DISCORD_TOKEN}")))
        );
    }
    assert_eq!(seen[2].query["query"], "ad a");
    assert_eq!(seen[2].query["limit"], "1");
    assert_eq!(
        (seen[3].query["limit"].as_str(), seen[3].query["after"].as_str()),
        ("1000", "6"),
        "a page is at most 1000"
    );
    assert!(!seen[4].query.contains_key("after"));
}
