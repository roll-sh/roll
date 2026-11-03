//! GitHub App integration for roll code (docs/code/architecture.md, "GitHub-connected
//! repositories"). A library with no knowledge of roll's own records:
//!
//! - [`AppConfig`]: the App's id, private key, webhook secret and slug.
//! - [`auth`]: RS256 App JWTs and cached installation tokens.
//! - [`webhook`]: `X-Hub-Signature-256` verification and typed events.
//! - [`client::GitHub`]: the REST calls code-control needs, with rate-limit backoff.
//! - [`mapping`]: GitHub issues, PRs and pushes as plain drafts for code-control to translate.
//! - [`mirror::Mirror`]: when to push landed heads to GitHub, so thousands of landings become
//!   one fast-forward push.

pub mod auth;
pub mod client;
pub mod config;
pub mod mapping;
pub mod mirror;
pub mod model;
pub mod webhook;

pub use auth::{AppAuth, InstallationToken};
pub use client::{BranchProtection, ClientOptions, GitCredentials, GitHub, RateLimit};
pub use config::AppConfig;
pub use mapping::{ChangeDraft, DraftState, PrState, PushCommit, PushDraft, WorkItemDraft};
pub use mirror::{
    Ancestry, Blocked, BlockedKind, Mirror, MirrorAction, MirrorConfig, MirrorStatus, PushFailure,
};
pub use webhook::{Event, verify_signature};

/// Errors from the GitHub API and from App configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("github app is not configured: {0}")]
    Config(String),
    #[error("github auth: {0}")]
    Auth(String),
    #[error("github request: {0}")]
    Http(#[from] reqwest::Error),
    #[error("github {status}: {message}")]
    Status { status: u16, message: String },
    #[error("github rate limit exhausted; resets at unix {reset}")]
    RateLimited { reset: u64 },
    #[error("github response: {0}")]
    Decode(String),
}

impl Error {
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
