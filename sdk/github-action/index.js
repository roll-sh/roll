// The roll secrets GitHub Action (docs/secrets.md, "GitHub Actions"): an environment's values in
// the job, each masked before anything else sees it, through a workload identity's login with the
// job's own OIDC token (no stored token) or a roll token. No dependencies: Node's own fetch.
"use strict";

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
  if (!api || !environment) throw new Error("api-url and environment are required");
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

module.exports = { run };

if (require.main === module) {
  run(process.env, process.stdout).catch((e) => {
    process.stdout.write(`::error::${String(e.message || e).replace(/\r?\n/g, " ")}\n`);
    process.exitCode = 1;
  });
}
