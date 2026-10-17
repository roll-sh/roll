//! Query commands of RediSearch: FT.EXPLAIN and FT.EXPLAINCLI (`queryExplainCommon`): the
//! request compiled as FT.SEARCH/FT.AGGREGATE compile theirs, the index applied, and the parsed
//! query tree printed.

use super::ft::{safe, verify_acl};
use super::*;
use crate::search::areq::{self, Command};
use crate::search::error::{Code, QErr};
use crate::search::qast;

/// `RS_GetExplainOutput`: compile, look the index up (counting a use), apply it, dump.
fn explain_output(cx: &mut Cx, args: &[Bytes]) -> Result<Vec<u8>, Error> {
    let mut req = areq::compile(Command::Explain, &cx.eng.cfg, &args[2..])?;
    let Some(name) = cx.eng.search.load(crate::util::cstr(&args[1]), false) else {
        return Err(QErr::user(Code::NoIndex, "Index not found", format!(": {}", qast::lossy(&args[1]))).into());
    };
    let resp3 = cx.client.resp >= 3;
    let ix = cx.eng.search.indexes.get_mut(&name).expect("loaded");
    req.apply_context(ix, resp3)?;
    let root = req.root.as_ref().expect("a parsed query");
    Ok(qast::explain(root, ix))
}

fn explain_common(cx: &mut Cx, args: &[Bytes], name: &str, lines: bool) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error(name));
    }
    verify_acl(cx, &args[1])?;
    let out = explain_output(cx, args)?;
    // The dump is read as a C string.
    let out = crate::util::cstr(&out);
    if lines {
        return Ok(Reply::Array(out.split(|&c| c == b'\n').map(safe).collect()));
    }
    Ok(Reply::Bulk(Bytes::copy_from_slice(out)))
}

/// FT.EXPLAIN index query [options]: the query tree as text.
pub(super) fn ft_explain(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    explain_common(cx, args, "FT.EXPLAIN", false)
}

/// FT.EXPLAINCLI index query [options]: the same, a line per element.
pub(super) fn ft_explaincli(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    explain_common(cx, args, "FT.EXPLAINCLI", true)
}
