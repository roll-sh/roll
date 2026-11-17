//! A static site's functions, run (docs/apps/README.md, "Functions in a static site"; Netlify
//! Functions, Vercel's `api/`): the `functions` builder builds the tree's `netlify/functions/*`
//! and `api/*` into a small Node service that answers `/.netlify/functions/<name>` and
//! `/api/<name>`, beside the site (which stays static) behind its environment's routes.
//!
//! Each handler is the one its platform calls: Netlify's `export default (request, context)`
//! (a Web `Request` to a `Response`) or `exports.handler(event, context)` (Lambda's event, an
//! answer `{statusCode, headers, body}`); Vercel's `export default (request)` (Web) or
//! `(req, res)` (Node's, with `req.query`, `req.body`, `res.status()`, `res.json()`,
//! `res.send()`). TypeScript runs as Node strips its types. Edge functions (Deno) aren't run.

use crate::df::Df;
use crate::{Commands, DEFAULT_PORT, Error, Overrides, Plan, Provider, Tree};

/// Where each platform keeps its functions, and the path they answer under.
pub(crate) const DIRS: [(&str, &str); 2] = [("netlify/functions", "/.netlify/functions/*"), ("api", "/api/*")];
/// What a function's file may be.
const EXT: [&str; 5] = [".mjs", ".js", ".cjs", ".mts", ".ts"];

/// The function directories a tree has (with at least one function in it).
pub fn dirs(tree: &dyn Tree) -> Vec<&'static str> {
    DIRS.iter().map(|(d, _)| *d).filter(|d| tree.list(d).iter().any(|f| EXT.iter().any(|e| f.ends_with(e)) || f.ends_with('/'))).collect()
}

/// The paths a tree's functions answer (a route each, to the functions service).
pub fn routes(tree: &dyn Tree) -> Vec<&'static str> {
    DIRS.iter().filter(|(d, _)| dirs(tree).contains(d)).map(|(_, r)| *r).collect()
}

pub(crate) fn plan(tree: &dyn Tree, o: &Overrides) -> Result<Plan, Error> {
    let found = dirs(tree);
    if found.is_empty() {
        return Err(Error::Invalid { file: "netlify/functions or api".into(), reason: "the functions builder found no functions (a .js, .mjs, .cjs, .ts or .mts file, or a directory with an index, in netlify/functions/ or api/)".into() });
    }
    let install = o.install.clone().unwrap_or_else(|| "if [ -f package-lock.json ]; then npm ci --omit=dev; elif [ -f package.json ]; then npm install --omit=dev; fi".into());
    let c = Commands { install: Some(&install), build: None, start: None, custom_install: o.install.is_some(), env: &o.build_env, files: &o.build_files };
    let mut df = Df::new(&c);
    df.from("node:22-slim", None);
    df.line("WORKDIR /app");
    df.copy_source();
    df.run(&["/root/.npm"], &install);
    df.line(&format!("COPY <<'ROLL_FUNCTIONS' /roll/functions.mjs\n{SERVER}\nROLL_FUNCTIONS"));
    df.line("ENV NODE_ENV=production");
    df.line(&format!("ENV PORT={DEFAULT_PORT}"));
    df.line(&format!("EXPOSE {DEFAULT_PORT}"));
    df.line("USER node");
    df.line("CMD [\"node\", \"--experimental-strip-types\", \"--no-warnings\", \"/roll/functions.mjs\"]");
    let notes = found.iter().map(|d| format!("{d}/: its functions, answered at {}", DIRS.iter().find(|(x, _)| x == d).map(|(_, r)| *r).unwrap_or(""))).collect();
    Ok(Plan {
        provider: Provider::Functions,
        framework: None,
        language_version: Some("22".into()),
        install: Some(install),
        build: None,
        start: Some("node /roll/functions.mjs".into()),
        port: DEFAULT_PORT,
        dockerfile: df.finish(),
        notes,
        spa: false,
        uses: Vec::new(),
        release: None,
        crons: Vec::new(),
        functions: Vec::new(),
    })
}

/// The functions' server: Node's own HTTP, each function imported once, a body at most 6 MiB.
const SERVER: &str = r#"import http from 'node:http';
import { existsSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const ROOT = '/app', EXT = ['.mjs', '.js', '.cjs', '.mts', '.ts'], MAX = 6 << 20;
const loaded = new Map();
function find(dir, name) {
  if (!/^[A-Za-z0-9_-]+$/.test(name)) return null;
  for (const e of EXT) {
    for (const f of [`${ROOT}/${dir}/${name}${e}`, `${ROOT}/${dir}/${name}/index${e}`]) if (existsSync(f)) return f;
  }
  return null;
}
function load(f) {
  if (!loaded.has(f)) loaded.set(f, import(pathToFileURL(f).href));
  return loaded.get(f);
}
async function read(req) {
  const parts = [];
  let n = 0;
  for await (const c of req) {
    n += c.length;
    if (n > MAX) throw Object.assign(new Error('too large'), { status: 413 });
    parts.push(c);
  }
  return Buffer.concat(parts);
}
function web(req, buf) {
  const headers = new Headers();
  for (const [k, v] of Object.entries(req.headers)) if (v !== undefined) headers.set(k, Array.isArray(v) ? v.join(', ') : v);
  const body = req.method === 'GET' || req.method === 'HEAD' ? undefined : buf;
  return new Request(`http://${req.headers.host ?? 'localhost'}${req.url}`, { method: req.method, headers, body });
}
async function answer(res, r) {
  res.statusCode = r.status;
  r.headers.forEach((v, k) => res.setHeader(k, v));
  res.end(Buffer.from(await r.arrayBuffer()));
}
http.createServer(async (req, res) => {
  try {
    const u = new URL(req.url, 'http://localhost');
    let m, f, kind;
    if ((m = u.pathname.match(/^\/\.netlify\/functions\/([^/]+)/))) [f, kind] = [find('netlify/functions', m[1]), 'netlify'];
    else if ((m = u.pathname.match(/^\/api\/([^/]+)/))) [f, kind] = [find('api', m[1].replace(/\.[A-Za-z]+$/, '')), 'vercel'];
    if (!f) {
      res.statusCode = 404;
      return res.end('No such function.');
    }
    const mod = await load(f);
    const buf = await read(req);
    const lambda = mod.handler ?? mod.default?.handler;
    if (kind === 'netlify' && typeof lambda === 'function' && typeof mod.default !== 'function') {
      const event = { path: u.pathname, httpMethod: req.method, headers: req.headers, queryStringParameters: Object.fromEntries(u.searchParams), rawUrl: `http://${req.headers.host}${req.url}`, rawQuery: u.search.slice(1), body: buf.length ? buf.toString('utf8') : null, isBase64Encoded: false };
      const r = (await lambda(event, {})) ?? {};
      res.statusCode = r.statusCode ?? 200;
      for (const [k, v] of Object.entries(r.headers ?? {})) res.setHeader(k, v);
      return res.end(r.isBase64Encoded ? Buffer.from(r.body ?? '', 'base64') : (r.body ?? ''));
    }
    const fn = typeof mod.default === 'function' ? mod.default : typeof mod.default?.default === 'function' ? mod.default.default : null;
    if (!fn) {
      res.statusCode = 500;
      return res.end('The function exports no handler.');
    }
    if (kind === 'vercel' && fn.length >= 2) {
      const text = buf.toString('utf8');
      req.query = Object.fromEntries(u.searchParams);
      try { req.body = (req.headers['content-type'] ?? '').includes('json') && text ? JSON.parse(text) : text; } catch { req.body = text; }
      res.status = (code) => { res.statusCode = code; return res; };
      res.json = (o) => { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(o)); return res; };
      res.send = (b) => { res.end(typeof b === 'object' && !Buffer.isBuffer(b) ? JSON.stringify(b) : b); return res; };
      return await fn(req, res);
    }
    return await answer(res, await fn(web(req, buf), {}));
  } catch (e) {
    console.error(e);
    if (!res.headersSent) {
      res.statusCode = e.status ?? 500;
      res.end(e.status === 413 ? 'The request is over 6 MiB.' : 'The function failed.');
    }
  }
}).listen(Number(process.env.PORT ?? 8080), '0.0.0.0');"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_server_is_never_cut_short_by_its_heredoc() {
        assert!(!SERVER.lines().any(|l| l.trim() == "ROLL_FUNCTIONS"));
    }
}
