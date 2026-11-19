//! roll secrets' gateway into private networks (docs/secrets.md, "Gateways"): a process inside a
//! team's network dials out to roll over one WebSocket, and roll's conversations with private
//! hosts travel through it as byte streams.
//!
//! - [`proto`]: the frames, their limits and close codes.
//! - [`mux`]: streams multiplexed over one link, with credit-based flow control, used by both
//!   sides (rolld opens streams, the gateway accepts them).
//! - [`allow`]: the gateway's own allow-list, which decides what it reaches.
//! - `agent` (feature `agent`): the gateway's loop, `roll gateway up`.
//!
//! No TLS is terminated here: rolld runs TLS end to end with the server through a stream, so the
//! gateway relays ciphertext.

pub mod allow;
pub mod mux;
pub mod proto;

#[cfg(feature = "agent")]
pub mod agent;
