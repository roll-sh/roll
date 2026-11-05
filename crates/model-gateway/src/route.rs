//! Which upstreams serve a request, in order (docs/code/gateway.md, "Routing"): the most
//! specific route for the model (exact, then the longest prefix), else every enabled upstream
//! that serves the model in the settings' order; then health: open circuits and cooling
//! upstreams are skipped, and with affinity on a session goes back to the upstream it started
//! on while that one is healthy.
//!
//! Failover is between upstreams on errors and outages (a connection failure, a timeout, 5xx,
//! Anthropic's 529 overloaded) before the first byte reaches the client. A 429 cools that
//! upstream down for its `retry-after` and only an upstream of another provider (another
//! kind, or another host) may take over: never another key of the same provider.
//!
//! The proxy helper owns this module.
