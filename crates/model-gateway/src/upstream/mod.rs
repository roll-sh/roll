//! Each upstream kind (docs/code/gateway.md, "Upstreams"): the URL for a request in the API it
//! speaks, its auth and its stream framing.
//!
//! - Anthropic and Anthropic-compatible: `<base>/v1/messages`, `x-api-key`, `anthropic-version`.
//! - OpenAI, OpenRouter, OpenAI-compatible: `<base>/chat/completions` and `<base>/responses`,
//!   a bearer key.
//! - Azure OpenAI: `<base>/openai/v1/{chat/completions,responses}`, the `api-key` header.
//! - Bedrock: `bedrock-runtime.<region>.amazonaws.com/model/<id>/invoke(-with-response-stream)`,
//!   a Bedrock API key (bearer) or SigV4; its event stream decoded into Messages events.
//! - Vertex: `<region>-aiplatform.googleapis.com/v1/projects/<p>/locations/<r>/publishers/
//!   anthropic/models/<m>:(stream)rawPredict` for Claude, the `endpoints/openapi/chat/
//!   completions` endpoint for other models; an access token from the service account's JWT,
//!   cached until it expires.
//!
//! The proxy helper owns this module.
