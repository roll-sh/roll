//! Selectors: a set of command, key and channel permissions. Every user has a root selector and
//! any number of additional `(...)` selectors; a command is allowed if any selector allows it.
//! This follows Redis' `aclSelector` closely, including how rules are remembered for display
//! (`command_rules`: the ordered `+cmd`/`-@cat` rules applied after the last `+@all`/`-@all`).

use super::index::{self, BITS, Channels, FUTURE_BIT, Meta, PERM_R, PERM_W, Variable, index};
use crate::commands::{self, Command};
use crate::util::glob_match;
use bytes::Bytes;
use smallvec::SmallVec;

/// Why a rule was rejected (Redis' errno values in `ACLSetUser`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleError {
    /// EINVAL
    Syntax,
    /// ENOENT
    UnknownCommand,
    /// EEXIST
    KeyAfterAll,
    /// EISDIR
    ChannelAfterAll,
    /// ENODEV
    NoSuchPassword,
    /// EBADMSG
    BadHash,
    /// ECHILD
    FirstArgOfSub,
}

impl RuleError {
    pub fn message(self) -> &'static str {
        match self {
            RuleError::Syntax => "Syntax error",
            RuleError::UnknownCommand => "Unknown command or category name in ACL",
            RuleError::KeyAfterAll => {
                "Adding a pattern after the * pattern (or the 'allkeys' flag) is not valid and does not have any effect. Try 'resetkeys' to start with an empty list of patterns"
            }
            RuleError::ChannelAfterAll => {
                "Adding a pattern after the * pattern (or the 'allchannels' flag) is not valid and does not have any effect. Try 'resetchannels' to start with an empty list of channels"
            }
            RuleError::NoSuchPassword => "The password you are trying to remove from the user does not exist",
            RuleError::BadHash => "The password hash must be exactly 64 characters and contain only lowercase hexadecimal characters",
            RuleError::FirstArgOfSub => "Allowing first-arg of a subcommand is not supported",
        }
    }
}

/// What denied a command. Ordered like Redis' `ACL_DENIED_*` codes (the most specific error
/// across selectors is reported).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    Command = 1,
    Key = 2,
    Channel = 4,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPattern {
    /// `PERM_R | PERM_W`.
    pub perm: u8,
    pub pattern: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Selector {
    pub root: bool,
    pub allkeys: bool,
    pub allchannels: bool,
    pub allcommands: bool,
    bits: [u64; BITS / 64],
    /// Deprecated `+cmd|firstarg` rules: allowed first arguments per command id.
    firstargs: Vec<(u16, Vec<Vec<u8>>)>,
    pub keys: Vec<KeyPattern>,
    pub channels: Vec<Vec<u8>>,
    /// `+get`, `-@string`… in the order given (lowercase, without the leading `+@all`/`-@all`).
    rules: Vec<Vec<u8>>,
}

/// `isspace() || NUL`, like `ACLStringHasSpaces`.
pub fn has_spaces(s: &[u8]) -> bool {
    s.iter().any(|&c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0))
}

impl Selector {
    /// An empty selector (no permissions; `acl-pubsub-default` is `resetchannels`).
    pub fn new(root: bool) -> Self {
        Self {
            root,
            allkeys: false,
            allchannels: false,
            allcommands: false,
            bits: [0; BITS / 64],
            firstargs: Vec::new(),
            keys: Vec::new(),
            channels: Vec::new(),
            rules: Vec::new(),
        }
    }

    /// Allows every command, key and channel (fast path).
    pub fn unrestricted(&self) -> bool {
        self.allcommands && self.allkeys && self.allchannels
    }

    #[inline]
    fn bit(&self, id: u16) -> bool {
        self.bits[id as usize / 64] & (1 << (id % 64)) != 0
    }

    fn set_bit(&mut self, id: u16, on: bool) {
        if on {
            self.bits[id as usize / 64] |= 1 << (id % 64);
        } else {
            self.bits[id as usize / 64] &= !(1 << (id % 64));
            self.allcommands = false;
        }
    }

    /// `+@all` was the base: future commands are allowed too.
    pub fn future(&self) -> bool {
        self.bit(FUTURE_BIT)
    }

    /// `ACLChangeSelectorPerm`: a command and all its subcommands.
    fn change_perm(&mut self, id: u16, allow: bool) {
        self.set_bit(id, allow);
        self.firstargs.retain(|(i, _)| *i != id);
        for &s in &index().cmds[id as usize].subs {
            self.set_bit(s, allow);
        }
    }

    fn set_category(&mut self, bit: u32, allow: bool) {
        let idx = index();
        for &t in &idx.top {
            let m = &idx.cmds[t as usize];
            if m.cats & bit != 0 {
                self.change_perm(t, allow);
            }
            for &s in &m.subs {
                if idx.cmds[s as usize].cats & bit != 0 {
                    self.change_perm(s, allow);
                }
            }
        }
    }

    /// `ACLUpdateCommandRules`: drop earlier rules for the same command (and its subcommands)
    /// and append this one.
    fn update_rules(&mut self, rule: &[u8], allow: bool) {
        let rule = rule.to_ascii_lowercase();
        self.rules.retain(|r| {
            let name = &r[1..];
            !(name == &rule[..] || (name.len() > rule.len() && name.starts_with(&rule) && name[rule.len()] == b'|'))
        });
        let mut r = Vec::with_capacity(rule.len() + 1);
        r.push(if allow { b'+' } else { b'-' });
        r.extend_from_slice(&rule);
        self.rules.push(r);
    }

    fn all_commands(&mut self, allow: bool) {
        self.bits = if allow { [u64::MAX; BITS / 64] } else { [0; BITS / 64] };
        self.allcommands = allow;
        self.rules.clear();
        self.firstargs.clear();
    }

    /// Apply one selector rule (`ACLSetSelector`).
    pub fn apply(&mut self, op: &[u8]) -> Result<(), RuleError> {
        let is = |s: &str| op.eq_ignore_ascii_case(s.as_bytes());
        let at = |i: usize| op.get(i).copied().unwrap_or(0);
        if is("allkeys") || op == b"~*" {
            self.allkeys = true;
            self.keys.clear();
        } else if is("resetkeys") {
            self.allkeys = false;
            self.keys.clear();
        } else if is("allchannels") || op == b"&*" {
            self.allchannels = true;
            self.channels.clear();
        } else if is("resetchannels") {
            self.allchannels = false;
            self.channels.clear();
        } else if is("allcommands") || is("+@all") {
            self.all_commands(true);
        } else if is("nocommands") || is("-@all") {
            self.all_commands(false);
        } else if at(0) == b'~' || at(0) == b'%' {
            if self.allkeys {
                return Err(RuleError::KeyAfterAll);
            }
            let mut perm = 0;
            let mut off = 1;
            if at(0) == b'%' {
                let mut ok = true;
                while off < op.len() {
                    let c = op[off].to_ascii_uppercase();
                    if c == b'R' && perm & PERM_R == 0 {
                        perm |= PERM_R;
                    } else if c == b'W' && perm & PERM_W == 0 {
                        perm |= PERM_W;
                    } else if op[off] == b'~' {
                        off += 1;
                        break;
                    } else {
                        ok = false;
                        break;
                    }
                    off += 1;
                }
                if perm == 0 || !ok {
                    return Err(RuleError::Syntax);
                }
            } else {
                perm = PERM_R | PERM_W;
            }
            let pattern = &op[off.min(op.len())..];
            if has_spaces(pattern) {
                return Err(RuleError::Syntax);
            }
            match self.keys.iter_mut().find(|k| k.pattern == pattern) {
                Some(k) => k.perm |= perm,
                None => self.keys.push(KeyPattern { perm, pattern: pattern.to_vec() }),
            }
            self.allkeys = false;
        } else if at(0) == b'&' {
            if self.allchannels {
                return Err(RuleError::ChannelAfterAll);
            }
            let pattern = &op[1..];
            if has_spaces(pattern) {
                return Err(RuleError::Syntax);
            }
            if !self.channels.iter().any(|c| c == pattern) {
                self.channels.push(pattern.to_vec());
            }
            self.allchannels = false;
        } else if at(0) == b'+' && at(1) != b'@' {
            let name = &op[1..];
            match name.iter().rposition(|&c| c == b'|') {
                None => {
                    let cmd = index::lookup_full(name).ok_or(RuleError::UnknownCommand)?;
                    let m = meta(cmd);
                    self.change_perm(m.id, true);
                    let full = m.full.clone();
                    self.update_rules(full.as_bytes(), true);
                }
                Some(bar) => {
                    let (base, sub) = (&name[..bar], &name[bar + 1..]);
                    let cmd = index::lookup_full(base).ok_or(RuleError::UnknownCommand)?;
                    let m = meta(cmd);
                    if m.parent.is_some() {
                        return Err(RuleError::FirstArgOfSub);
                    }
                    if sub.is_empty() {
                        return Err(RuleError::Syntax);
                    }
                    if !cmd.subs.is_empty() {
                        let sub = index::lookup_full(name).ok_or(RuleError::UnknownCommand)?;
                        self.change_perm(meta(sub).id, true);
                    } else {
                        let id = m.id;
                        match self.firstargs.iter_mut().find(|(i, _)| *i == id) {
                            Some((_, list)) => {
                                if !list.iter().any(|a| a.eq_ignore_ascii_case(sub)) {
                                    list.push(sub.to_vec());
                                }
                            }
                            None => self.firstargs.push((id, vec![sub.to_vec()])),
                        }
                    }
                    self.update_rules(name, true);
                }
            }
        } else if at(0) == b'-' && at(1) != b'@' {
            let cmd = index::lookup_full(&op[1..]).ok_or(RuleError::UnknownCommand)?;
            let m = meta(cmd);
            self.change_perm(m.id, false);
            let full = m.full.clone();
            self.update_rules(full.as_bytes(), false);
        } else if (at(0) == b'+' || at(0) == b'-') && at(1) == b'@' {
            let allow = at(0) == b'+';
            let bit = index::category_bit(&op[2..]).ok_or(RuleError::UnknownCommand)?;
            self.update_rules(&op[1..], allow);
            self.set_category(bit, allow);
        } else {
            return Err(RuleError::Syntax);
        }
        Ok(())
    }

    /// `ACLDescribeSelectorCommandRules`: `+@all`/`-@all` followed by the remembered rules.
    pub fn describe_commands(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(if self.future() { b"+@all" } else { b"-@all" });
        for r in &self.rules {
            out.push(b' ');
            out.extend_from_slice(r);
        }
        out
    }

    /// Key patterns as `ACL GETUSER` shows them.
    pub fn describe_keys(&self) -> Vec<u8> {
        if self.allkeys {
            return b"~*".to_vec();
        }
        let mut out = Vec::new();
        for (i, k) in self.keys.iter().enumerate() {
            if i > 0 {
                out.push(b' ');
            }
            push_pattern(&mut out, k);
        }
        out
    }

    /// Channel patterns as `ACL GETUSER` shows them.
    pub fn describe_channels(&self) -> Vec<u8> {
        if self.allchannels {
            return b"&*".to_vec();
        }
        let mut out = Vec::new();
        for (i, c) in self.channels.iter().enumerate() {
            if i > 0 {
                out.push(b' ');
            }
            out.push(b'&');
            out.extend_from_slice(c);
        }
        out
    }

    /// `ACLDescribeSelector`: keys, channels and commands as `ACL LIST` shows them.
    pub fn describe(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.allkeys {
            out.extend_from_slice(b"~* ");
        } else {
            for k in &self.keys {
                push_pattern(&mut out, k);
                out.push(b' ');
            }
        }
        if self.allchannels {
            out.extend_from_slice(b"&* ");
        } else {
            out.extend_from_slice(b"resetchannels ");
            for c in &self.channels {
                out.push(b'&');
                out.extend_from_slice(c);
                out.push(b' ');
            }
        }
        out.extend_from_slice(&self.describe_commands());
        out
    }

    /// `ACLSelectorCheckCmd`. On denial returns the reason and the offending argument index (0
    /// for the command itself).
    pub(crate) fn check(&self, m: &Meta, args: &[Bytes], keys: &mut Keys) -> Result<(), (Reason, usize)> {
        if !self.allcommands && !m.cmd.has(commands::NO_AUTH) && !self.bit(m.id) {
            let ok = args.len() >= 2
                && self.firstargs.iter().find(|(i, _)| *i == m.id).is_some_and(|(_, list)| {
                    let idx = if m.parent.is_some() { 2 } else { 1 };
                    args.get(idx).is_some_and(|a| list.iter().any(|f| f.eq_ignore_ascii_case(a)))
                });
            if !ok {
                return Err((Reason::Command, 0));
            }
        }
        if !self.allkeys && m.has_keys {
            for (n, &pos) in keys.get(m.cmd, args).iter().enumerate() {
                let need = key_perm(m, n, args);
                let key = &args[pos];
                if !self.keys.iter().any(|k| k.perm & need == need && glob_match(&k.pattern, key, false)) {
                    return Err((Reason::Key, pos));
                }
            }
        }
        if !self.allchannels && m.channels != Channels::None {
            let (range, pattern) = match m.channels {
                Channels::Subscribe => (1..args.len(), false),
                Channels::Pattern => (1..args.len(), true),
                _ => (1..args.len().min(2), false),
            };
            for pos in range {
                if !channel_allowed(&self.channels, &args[pos], pattern) {
                    return Err((Reason::Channel, pos));
                }
            }
        }
        Ok(())
    }
}

fn meta(cmd: &Command) -> &'static Meta {
    index().meta(cmd).expect("command in the ACL index")
}

fn push_pattern(out: &mut Vec<u8>, k: &KeyPattern) {
    match k.perm {
        p if p == PERM_R | PERM_W => out.push(b'~'),
        PERM_R => out.extend_from_slice(b"%R~"),
        _ => out.extend_from_slice(b"%W~"),
    }
    out.extend_from_slice(&k.pattern);
}

/// `ACLCheckChannelAgainstList`: channels match ACL patterns; subscription patterns
/// (PSUBSCRIBE) must equal an ACL pattern literally.
pub fn channel_allowed(list: &[Vec<u8>], channel: &[u8], is_pattern: bool) -> bool {
    list.iter().any(|p| if is_pattern { p == channel } else { glob_match(p, channel, false) })
}

/// Read/write permission needed for the `n`th key of a command.
fn key_perm(m: &Meta, n: usize, args: &[Bytes]) -> u8 {
    match m.variable {
        Variable::Set => {
            let get = args.iter().skip(3).any(|a| a.eq_ignore_ascii_case(b"get"));
            return PERM_W | if get { PERM_R } else { 0 };
        }
        Variable::Bitfield => return if bitfield_readonly(args) { PERM_R } else { PERM_R | PERM_W },
        Variable::No => {}
    }
    match m.specs {
        [] => {
            if m.cmd.is_write() {
                PERM_R | PERM_W
            } else {
                PERM_R
            }
        }
        [first, ..] if n == 0 => first & (PERM_R | PERM_W),
        [.., last] => last & (PERM_R | PERM_W),
    }
}

/// Redis' `bitfieldGetKeys`: only GET (and OVERFLOW) operations.
fn bitfield_readonly(args: &[Bytes]) -> bool {
    let mut i = 2;
    while i < args.len() {
        let rem = args.len() - i - 1;
        let a = &args[i];
        if a.eq_ignore_ascii_case(b"get") && rem >= 2 {
            i += 3;
        } else if (a.eq_ignore_ascii_case(b"set") || a.eq_ignore_ascii_case(b"incrby")) && rem >= 3 {
            return false;
        } else if a.eq_ignore_ascii_case(b"overflow") && rem >= 1 {
            i += 2;
        } else {
            return false;
        }
    }
    true
}

/// Key positions of the command being checked, computed once for all selectors. Range-key
/// commands don't allocate.
#[derive(Default)]
pub(crate) struct Keys {
    done: bool,
    pos: SmallVec<[usize; 8]>,
}

impl Keys {
    fn get(&mut self, cmd: &Command, args: &[Bytes]) -> &[usize] {
        if !self.done {
            self.done = true;
            if cmd.keys_fn.is_some() {
                self.pos.extend(cmd.key_positions(args));
            } else if cmd.first_key > 0 {
                let n = args.len() as i32;
                let last = if cmd.last_key < 0 { n + cmd.last_key } else { cmd.last_key };
                let step = cmd.key_step.max(1);
                let mut i = cmd.first_key;
                while i <= last && i < n {
                    self.pos.push(i as usize);
                    i += step;
                }
            }
        }
        &self.pos
    }
}

/// Does a selector list, flattened (as Redis' `getUpcomingChannelList`), allow `channel`?
pub fn channels_allow(selectors: &[Selector], channel: &[u8], is_pattern: bool) -> bool {
    selectors.iter().any(|s| s.allchannels) || selectors.iter().any(|s| channel_allowed(&s.channels, channel, is_pattern))
}

/// Redis' `getUpcomingChannelList` test: can `new` access every channel `old` could?
pub fn channels_superset(new: &[Selector], old: &[Selector]) -> bool {
    if new.iter().any(|s| s.allchannels) {
        return true;
    }
    if old.iter().any(|s| s.allchannels) {
        return false;
    }
    old.iter().flat_map(|s| &s.channels).all(|c| new.iter().any(|s| s.channels.contains(c)))
}
