//! roll's MCP Server Card (SEP-2127, the experimental `server-card` extension): what a registry,
//! a crawler or a client learns about roll's MCP server before it connects. It holds who the server
//! is and where to connect (the streamable HTTP URL, the protocol versions `server/discover`
//! answers), never tools or anything a token decides. Public and the same for everyone.
//!
//! - `GET /v1/mcp/server-card`: the card, at the location MCP reserves (`<streamable-http-url>/server-card`).
//! - `GET /.well-known/ai-catalog.json`: the domain's AI Catalog, naming the card.

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

pub const CARD_TYPE: &str = "application/mcp-server-card+json";
const CATALOG_TYPE: &str = "application/ai-catalog+json";
const SCHEMA: &str = "https://static.modelcontextprotocol.io/schemas/v1/server-card.schema.json";

/// The card for a daemon whose API is at `api_url` and dashboard at `web_url`.
pub fn card(api_url: &str, web_url: &str) -> Value {
    let api = api_url.trim_end_matches('/');
    json!({
        "$schema": SCHEMA,
        "name": "sh.roll/roll",
        "version": env!("CARGO_PKG_VERSION"),
        "title": "roll",
        "description": "roll's products over MCP: databases, code, apps, secrets and search, within a token's rules",
        "websiteUrl": web_url.trim_end_matches('/'),
        "icons": [crate::server_icon()],
        "remotes": [{
            "type": "streamable-http",
            "url": format!("{api}/v1/mcp"),
            // Optional: without one, the server answers 401 naming its OAuth metadata.
            "headers": [{
                "name": "Authorization",
                "description": "A roll token as a bearer token. Without one, sign in with OAuth.",
                "isRequired": false,
                "isSecret": true,
                "value": "Bearer {token}",
                "variables": { "token": { "description": "A roll token (rlt_…)", "isRequired": true, "isSecret": true } },
            }],
            "supportedProtocolVersions": crate::supported_versions(),
        }],
    })
}

/// The AI Catalog for the API's domain: one entry, roll's card.
pub fn catalog(api_url: &str) -> Value {
    let api = api_url.trim_end_matches('/');
    let host = api.split("://").nth(1).unwrap_or(api).split(['/', ':']).next().unwrap_or_default();
    json!({
        "specVersion": "1.0",
        "entries": [{ "identifier": format!("urn:air:{host}:mcp:roll"), "type": CARD_TYPE, "url": format!("{api}/v1/mcp/server-card") }],
    })
}

fn answer(body: &Value, kind: &'static str) -> Response {
    let mut res = axum::Json(body).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    // The same for everyone, and it changes only with an upgrade.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600"));
    res
}

/// The card's and the catalog's routes: public, merged in front of the gate.
pub fn routes(api_url: &str, web_url: &str) -> Router {
    let (card, catalog) = (card(api_url, web_url), catalog(api_url));
    Router::new()
        .route("/v1/mcp/server-card", get(move || async move { answer(&card, CARD_TYPE) }))
        .route("/.well-known/ai-catalog.json", get(move || async move { answer(&catalog, CATALOG_TYPE) }))
}
