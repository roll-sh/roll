//! A static site's functions, run (docs/apps/README.md, "Functions in a static site"; Netlify
//! Functions, Vercel's `api/`): the `functions` builder builds the tree's `netlify/functions/*`
//! and `api/*` into a small Node service that answers `/.netlify/functions/<name>` and
//! `/api/<name>`, beside the site (which stays static) behind its environment's routes.
//!
//! Each handler is the one its platform calls: Netlify's `export default (request, context)`
//! (a Web `Request` to a `Response`) or `exports.handler(event, context)` (Lambda's event, an
//! answer `{statusCode, headers, body}`); Vercel's `export default (request)` (Web) or
//! `(req, res)` (Node's, with `req.query`, `req.body`, `res.status()`, `res.json()`,
//! `res.send()`). TypeScript runs as Node strips its types.
//!
//! A site's edge functions ([`crate::edge`]) run in the same image on Deno (pinned), a server of
//! their own on 127.0.0.1 that the Node server hands `/.roll/edge/<name>` to: roll sends each
//! request on an edge function's path there, with the visitor's path in `x-edge-path` and where
//! `context.next()` reads the site's own answer in `x-edge-next`.

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

/// Deno for edge functions, by digest: its binary only.
const DENO: &str = "denoland/deno:bin-2.9.8@sha256:b507583d3c68ba69f3ed2ce5969ae5b8b70f43421c781279c7aea06435263543";

pub(crate) fn plan(tree: &dyn Tree, o: &Overrides) -> Result<Plan, Error> {
    let found = dirs(tree);
    let (edge, _) = crate::edge::find(tree);
    if found.is_empty() && edge.is_empty() {
        return Err(Error::Invalid { file: "netlify/functions or api".into(), reason: "the functions builder found no functions (a .js, .mjs, .cjs, .ts or .mts file, or a directory with an index, in netlify/functions/ or api/, or an edge function with its path)".into() });
    }
    let install = o.install.clone().unwrap_or_else(|| "if [ -f package-lock.json ]; then npm ci --omit=dev; elif [ -f package.json ]; then npm install --omit=dev; fi".into());
    let c = Commands { install: Some(&install), build: None, start: None, custom_install: o.install.is_some(), env: &o.build_env, files: &o.build_files };
    let mut df = Df::new(&c);
    df.from("node:22-slim", None);
    df.line("WORKDIR /app");
    df.copy_source();
    df.run(&["/root/.npm"], &install);
    df.line(&format!("COPY <<'ROLL_FUNCTIONS' /roll/functions.mjs\n{SERVER}\nROLL_FUNCTIONS"));
    if !edge.is_empty() {
        let list = serde_json::to_string(&edge).expect("edge functions serialize");
        df.line(&format!("COPY --from={DENO} /deno /usr/local/bin/deno"));
        df.line(&format!("COPY <<'ROLL_EDGE' /roll/edge.mjs\n{EDGE_SERVER}\nROLL_EDGE"));
        df.line(&format!("COPY <<'ROLL_EDGE_LIST' /roll/edge.json\n{list}\nROLL_EDGE_LIST"));
        // Their imports (URLs, npm:) fetched now, where the build may reach them.
        df.line("ENV DENO_DIR=/roll/deno DENO_NO_UPDATE_CHECK=1");
        let files: Vec<String> = edge.iter().map(|f| format!("/app/{}", f.file)).collect();
        df.line(&format!("RUN deno cache --allow-import {} && chmod -R a+rX /roll/deno", files.join(" ")));
    }
    df.line("ENV NODE_ENV=production");
    df.line(&format!("ENV PORT={DEFAULT_PORT}"));
    df.line(&format!("EXPOSE {DEFAULT_PORT}"));
    df.line("USER node");
    df.line("CMD [\"node\", \"--experimental-strip-types\", \"--no-warnings\", \"/roll/functions.mjs\"]");
    let mut notes: Vec<String> = found.iter().map(|d| format!("{d}/: its functions, answered at {}", DIRS.iter().find(|(x, _)| x == d).map(|(_, r)| *r).unwrap_or(""))).collect();
    notes.extend(edge.iter().map(|f| format!("{}: an edge function on {}, on Deno", f.file, f.paths.join(", "))));
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
        edge: Vec::new(),
        otel: None,
    })
}

/// The functions' server: Node's own HTTP, each function imported once, a body at most 6 MiB.
const SERVER: &str = r#"import http from 'node:http';
import { spawn } from 'node:child_process';
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
// Edge functions: Deno beside this server, on 127.0.0.1, handed /.roll/edge/<name>.
const EDGE = existsSync('/roll/edge.json') ? 8081 : 0;
if (EDGE) {
  const start = () => {
    const c = spawn('deno', ['run', '--quiet', '--allow-net', '--allow-env', '--allow-read=/app,/roll', '--allow-import', '/roll/edge.mjs'], { stdio: 'inherit', env: { ...process.env, ROLL_EDGE_PORT: String(EDGE) } });
    c.on('exit', (code) => { console.error(`edge functions' server exited (${code}); starting it again`); setTimeout(start, 1000); });
  };
  start();
}
async function edge(req, res) {
  const body = req.method === 'GET' || req.method === 'HEAD' ? undefined : await read(req);
  const headers = {};
  for (const [k, v] of Object.entries(req.headers)) if (v !== undefined) headers[k] = Array.isArray(v) ? v.join(', ') : v;
  let r;
  for (let i = 0; ; i++) {
    try {
      r = await fetch(`http://127.0.0.1:${EDGE}${req.url}`, { method: req.method, headers, body, redirect: 'manual' });
      break;
    } catch (e) {
      if (i >= 50) throw e;
      await new Promise((ok) => setTimeout(ok, 100));
    }
  }
  res.statusCode = r.status;
  r.headers.forEach((v, k) => { if (k !== 'content-encoding' && k !== 'content-length' && k !== 'transfer-encoding') res.setHeader(k, v); });
  res.end(Buffer.from(await r.arrayBuffer()));
}
http.createServer(async (req, res) => {
  try {
    const u = new URL(req.url, 'http://localhost');
    if (EDGE && u.pathname.startsWith('/.roll/edge/')) return await edge(req, res);
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

/// Edge functions' server, on Deno: Netlify's `(request, context)` with `context.next()`,
/// `geo`, `ip`, `cookies`, `rewrite` and `Netlify.env`; Vercel's middleware `(request)`, its
/// `next()` and `rewrite()` answers (`x-middleware-next`, `x-middleware-rewrite`). Nothing
/// returned (undefined) is the site's own answer.
const EDGE_SERVER: &str = r#"const fns = JSON.parse(await Deno.readTextFile('/roll/edge.json'));
const loaded = new Map();
globalThis.Netlify = { env: { get: (k) => Deno.env.get(k), has: (k) => Deno.env.has(k), toObject: () => Deno.env.toObject() } };
// The site's runtime config (ROLL_CONFIG_URL), kept current by a long poll: `context.config`, and
// `Roll.config()` for middleware.
let config = {};
globalThis.Roll = { config: () => config };
const configUrl = Deno.env.get('ROLL_CONFIG_URL');
if (configUrl) (async () => {
  let tag = null;
  for (;;) {
    try {
      const r = await fetch(tag ? `${configUrl}?wait=25` : configUrl, { headers: tag ? { 'if-none-match': tag } : {} });
      if (r.status === 200) { tag = r.headers.get('etag'); config = (await r.json()).config ?? {}; }
      else if (r.status !== 304) await new Promise((s) => setTimeout(s, 5000));
    } catch { await new Promise((s) => setTimeout(s, 5000)); }
  }
})();
const load = (f) => { if (!loaded.has(f.name)) loaded.set(f.name, import('file:///app/' + f.file)); return loaded.get(f.name); };
function cookies(req, out) {
  const jar = new Map();
  for (const c of (req.headers.get('cookie') ?? '').split(';')) { const i = c.indexOf('='); if (i > 0) jar.set(c.slice(0, i).trim(), decodeURIComponent(c.slice(i + 1).trim())); }
  return {
    get: (n) => jar.get(n),
    set: (o, v) => { const c = typeof o === 'string' ? { name: o, value: v } : o; out.push(`${c.name}=${encodeURIComponent(c.value)}; Path=${c.path ?? '/'}${c.maxAge != null ? `; Max-Age=${c.maxAge}` : ''}${c.httpOnly ? '; HttpOnly' : ''}${c.secure ? '; Secure' : ''}${c.sameSite ? `; SameSite=${c.sameSite}` : ''}`); },
    delete: (n) => out.push(`${typeof n === 'string' ? n : n.name}=; Path=/; Max-Age=0`),
  };
}
Deno.serve({ hostname: '127.0.0.1', port: Number(Deno.env.get('ROLL_EDGE_PORT') ?? 8081), onListen() {} }, async (incoming) => {
  const u = new URL(incoming.url);
  const m = u.pathname.match(/^\/\.roll\/edge\/([A-Za-z0-9_-]+)$/);
  const f = m && fns.find((x) => x.name === m[1]);
  // Where `next()` asks: roll's address for this service (ROLL_EDGE_BASE) and the signed path the
  // request came with; never another host.
  const nextPath = incoming.headers.get('x-edge-next') ?? '';
  const root = Deno.env.get('ROLL_EDGE_BASE');
  if (!f || !root || !nextPath.startsWith('/_roll/edge/')) return new Response('No such edge function.', { status: 404 });
  const base = root + nextPath;
  const path = incoming.headers.get('x-edge-path') ?? '/';
  const headers = new Headers(incoming.headers);
  for (const h of ['x-edge-path', 'x-edge-next']) headers.delete(h);
  const host = headers.get('x-forwarded-host') ?? headers.get('host') ?? 'localhost';
  const proto = headers.get('x-forwarded-proto') ?? 'https';
  const body = incoming.method === 'GET' || incoming.method === 'HEAD' ? undefined : await incoming.arrayBuffer();
  const request = new Request(`${proto}://${host}${path}`, { method: incoming.method, headers, body });
  // The site's own answer for a request (this one, changed, or another of its paths).
  const next = async (r, init) => {
    const nr = r instanceof Request ? r : new Request(request, init ?? (r && typeof r === 'object' ? r : undefined));
    const nu = new URL(nr.url);
    const b = nr.method === 'GET' || nr.method === 'HEAD' ? undefined : await nr.arrayBuffer();
    return fetch(base + nu.pathname + nu.search, { method: nr.method, headers: nr.headers, body: b, redirect: 'manual' });
  };
  const set = [];
  try {
    const mod = await load(f);
    const fn = typeof mod.default === 'function' ? mod.default : mod.middleware;
    if (typeof fn !== 'function') return new Response('The edge function exports no handler.', { status: 500 });
    let res;
    if (f.kind === 'netlify') {
      const ip = (headers.get('x-forwarded-for') ?? '').split(',')[0].trim();
      const country = headers.get('cf-ipcountry');
      const context = { next, config, ip, requestId: headers.get('x-request-id') ?? crypto.randomUUID(), geo: country ? { country: { code: country } } : {}, cookies: cookies(request, set), params: {}, site: {}, deploy: {}, json: (o, i) => Response.json(o, i), rewrite: (to) => next(new Request(new URL(to, request.url), request)), waitUntil: () => {} };
      res = (await fn(request, context)) ?? (await next());
    } else {
      res = await fn(request, { waitUntil: () => {} });
      const rw = res?.headers.get('x-middleware-rewrite');
      if (!res) res = await next();
      else if (rw) res = await next(new Request(new URL(rw, request.url), request));
      else if (res.headers.get('x-middleware-next')) {
        const h = new Headers(request.headers);
        for (const [k, v] of res.headers) if (k.startsWith('x-middleware-request-')) h.set(k.slice(21), v);
        const n = await next(new Request(request, { headers: h }));
        const out = new Response(n.body, n);
        for (const [k, v] of res.headers) if (!k.startsWith('x-middleware-')) out.headers.set(k, v);
        res = out;
      }
    }
    if (set.length) { res = new Response(res.body, res); for (const c of set) res.headers.append('set-cookie', c); }
    return res;
  } catch (e) {
    console.error(`edge function ${f.name}:`, e);
    return new Response('The edge function failed.', { status: 500 });
  }
});"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_server_is_never_cut_short_by_its_heredoc() {
        assert!(!SERVER.lines().any(|l| l.trim() == "ROLL_FUNCTIONS"));
        assert!(!EDGE_SERVER.lines().any(|l| l.trim() == "ROLL_EDGE"));
    }
}
