//! The standalone gateway's config file (`roll gateway serve --config gateway.toml`), and its
//! hot reload: the file is read again when it changes (checked every 2 s) and applied live
//! with [`crate::Gateway::apply`]; a file that doesn't parse or check keeps the previous
//! settings and logs why.
//!
//! The file is [`agent_proto::GatewaySettings`] in TOML plus `[keys]`: `upstream = "env:VAR"`
//! or `"file:/path"` (never the key itself in the file). Client keys are made with
//! `roll gateway client add <name>`, which prints the key once.
//!
//! The store helper owns this module.
