//! ACL users and permission checks. (Minimal: every user may do everything. To be completed.)

use bytes::Bytes;
use std::sync::{Arc, OnceLock};

use crate::commands::Command;

#[derive(Clone, Debug)]
pub struct User {
    pub name: String,
    pub enabled: bool,
    pub nopass: bool,
}

impl User {
    pub fn default_user() -> Arc<User> {
        static DEFAULT: OnceLock<Arc<User>> = OnceLock::new();
        DEFAULT.get_or_init(|| Arc::new(User { name: "default".into(), enabled: true, nopass: true })).clone()
    }

    /// May this user run `cmd` with `args`? `Err` carries the ACL error text.
    pub fn check(&self, _cmd: &Command, _args: &[Bytes]) -> Result<(), String> {
        Ok(())
    }
}
