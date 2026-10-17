//! ACL commands. Users, rules and the log live in `crate::acl`; AUTH is handled by the server
//! layer (it needs the directory), which authenticates through `Acl::authenticate`.

use super::*;
use crate::acl::{self, CATEGORIES, category_bit};
use crate::{cmd, container};

const ACL_FLAGS: u64 = NOSCRIPT | LOADING | STALE;

pub static COMMANDS: &[Command] = &[container!("acl", 0, Server, SUBCOMMANDS)];

static SUBCOMMANDS: &[Command] = &[
    cmd!("cat", -2, ACL_FLAGS, 0, 0, 0, acl_cat, Server),
    cmd!("deluser", -3, ADMIN | ACL_FLAGS, 0, 0, 0, acl_deluser, Server),
    cmd!("dryrun", -4, ADMIN | ACL_FLAGS, 0, 0, 0, acl_dryrun, Server),
    cmd!("genpass", -2, ACL_FLAGS, 0, 0, 0, acl_genpass, Server),
    cmd!("getuser", 3, ADMIN | ACL_FLAGS, 0, 0, 0, acl_getuser, Server),
    cmd!("list", 2, ADMIN | ACL_FLAGS, 0, 0, 0, acl_list, Server),
    cmd!("load", 2, ADMIN | ACL_FLAGS, 0, 0, 0, acl_no_file, Server),
    cmd!("log", -2, ADMIN | ACL_FLAGS, 0, 0, 0, acl_log, Server),
    cmd!("save", 2, ADMIN | ACL_FLAGS, 0, 0, 0, acl_no_file, Server),
    cmd!("setuser", -3, ADMIN | ACL_FLAGS, 0, 0, 0, acl_setuser, Server),
    cmd!("users", 2, ADMIN | ACL_FLAGS, 0, 0, 0, acl_users, Server),
    cmd!("whoami", 2, ACL_FLAGS, 0, 0, 0, acl_whoami, Server),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, acl_help, Server),
];

/// `addReplySubcommandSyntaxError`: the subcommand exists but got the wrong arguments.
fn sub_syntax(args: &[Bytes]) -> Error {
    Error::msg(format!(
        "ERR unknown subcommand or wrong number of arguments for '{}'. Try ACL HELP.",
        String::from_utf8_lossy(&args[1]).chars().take(128).collect::<String>()
    ))
}

/// The error text as one line (`addReplyErrorSdsSafe`).
fn safe_err(s: String) -> Error {
    Error::msg(format!("ERR {}", s.replace(['\r', '\n'], " ")))
}

pub fn acl_setuser(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let name = &args[2];
    if !acl::valid_username(name) {
        return Err(Error::new("ERR Usernames can't contain spaces or null characters"));
    }
    cx.eng.acl.write().set_user(name, &args[3..]).map_err(safe_err)?;
    // Users are part of the database's durable state (they survive restarts and reach
    // replicas), so the change goes into the write stream, with passwords only as hashes.
    let rules = args.iter().map(|a| match a.first() {
        Some(b'>') => Bytes::from(format!("#{}", acl::hash_password(&a[1..]))),
        Some(b'<') => Bytes::from(format!("!{}", acl::hash_password(&a[1..]))),
        _ => a.clone(),
    });
    let effect: Vec<Bytes> = rules.collect();
    if cx.mode != crate::engine::Mode::Replay {
        cx.also_propagate(0, effect);
    }
    cx.no_propagate();
    Ok(Reply::Ok)
}

pub fn acl_deluser(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let n = cx.eng.acl.write().del_users(&args[2..]).map_err(Error::new)?;
    if n > 0 && cx.mode != crate::engine::Mode::Replay {
        cx.also_propagate(0, args.to_vec());
    }
    cx.no_propagate();
    Ok(Reply::Int(n))
}

pub fn acl_getuser(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let acl = cx.eng.acl.read();
    Ok(match acl.user(&args[2]) {
        Some(u) => u.rules().getuser_reply(),
        None => Reply::Nil,
    })
}

pub fn acl_list(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let acl = cx.eng.acl.read();
    Ok(Reply::Array(
        acl.users()
            .map(|(name, u)| {
                let mut line = b"user ".to_vec();
                line.extend_from_slice(name);
                line.push(b' ');
                line.extend_from_slice(&u.rules().describe());
                Reply::Bulk(line.into())
            })
            .collect(),
    ))
}

pub fn acl_users(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let acl = cx.eng.acl.read();
    Ok(Reply::Array(acl.users().map(|(name, _)| Reply::bulk(name)).collect()))
}

pub fn acl_whoami(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::bulk(&cx.client.user.name))
}

pub fn acl_no_file(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Err(Error::new(
        "ERR This Redis instance is not configured to use an ACL file. You may want to specify users via the ACL SETUSER command and then issue a CONFIG REWRITE (assuming you have a Redis configuration file set) in order to store users in the Redis configuration.",
    ))
}

pub fn acl_cat(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    match args.len() {
        2 => Ok(Reply::Array(CATEGORIES.iter().map(Reply::bulk).collect())),
        3 => {
            let Some(bit) = category_bit(&args[2]) else {
                return Err(Error::msg(format!(
                    "ERR Unknown category '{}'",
                    String::from_utf8_lossy(&args[2][..args[2].len().min(128)])
                )));
            };
            Ok(Reply::Array(acl::commands_in_category(bit).into_iter().map(Reply::bulk).collect()))
        }
        _ => Err(sub_syntax(args)),
    }
}

pub fn acl_genpass(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let bits = match args.len() {
        2 => 256,
        3 => int(&args[2])?,
        _ => return Err(sub_syntax(args)),
    };
    if bits <= 0 || bits > 4096 {
        return Err(Error::new(
            "ERR ACL GENPASS argument must be the number of bits for the output password, a positive number up to 4096",
        ));
    }
    let chars = ((bits + 3) / 4) as usize;
    let mut raw = vec![0u8; chars.div_ceil(2)];
    getrandom::fill(&mut raw).map_err(|e| Error::msg(format!("ERR {e}")))?;
    let mut hex = String::with_capacity(chars + 1);
    for b in raw {
        hex.push(char::from(b"0123456789abcdef"[(b >> 4) as usize]));
        hex.push(char::from(b"0123456789abcdef"[(b & 15) as usize]));
    }
    hex.truncate(chars);
    Ok(Reply::bulk_str(hex))
}

pub fn acl_log(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut count = 10;
    match args.len() {
        2 => {}
        3 if eq(&args[2], "reset") => {
            cx.eng.acl.read().log.lock().entries.clear();
            return Ok(Reply::Ok);
        }
        3 => count = int(&args[2])?.max(0),
        _ => return Err(sub_syntax(args)),
    }
    let now = crate::util::now_ms();
    Ok(cx.eng.acl.read().log.lock().reply(count as usize, now))
}

pub fn acl_dryrun(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let acl = cx.eng.acl.read();
    let Some(user) = acl.user(&args[2]).cloned() else {
        return Err(Error::msg(format!("ERR User '{}' not found", String::from_utf8_lossy(&args[2]))));
    };
    drop(acl);
    let sub = &args[3..];
    let not_found = || Error::msg(format!("ERR Command '{}' not found", String::from_utf8_lossy(&sub[0])));
    let mut cmd = lookup(&sub[0]).ok_or_else(not_found)?;
    if !cmd.subs.is_empty() && sub.len() > 1 {
        cmd = lookup_sub(cmd, &sub[1]).ok_or_else(not_found)?;
    }
    let full = acl::full_name(cmd).unwrap_or(cmd.name);
    if !cmd.check_arity(sub.len()) {
        return Err(Error::msg(format!("ERR wrong number of arguments for '{full}' command")));
    }
    Ok(match user.check(cmd, sub) {
        Ok(()) => Reply::Ok,
        Err(d) => Reply::bulk_str(acl::dryrun_message(&user.name, full, sub, d)),
    })
}

pub fn acl_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "ACL <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "CAT [<category>]",
        "    List all commands that belong to <category>, or all command categories",
        "    when no category is specified.",
        "DELUSER <username> [<username> ...]",
        "    Delete a list of users.",
        "DRYRUN <username> <command> [<arg> ...]",
        "    Returns whether the user can execute the given command without executing the command.",
        "GETUSER <username>",
        "    Get the user's details.",
        "GENPASS [<bits>]",
        "    Generate a secure 256-bit user password. The optional `bits` argument can",
        "    be used to specify a different size.",
        "LIST",
        "    Show users details in config file format.",
        "LOAD",
        "    Reload users from the ACL file.",
        "LOG [<count> | RESET]",
        "    Show the ACL log entries.",
        "SAVE",
        "    Save the current config to the ACL file.",
        "SETUSER <username> <attribute> [<attribute> ...]",
        "    Create or modify a user with the specified attributes.",
        "USERS",
        "    List all the registered usernames.",
        "WHOAMI",
        "    Return the current connection username.",
        "HELP",
        "    Print this help.",
    ]))
}
