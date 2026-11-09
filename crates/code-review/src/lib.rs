//! roll's built-in code reviewer (docs/code/reviews.md, "roll's reviewer"). A library with no
//! HTTP routes of its own: code-control decides when to review, gathers the diff and the
//! repository context, and submits what comes back as a roll review.
//!
//! - [`provider`]: the model behind it. Anthropic's Messages API over raw HTTP (there's no
//!   official Rust SDK) or any OpenAI-compatible endpoint, with the same retry policy.
//! - [`settings`]: the `reviewer:` section of `.roll/review.yaml` and the repo's settings.
//! - [`mode`]: which depth a change needs (`auto`) from its risk.
//! - [`engine`]: one review. Chunks the diff by file group, runs the groups, verifies findings
//!   (`max` / `thorough`), and returns the walkthrough, findings, skipped files and token usage.
//! - [`pricing`]: what a review costs the provider and what roll charges for it.
//! - [`secret`]: bring-your-own keys, encrypted at rest.
//! - [`commands`]: `@roll …` in a thread.

pub mod commands;
pub mod engine;
pub mod glob;
pub mod mode;
pub mod pricing;
pub mod prompt;
pub mod provider;
pub mod schema;
pub mod secret;
pub mod settings;

pub use engine::{Engine, FileInput, Finding, Partial, ReviewInput, ReviewResult, Skipped};
pub use mode::{Mode, RiskInput, Speed};
pub use provider::{Effort, Provider, ProviderError, Usage};
pub use settings::ReviewerSettings;
