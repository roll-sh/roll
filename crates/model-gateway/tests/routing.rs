//! Routing: targets by routes and patterns, circuits, cool-downs, providers and affinity
//! (the state machine with a clock the test moves, and affinity through a running gateway).

mod common;

use agent_proto::{CircuitState, GatewaySettings, RouteSpec, RouteTarget, UpstreamKind};
use common::*;
use model_gateway::route::{self, Health, Target, Unavailable};
use std::time::Duration;

fn t(upstream: &str, model: &str) -> Target {
    Target { upstream: upstream.into(), model: model.into() }
}

#[test]
fn routes_pick_the_most_specific_then_upstreams_by_pattern_in_order() {
    let mut a = upstream("a", UpstreamKind::Anthropic, "https://api.anthropic.com");
    a.models = vec!["claude-*".into()];
    let mut b = upstream("b", UpstreamKind::Openai, "https://api.openai.com/v1");
    b.models = vec!["gpt-*".into(), "claude-opus-4-5".into()];
    let mut c = upstream("c", UpstreamKind::OpenaiCompatible, "https://models.example/v1");
    c.models = vec![];
    let mut off = upstream("off", UpstreamKind::Anthropic, "https://x.example");
    off.disabled = true;
    off.models = vec!["*".into()];
    let routes = vec![
        RouteSpec { model: "claude-*".into(), targets: vec![RouteTarget { upstream: "b".into(), model: None }, RouteTarget { upstream: "off".into(), model: None }] },
        RouteSpec { model: "claude-haiku-*".into(), targets: vec![RouteTarget { upstream: "a".into(), model: None }] },
        RouteSpec { model: "fast".into(), targets: vec![RouteTarget { upstream: "a".into(), model: Some("claude-haiku-4-5".into()) }, RouteTarget { upstream: "b".into(), model: Some("gpt-5-mini".into()) }] },
    ];
    let s = GatewaySettings { upstreams: vec![a, b, c, off], routes, ..Default::default() }.check().unwrap();
    let unknown = |_: &str, _: &str| None;
    // An alias renames per target.
    assert_eq!(route::targets(&s, "fast", unknown), vec![t("a", "claude-haiku-4-5"), t("b", "gpt-5-mini")]);
    // The longest prefix wins; disabled upstreams drop out.
    assert_eq!(route::targets(&s, "claude-haiku-4-5", unknown), vec![t("a", "claude-haiku-4-5")]);
    assert_eq!(route::targets(&s, "claude-opus-4-5", unknown), vec![t("b", "claude-opus-4-5")]);
    // No route: upstreams whose patterns match, then those without patterns unless their list
    // says otherwise.
    assert_eq!(route::targets(&s, "gpt-5", unknown), vec![t("b", "gpt-5"), t("c", "gpt-5")]);
    assert_eq!(route::targets(&s, "gpt-5", |u, m| Some(u == "c" && m == "other")), vec![t("b", "gpt-5")]);
    assert_eq!(route::targets(&s, "llama", |_, _| Some(false)), vec![]);
}

#[test]
fn providers_are_kinds_or_hosts() {
    let ant1 = upstream("a1", UpstreamKind::Anthropic, "https://api.anthropic.com");
    let mut ant2 = upstream("a2", UpstreamKind::Anthropic, "https://proxy.corp.example");
    ant2.base_url = None;
    let compat_same_host = upstream("c1", UpstreamKind::AnthropicCompatible, "https://api.anthropic.com/");
    let compat_other = upstream("c2", UpstreamKind::AnthropicCompatible, "https://api.z.example");
    let compat_other_2 = upstream("c3", UpstreamKind::OpenaiCompatible, "https://api.z.example/v1");
    let mut bedrock_east = upstream("b1", UpstreamKind::Bedrock, "");
    bedrock_east.base_url = None;
    bedrock_east.region = Some("us-east-1".into());
    let mut bedrock_west = bedrock_east.clone();
    bedrock_west.region = Some("us-west-2".into());
    assert!(route::same_provider(&ant1, &ant2));
    assert!(route::same_provider(&ant1, &compat_same_host));
    assert!(!route::same_provider(&ant1, &compat_other));
    assert!(route::same_provider(&compat_other, &compat_other_2));
    assert!(route::same_provider(&bedrock_east, &bedrock_west));
    assert!(!route::same_provider(&bedrock_east, &ant1));
    assert_eq!(route::host_of(&bedrock_west), "bedrock-runtime.us-west-2.amazonaws.com");
}

#[test]
fn a_circuit_opens_half_opens_and_closes() {
    let h = Health::new();
    let ms = |d: Duration| d.as_millis() as u64;
    let mut now = 1_000_000;
    let both = || vec![t("a", "m"), t("b", "m")];
    for i in 0..route::OPEN_AFTER {
        assert_eq!(h.health("a", now).circuit, CircuitState::Closed, "after {i}");
        assert!(h.admit("a", now));
        h.failure("a", "500", now);
    }
    let health = h.health("a", now);
    assert_eq!((health.circuit, health.failures), (CircuitState::Open, 5));
    assert_eq!(health.open_until, Some(now + ms(route::OPEN_FIRST)));
    assert_eq!(h.order(both(), None, now).unwrap(), vec![t("b", "m")]);
    assert!(!h.admit("a", now));

    // Half open: one request goes through, the next waits for it.
    now += ms(route::OPEN_FIRST);
    assert_eq!(h.health("a", now).circuit, CircuitState::HalfOpen);
    assert_eq!(h.order(both(), None, now).unwrap(), both());
    assert!(h.admit("a", now));
    assert!(!h.admit("a", now), "a half-open circuit let two through");
    assert_eq!(h.order(both(), None, now).unwrap(), vec![t("b", "m")]);
    // It fails: open again, for twice as long.
    h.failure("a", "502", now);
    assert_eq!(h.health("a", now).open_until, Some(now + 2 * ms(route::OPEN_FIRST)));
    // Each reopening doubles, up to the cap.
    for _ in 0..10 {
        now = h.health("a", now).open_until.unwrap();
        assert!(h.admit("a", now));
        h.failure("a", "502", now);
    }
    assert_eq!(h.health("a", now).open_until, Some(now + ms(route::OPEN_MAX)));
    // A probe that never reports frees itself.
    now = h.health("a", now).open_until.unwrap();
    assert!(h.admit("a", now));
    assert!(!h.admit("a", now + 1000));
    assert!(h.admit("a", now + ms(route::PROBE_MAX)));
    // A success closes it.
    h.success("a", now);
    let health = h.health("a", now);
    assert_eq!((health.circuit, health.failures, health.open_until), (CircuitState::Closed, 0, None));
    assert_eq!(health.last_ok_at, Some(now));
    assert!(health.last_error.unwrap().contains("502"));

    // Every target open: nothing to try.
    let only = vec![t("c", "m")];
    for _ in 0..route::OPEN_AFTER {
        h.failure("c", "boom", now);
    }
    assert_eq!(h.order(only, None, now), Err(Unavailable::Open));
}

#[test]
fn a_429_cools_down_for_retry_after_within_bounds() {
    let h = Health::new();
    let now = 5_000_000;
    h.rate_limited("a", Some(Duration::from_secs(7)), now);
    assert_eq!(h.cooling_until("a", now), Some(now + 7000));
    assert_eq!(h.health("a", now).circuit, CircuitState::Closed, "a 429 isn't a failure");
    assert_eq!(h.order(vec![t("a", "m")], None, now), Err(Unavailable::Cooling { until: now + 7000 }));
    assert_eq!(h.order(vec![t("a", "m")], None, now + 7000).unwrap(), vec![t("a", "m")]);
    h.rate_limited("b", None, now);
    assert_eq!(h.cooling_until("b", now), Some(now + route::COOL_DEFAULT.as_millis() as u64));
    h.rate_limited("c", Some(Duration::from_secs(86_400)), now);
    assert_eq!(h.cooling_until("c", now), Some(now + route::COOL_MAX.as_millis() as u64));
}

#[test]
fn affinity_is_bounded_and_forgets() {
    let h = Health::new();
    let now = 10_000_000;
    h.bind("s1", "b", now);
    assert_eq!(h.order(vec![t("a", "m"), t("b", "m")], Some("s1"), now).unwrap(), vec![t("b", "m"), t("a", "m")]);
    // An hour later it's forgotten.
    let later = now + route::AFFINITY_TTL.as_millis() as u64;
    assert_eq!(h.session("s1", later), None);
    // Unhealthy: the session goes elsewhere.
    for _ in 0..route::OPEN_AFTER {
        h.failure("b", "x", now);
    }
    assert_eq!(h.order(vec![t("a", "m"), t("b", "m")], Some("s1"), now).unwrap(), vec![t("a", "m")]);
    // Bounded.
    for i in 0..route::AFFINITY_MAX + 50 {
        h.bind(&format!("s{i}"), "a", now + i as u64);
    }
    assert!(h.sessions() <= route::AFFINITY_MAX);
    // Gone upstreams are forgotten.
    h.retain(&["z"]);
    assert_eq!(h.sessions(), 0);
}

#[test]
fn session_ids_come_from_explicit_headers_then_the_body() {
    let mut headers = axum::http::HeaderMap::new();
    let body = serde_json::json!({ "prompt_cache_key": "pck-1", "metadata": { "user_id": "user_abc_session_x" } });
    assert_eq!(route::session_id(&headers, &body).as_deref(), Some("pck:pck-1"));
    assert_eq!(route::session_id(&headers, &serde_json::json!({ "metadata": { "user_id": "u" } })).as_deref(), Some("user:u"));
    headers.insert("x-session-affinity", "aff".parse().unwrap());
    assert_eq!(route::session_id(&headers, &body).as_deref(), Some("affinity:aff"));
    headers.insert("session_id", "codex-1".parse().unwrap());
    assert_eq!(route::session_id(&headers, &body).as_deref(), Some("session:codex-1"));
    headers.insert("x-claude-code-session-id", "cc-1".parse().unwrap());
    assert_eq!(route::session_id(&headers, &body).as_deref(), Some("claude:cc-1"));
    // Nothing explicit: no session (no hashing of messages).
    assert_eq!(route::session_id(&axum::http::HeaderMap::new(), &serde_json::json!({ "messages": [{ "role": "user", "content": "hi" }] })), None);
}

#[tokio::test]
async fn a_session_stays_on_the_upstream_it_started_on() {
    let first = Mock::start(|_| status(500)).await;
    let second = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let ups = vec![upstream("first", UpstreamKind::AnthropicCompatible, &first.url("")), upstream("second", UpstreamKind::AnthropicCompatible, &second.url(""))];
    let g = Gw::start(ups.clone(), vec![], &[]).await;
    let send = |session: Option<&'static str>| {
        let mut r = g.http.post(g.url("/v1/messages")).bearer_auth(&g.key).json(&messages("claude-x", false));
        if let Some(s) = session {
            r = r.header("x-claude-code-session-id", s);
        }
        r.send()
    };
    // `first` fails, so the session starts on `second`.
    assert_eq!(send(Some("sess-1")).await.unwrap().status(), 200);
    assert_eq!((first.count(), second.count()), (1, 1));
    // `first` recovers (another mock at its place): the session stays on `second`, a request
    // without one goes to `first`.
    let healthy = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let ups = vec![upstream("first", UpstreamKind::AnthropicCompatible, &healthy.url("")), ups[1].clone()];
    g.gw.apply(settings(g.port, ups.clone(), vec![]), keys(&[])).await;
    assert_eq!(send(Some("sess-1")).await.unwrap().status(), 200);
    assert_eq!((healthy.count(), second.count()), (0, 2));
    assert_eq!(send(None).await.unwrap().status(), 200);
    assert_eq!((healthy.count(), second.count()), (1, 2));

    // Affinity off: priority order.
    let mut s = settings(g.port, ups, vec![]);
    s.affinity = false;
    g.gw.apply(s, keys(&[])).await;
    assert_eq!(send(Some("sess-1")).await.unwrap().status(), 200);
    assert_eq!((healthy.count(), second.count()), (2, 2));
}
