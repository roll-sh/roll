//! Credentials kept out of what agents write: [`roll_auth::credentials`], shared with roll code's
//! own MCP server. roll-mcp adds the values of roll secrets a token may reveal (`lib.rs`,
//! `secret_value_in`: roll secrets' own scan, as the token).

pub use roll_auth::credentials::{Found, MAX_CANDIDATES, candidates, check, read_by_others};
