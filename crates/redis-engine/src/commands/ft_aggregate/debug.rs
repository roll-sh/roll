//! `_FT.DEBUG`'s query wrappers (`aggregate_debug.c`, `hybrid_debug.c`, the debug wrappers of
//! `debug_commands.c`): FT.SEARCH, FT.AGGREGATE, FT.PROFILE and FT.HYBRID (and their internal
//! `_FT.*` twins) with debug parameters at the end, `… DEBUG_PARAMS_COUNT <count>`. The count
//! is parsed first; the command runs without them, and its own errors (arguments, index, query,
//! pipeline) come before the debug parameters'. `TIMEOUT_AFTER_N n` times the query out once
//! its index reader gave `n` results (`RPTimeoutAfterCount`, just downstream of the reader;
//! a cursor's every read gets `n` more).

use super::*;

/// `parseDebugParamsCount`: the debug parameters' count, the last argument, after
/// `DEBUG_PARAMS_COUNT` (any prefix of the name, `strncasecmp` over the argument's length).
pub(in crate::commands) fn params_count(argv: &[Bytes]) -> Result<usize, QErr> {
    let argc = argv.len();
    if argc < 2 {
        return Err(QErr::new(Code::ParseArgs, "DEBUG_PARAMS_COUNT arg is missing"));
    }
    let name = b"DEBUG_PARAMS_COUNT";
    let arg = &argv[argc - 2];
    let mut matches = true;
    for (i, &c) in arg.iter().enumerate() {
        let n = name.get(i).copied().unwrap_or(0);
        if !c.eq_ignore_ascii_case(&n) {
            matches = false;
            break;
        }
        if c == 0 {
            break;
        }
    }
    if !matches {
        return Err(QErr::new(Code::ParseArgs, "DEBUG_PARAMS_COUNT arg is missing or not in the expected position"));
    }
    // `RedisModule_StringToULongLong`.
    let last = &argv[argc - 1];
    let count = std::str::from_utf8(last).ok().filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).and_then(|s| s.parse::<u64>().ok()).filter(|&c| c != 0);
    let Some(count) = count else { return Err(QErr::new(Code::ParseArgs, "Invalid DEBUG_PARAMS_COUNT count")) };
    if count > (argc - 2) as u64 {
        return Err(QErr::new(Code::ParseArgs, "DEBUG_PARAMS_COUNT exceeds the number of available arguments"));
    }
    Ok(count as usize)
}

/// The arguments split: the command's (without the debug parameters) and the parameters.
pub(in crate::commands) fn split(argv: &[Bytes]) -> Result<(&[Bytes], &[Bytes]), QErr> {
    let count = params_count(argv)?;
    let cut = argv.len() - (count + 2);
    Ok((&argv[..cut], &argv[cut..argv.len() - 2]))
}

/// `parseAndCompileDebug` (standalone: never the cluster coordinator): `TIMEOUT_AFTER_N`'s
/// count, if any.
pub(in crate::commands) fn parse(params: &[Bytes], policy: Policy, background: bool) -> Result<Option<u32>, QErr> {
    let mut ac = Ac::new(params);
    let mut timeout: Option<Ac<'_>> = None;
    let mut pause_after: Option<Ac<'_>> = None;
    let mut pause_before: Option<Ac<'_>> = None;
    let (mut crash, mut crash_rust, mut internal_only) = (false, false, false);
    // `AC_ParseArgSpec`.
    while let Some(a) = ac.peek() {
        let (name, slice): (&str, usize) = if c_eq(a, "TIMEOUT_AFTER_N") {
            ("TIMEOUT_AFTER_N", 1)
        } else if c_eq(a, "PAUSE_AFTER_RP_N") {
            ("PAUSE_AFTER_RP_N", 2)
        } else if c_eq(a, "PAUSE_BEFORE_RP_N") {
            ("PAUSE_BEFORE_RP_N", 2)
        } else if c_eq(a, "CRASH") {
            crash = true;
            ac.offset += 1;
            continue;
        } else if c_eq(a, "CRASH_IN_RUST") {
            crash_rust = true;
            ac.offset += 1;
            continue;
        } else if c_eq(a, "INTERNAL_ONLY") {
            internal_only = true;
            ac.offset += 1;
            continue;
        } else {
            return Err(QErr::user(Code::ParseArgs, "Unrecognized argument", format!(": {}", qast::lossy(a))));
        };
        ac.offset += 1;
        let sub = ac
            .get_slice(slice)
            .map_err(|e| QErr::user(Code::ParseArgs, "Error parsing arguments for index", format!(" {name}: {}", e.strerror())))?;
        match name {
            "TIMEOUT_AFTER_N" => timeout = Some(sub),
            "PAUSE_AFTER_RP_N" => pause_after = Some(sub),
            _ => pause_before = Some(sub),
        }
    }
    for (on, what) in [(crash, "CRASH"), (crash_rust, "CRASH_IN_RUST")] {
        if !on {
            continue;
        }
        if internal_only {
            return Err(QErr::new(Code::ParseArgs, format!("INTERNAL_ONLY is not supported with {what}")));
        }
        // `PipelineAddCrash`: the module aborts the server.
        return Err(QErr::new(Code::Generic, format!("{what} is not supported")));
    }
    if let Some(mut t) = timeout {
        let Ok(n) = t.get_size(crate::search::args::GE0) else {
            return Err(QErr::new(Code::ParseArgs, "Invalid TIMEOUT_AFTER_N count"));
        };
        if policy == Policy::Strict {
            return Err(QErr::new(Code::ParseArgs, "TIMEOUT_AFTER_N is not supported with ON_TIMEOUT RETURN-STRICT"));
        }
        if policy == Policy::Fail && background {
            return Err(QErr::new(Code::ParseArgs, "TIMEOUT_AFTER_N is not supported with ON_TIMEOUT FAIL if WORKERS > 0"));
        }
        // `RPTimeoutAfterCount.count` is 32 bits.
        return Ok(Some(n as u32));
    }
    if let Some((mut p, what)) = pause_before.map(|p| (p, "PAUSE_BEFORE_RP_N")).or(pause_after.map(|p| (p, "PAUSE_AFTER_RP_N"))) {
        if !background {
            return Err(QErr::new(Code::ParseArgs, format!("Query {what} is only supported with WORKERS")));
        }
        let ty = p.get().map_err(|_| QErr::new(Code::ParseArgs, format!("Invalid {what} RP type")))?;
        if p.get_size(crate::search::args::GE0).is_err() {
            return Err(QErr::new(Code::ParseArgs, format!("Invalid {what} count")));
        }
        // `PipelineAddPauseRPcount`: a paused query waits for SET_PAUSE_RP_RESUME on a worker.
        return Err(QErr::new(Code::Generic, format!("{} {what} is not supported", qast::lossy(ty))));
    }
    if internal_only {
        return Err(QErr::new(Code::ParseArgs, "INTERNAL_ONLY is not supported without TIMEOUT_AFTER_N or PAUSE_AFTER_RP_N/PAUSE_BEFORE_RP_N"));
    }
    Ok(None)
}

/// `parseHybridDebugParams`: TIMEOUT_AFTER_N_SEARCH, _VSIM and _TAIL's counts (0: none), at
/// least one of them; never under ON_TIMEOUT RETURN-STRICT (`strict`).
pub(in crate::commands) fn parse_hybrid(params: &[Bytes], strict: bool) -> Result<[u32; 3], QErr> {
    const NAMES: [&str; 3] = ["TIMEOUT_AFTER_N_SEARCH", "TIMEOUT_AFTER_N_VSIM", "TIMEOUT_AFTER_N_TAIL"];
    let mut ac = Ac::new(params);
    let mut subs: [Option<Ac<'_>>; 3] = [None, None, None];
    // `AC_ParseArgSpec`.
    while let Some(a) = ac.peek() {
        let Some(i) = NAMES.iter().position(|n| c_eq(a, n)) else {
            return Err(QErr::user(Code::ParseArgs, "Unrecognized argument", format!(": {}", qast::lossy(a))));
        };
        ac.offset += 1;
        let sub = ac.get_slice(1).map_err(|e| QErr::user(Code::ParseArgs, "Error parsing arguments for", format!(" {}: {}", NAMES[i], e.strerror())))?;
        subs[i] = Some(sub);
    }
    let mut counts = [0u32; 3];
    for (i, sub) in subs.iter_mut().enumerate() {
        if let Some(sub) = sub {
            let Ok(n) = sub.get_size(crate::search::args::GE0) else {
                return Err(QErr::new(Code::ParseArgs, format!("Invalid {} count", NAMES[i])));
            };
            counts[i] = n as u32;
        }
    }
    if subs.iter().all(Option::is_none) {
        return Err(QErr::new(
            Code::ParseArgs,
            "At least one component timeout parameter (TIMEOUT_AFTER_N_SEARCH, TIMEOUT_AFTER_N_VSIM, or TIMEOUT_AFTER_N_TAIL) must be specified",
        ));
    }
    if strict {
        return Err(QErr::new(Code::ParseArgs, "FT.HYBRID debug timeout is not supported with ON_TIMEOUT RETURN-STRICT"));
    }
    Ok(counts)
}
