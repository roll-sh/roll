//! List prices and cost: an estimate from public list prices (dated in the table), never a
//! bill. The owner's [`ModelPrice`]s come first; a model in neither costs 0 and the UI says
//! "no price".
//!
//! The store helper owns this module.

use crate::Usage;
use agent_proto::ModelPrice;

/// The price of `model` (the upstream's model name), the owner's first.
pub fn price_of(model: &str, overrides: &[ModelPrice]) -> Option<ModelPrice> {
    let _ = (model, overrides);
    None
}

/// Micro-dollars for `usage` of `model`.
pub fn cost_micros(model: &str, usage: Usage, overrides: &[ModelPrice]) -> u64 {
    let _ = (model, usage, overrides);
    0
}
