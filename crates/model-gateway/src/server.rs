//! The inbound HTTP API (docs/code/gateway.md, "Inbound"):
//!
//! - `POST /v1/messages`, `POST /v1/messages/count_tokens` (Anthropic Messages)
//! - `POST /v1/chat/completions` (OpenAI Chat)
//! - `POST /v1/responses` (OpenAI Responses)
//! - `GET /v1/models` (both shapes: OpenAI's `data[].id` plus Anthropic's fields)
//! - `GET /health` (no key: what a member's daemon checks before relying on it)
//!
//! A client key comes as `Authorization: Bearer`, `x-api-key` or `api-key`. Bodies are capped
//! (32 MiB), as are streamed events; a client's own auth and identity headers never go
//! upstream.
//!
//! The proxy helper owns this module.
