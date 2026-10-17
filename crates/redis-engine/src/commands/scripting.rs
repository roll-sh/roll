//! Lua scripting and functions. (Stub: to be implemented.)

use super::*;

pub static COMMANDS: &[Command] = &[];

/// Per-shard scripting state (Lua VM, script cache, function libraries).
#[derive(Default)]
pub struct Scripting {}

impl Scripting {
    /// Bytes used by the Lua VM.
    pub fn memory(&self) -> usize {
        0
    }

    pub fn cached_scripts(&self) -> usize {
        0
    }
}

#[allow(dead_code)]
fn unused(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Ok)
}
