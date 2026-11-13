// The roll secrets GitHub Action (docs/secrets.md, "GitHub Actions"): an environment's values in
// the job, each masked before anything else sees it, through a workload identity's login with the
// job's own OIDC token (no stored token) or a roll token. No dependencies: Node's own fetch.
"use strict";

const childProcess = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");

const KEY = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** Fails with roll's message (never a value). */
async function call(fetchFn, api, method, path, token, body) {
  const res = await fetchFn(api + path, {
    method,
    headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), ...(body ? { "content-type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`.trim();
    try {
      msg = JSON.parse(text).error || msg;
    } catch {
      // not JSON: the status says it
    }
    throw new Error(`${method} ${path}: ${msg}`);
  }
  return text ? JSON.parse(text) : null;
}

/** The job's own OIDC token for `audience` (needs `permissions: id-token: write`). */
async function oidcToken(fetchFn, env, audience) {
  const url = env.ACTIONS_ID_TOKEN_REQUEST_URL;
  const bearer = env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
  if (!url || !bearer) throw new Error("no OIDC token: give the job `permissions: id-token: write`, or pass `token`");
  const res = await fetchFn(`${url}${url.includes("?") ? "&" : "?"}audience=${encodeURIComponent(audience)}`, { headers: { authorization: `Bearer ${bearer}` } });
  if (!res.ok) throw new Error(`GitHub Actions answered ${res.status} for its OIDC token`);
  const value = (await res.json()).value;
  if (!value) throw new Error("GitHub Actions' OIDC answer has no value");
  return value;
}

/**
 * A line's candidates, as roll code's scan and `roll secrets scan` take them: runs of
 * `[A-Za-z0-9_\-+/=.:@~]`, the whole trimmed line, and the right-hand side of `KEY=VALUE` or
 * `key: value` (unquoted). The server never matches anything under 8 characters.
 */
function candidatesOf(line) {
  const out = new Set();
  const add = (s) => {
    const n = [...s].length;
    if (n >= 8 && n <= 4096) out.add(s);
  };
  for (const run of line.split(/[^A-Za-z0-9_\-+/=.:@~]/)) add(run);
  const trimmed = line.trim();
  add(trimmed);
  const i = trimmed.search(/[=:]/);
  if (i >= 0) {
    const rhs = trimmed.slice(i + 1).trim().replace(/[,;]+$/, "").trim();
    const q = rhs.match(/^(["'`])(.*)\1$/s);
    add(q ? q[2] : rhs);
  }
  return out;
}

/** The lines `git diff base...HEAD` adds: `{file, line, text}`. */
function addedLines(cwd, base) {
  const diff = childProcess.execFileSync("git", ["diff", "--unified=0", "--no-color", "--no-ext-diff", `${base}...HEAD`], { cwd, maxBuffer: 256 << 20, encoding: "utf8" });
  const out = [];
  let file = null;
  let line = 0;
  for (const l of diff.split("\n")) {
    if (l.startsWith("+++ ")) file = l === "+++ /dev/null" ? null : l.slice(4).replace(/^b\//, "");
    else if (l.startsWith("@@")) line = Number((l.match(/\+(\d+)/) || [0, 0])[1]);
    else if (l.startsWith("+") && file) out.push({ file, line: line++, text: l.slice(1) });
  }
  return out;
}

/**
 * `scan`: fails when a line this change adds holds one of the project's stored values, naming the
 * key, environment, file and line (never the value). Only fingerprints cross: `POST …/leaks`
 * answers which candidates are values the token may reveal.
 */
async function scan(fetchFn, api, token, project, env, out) {
  const cwd = env.GITHUB_WORKSPACE || process.cwd();
  const base = (env.INPUT_BASE || "").trim() || (env.GITHUB_BASE_REF ? `origin/${env.GITHUB_BASE_REF}` : "HEAD~1");
  const where = new Map(); // candidate → first {file, line}
  for (const a of addedLines(cwd, base)) {
    for (const c of candidatesOf(a.text)) if (!where.has(c)) where.set(c, a);
  }
  const all = [...where.keys()];
  const found = [];
  for (let i = 0; i < all.length; i += 20000) {
    const chunk = all.slice(i, i + 20000);
    const matches = await call(fetchFn, api, "POST", `/v1/secrets/projects/${encodeURIComponent(project)}/leaks`, token, { candidates: chunk });
    for (const m of matches) found.push({ ...m, at: where.get(chunk[m.index]) });
  }
  for (const f of found) {
    out.write(`::error file=${f.at.file},line=${f.at.line}::${f.key} (${f.environment}${f.path === "/" ? "" : `:${f.path}`}) from roll secrets is in this change: remove it and rotate it\n`);
  }
  if (env.GITHUB_OUTPUT) fs.appendFileSync(env.GITHUB_OUTPUT, `leaks=${found.length}\n`);
  out.write(`roll secrets: ${all.length} candidates from ${base}...HEAD, ${found.length} stored value${found.length === 1 ? "" : "s"}\n`);
  if (found.length) throw new Error(`${found.length} of the project's stored values ${found.length === 1 ? "is" : "are"} in this change`);
  return [];
}

/** A heredoc delimiter that isn't in `value`, for GITHUB_ENV and GITHUB_OUTPUT. */
function delimiter(value) {
  for (;;) {
    const d = `roll_${crypto.randomBytes(16).toString("hex")}`;
    if (!value.includes(d)) return d;
  }
}

/**
 * The action, given its environment (`INPUT_*`, `GITHUB_*`, the OIDC request variables), where
 * workflow commands go (stdout) and fetch. Returns the keys it exported.
 */
async function run(env, out, fetchFn = fetch) {
  const input = (name) => (env[`INPUT_${name.replace(/ /g, "_").toUpperCase()}`] || "").trim();
  const api = input("api-url").replace(/\/+$/, "");
  const environment = input("environment");
  const scanning = input("scan") === "true";
  if (!api || (!environment && !scanning)) throw new Error("api-url, and environment unless scanning, are required");
  let token = input("token");
  let project = input("project");
  if (token) {
    out.write(`::add-mask::${token}\n`);
  } else {
    const identity = input("identity");
    if (!identity) throw new Error("pass identity (a workload identity, idn_…) or token");
    const oidc = await oidcToken(fetchFn, env, input("audience") || api);
    out.write(`::add-mask::${oidc}\n`);
    const login = await call(fetchFn, api, "POST", "/v1/secrets/login", "", { identity, token: oidc });
    token = login.token;
    out.write(`::add-mask::${token}\n`);
    project ||= login.project;
  }
  if (!project) project = (await call(fetchFn, api, "GET", "/v1/secrets/whoami", token)).project;
  if (!project) throw new Error("which project? pass project");
  if (scanning) return scan(fetchFn, api, token, project, env, out);
  const path = input("path") || "/";
  const v = await call(fetchFn, api, "GET", `/v1/secrets/projects/${encodeURIComponent(project)}/environments/${encodeURIComponent(environment)}/values?path=${encodeURIComponent(path)}`, token);
  if (v.problems && v.problems.length) {
    throw new Error(`${environment} doesn't resolve: ${v.problems.map((p) => `${p.key}: ${p.problem}`).join("; ")}`);
  }
  const keys = Object.keys(v.values).sort();
  // Masked first, every line of every value: nothing below may print one unmasked.
  for (const k of keys) {
    for (const line of String(v.values[k]).split(/\r?\n/)) {
      if (line.trim()) out.write(`::add-mask::${line}\n`);
    }
  }
  const lines = [];
  for (const k of keys) {
    if (!KEY.test(k)) throw new Error(`${k} isn't a name a process environment can hold`);
    const value = String(v.values[k]);
    const d = delimiter(value);
    lines.push(`${k}<<${d}\n${value}\n${d}\n`);
  }
  const exportTo = input("export") === "false" ? env.GITHUB_OUTPUT : env.GITHUB_ENV;
  if (!exportTo) throw new Error(input("export") === "false" ? "GITHUB_OUTPUT isn't set" : "GITHUB_ENV isn't set");
  fs.appendFileSync(exportTo, lines.join(""), { mode: 0o600 });
  if (env.GITHUB_OUTPUT) {
    const names = keys.join(",");
    const d = delimiter(names);
    fs.appendFileSync(env.GITHUB_OUTPUT, `keys<<${d}\n${names}\n${d}\n`);
  }
  out.write(`roll secrets: ${keys.length} key${keys.length === 1 ? "" : "s"} from ${environment}${path === "/" ? "" : `:${path}`}\n`);
  return keys;
}

module.exports = { run, candidatesOf };

if (require.main === module) {
  run(process.env, process.stdout).catch((e) => {
    process.stdout.write(`::error::${String(e.message || e).replace(/\r?\n/g, " ")}\n`);
    process.exitCode = 1;
  });
}
