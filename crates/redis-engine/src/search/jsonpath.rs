//! JSONPaths as the search module gets them from RedisJSON's API (`JSONAPI_pathParse`):
//! compiled without the legacy-path rewrite, projections refused.

use crate::json::path;

/// What the module asks of a parsed path.
#[derive(Clone, Copy, Debug)]
pub struct PathInfo {
    single: bool,
}

impl PathInfo {
    /// `pathIsSingle`.
    pub fn is_single(&self) -> bool {
        self.single
    }

    /// `pathHasDefinedOrder` (the same test).
    pub fn defined_order(&self) -> bool {
        self.single
    }
}

/// `pathParse`: `Err(None)` when the path is not UTF-8 (no message is set).
pub fn parse(p: &[u8]) -> Result<PathInfo, Option<String>> {
    let p = std::str::from_utf8(crate::util::cstr(p)).map_err(|_| None)?;
    match path::compile(p) {
        Ok(q) if q.is_projection() => Err(Some("computed/projection expressions are not supported by the JSON LLAPI".into())),
        Ok(q) => Ok(PathInfo { single: q.is_static() }),
        Err(e) => Err(Some(e)),
    }
}
