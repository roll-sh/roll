//! Workload identities' OIDC tokens are checked by `roll-oidc`, which roll-access's trusted
//! machine clients share, so the rules are one: shared issuers, key fetches, pinned keys, the
//! caps. This module only joins it to roll secrets: its guarded fetcher ([`crate::Deliver::get`])
//! and its errors.

pub(crate) use roll_oidc::{Jwt, Keys, Window, pinned_keys, wildcard};

use crate::Error;

/// The guarded client as roll-oidc's fetcher.
pub(crate) struct Via<'a>(pub &'a dyn crate::Deliver);

impl roll_oidc::Fetch for Via<'_> {
    fn get(&self, url: &str, limit: usize) -> std::result::Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        self.0.get(url, limit).map_err(|e| Box::new(e) as _)
    }
}

impl From<roll_oidc::Error> for Error {
    fn from(e: roll_oidc::Error) -> Self {
        match e {
            roll_oidc::Error::Invalid(m) => Error::Unauthorized(m),
            roll_oidc::Error::Unavailable(m) => Error::Link(m),
            // The fetcher's own error, answered as it was.
            roll_oidc::Error::Fetch(e) => match e.downcast::<Error>() {
                Ok(e) => *e,
                Err(e) => Error::Link(e.to_string()),
            },
            roll_oidc::Error::Off => Error::NotFound("workload identities aren't on this server".into()),
        }
    }
}
