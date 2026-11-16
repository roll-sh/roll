// The roll apps deploy GitHub Action (docs/apps/README.md, "GitHub Actions"): deploy a service
// from a workflow once its tests pass, wait for the deployment, and say where it runs (and, on a
// pull request, in a comment). No dependencies: Node's own fetch, and git for an upload.
"use strict";

const childProcess = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");

/** Where a deployment stops moving. */
const DONE = new Set(["active", "ready", "failed", "canceled", "superseded", "removed", "inactive"]);

/** Fails with roll's message. */
async function call(fetchFn, api, method, path, token, body, headers = {}) {
  const json = body !== undefined && !Buffer.isBuffer(body);
  const res = await fetchFn(api + path, {
    method,
    headers: { authorization: `Bearer ${token}`, ...(json ? { "content-type": "application/json" } : {}), ...headers },
    body: body === undefined ? undefined : json ? JSON.stringify(body) : body,
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

const seg = encodeURIComponent;

/** The commit's subject, or none outside a checkout. */
function subject(exec, cwd) {
  try {
    return exec("git", ["log", "-1", "--format=%s"], { cwd }).toString().trim();
  } catch {
    return "";
  }
}

/** The pull request's own comment for this service and environment, made or brought up to date. */
async function comment(fetchFn, env, ghToken, marker, body) {
  const event = env.GITHUB_EVENT_PATH ? JSON.parse(fs.readFileSync(env.GITHUB_EVENT_PATH, "utf8")) : {};
  const number = event.pull_request?.number;
  if (!number) return false;
  const gh = (env.GITHUB_API_URL || "https://api.github.com").replace(/\/+$/, "");
  const base = `/repos/${env.GITHUB_REPOSITORY}/issues/${number}/comments`;
  const headers = { accept: "application/vnd.github+json" };
  const list = await call(fetchFn, gh, "GET", `${base}?per_page=100`, ghToken, undefined, headers);
  const mine = (list || []).find((c) => typeof c.body === "string" && c.body.includes(marker));
  const text = `${marker}\n${body}`;
  if (mine) await call(fetchFn, gh, "PATCH", `/repos/${env.GITHUB_REPOSITORY}/issues/comments/${mine.id}`, ghToken, { body: text }, headers);
  else await call(fetchFn, gh, "POST", base, ghToken, { body: text }, headers);
  return true;
}

/** A heredoc delimiter that isn't in `value`, for GITHUB_OUTPUT. */
function delimiter(value) {
  for (;;) {
    const d = `roll_${crypto.randomBytes(16).toString("hex")}`;
    if (!value.includes(d)) return d;
  }
}

/**
 * The action, given its environment (`INPUT_*`, `GITHUB_*`), where workflow commands go
 * (stdout), fetch, a command runner and a sleep. Returns the deployment as it ended.
 */
async function run(env, out, fetchFn = fetch, exec = childProcess.execFileSync, sleep = (ms) => new Promise((r) => setTimeout(r, ms))) {
  const input = (name) => (env[`INPUT_${name.replace(/ /g, "_").toUpperCase()}`] || "").trim();
  const api = input("api-url").replace(/\/+$/, "");
  const token = input("token");
  const service = input("service");
  const environment = input("environment") || "production";
  if (!api) throw new Error("api-url is rolld's API, e.g. https://api.roll.example");
  if (!token) {
    const fork = env.GITHUB_EVENT_NAME === "pull_request" ? " (a fork's pull request doesn't get the repository's secrets, and isn't deployed)" : "";
    throw new Error(`no token: pass a roll token that may deploy the service${fork}`);
  }
  out.write(`::add-mask::${token}\n`);
  if (!/^svc_[A-Za-z0-9]+$/.test(service)) throw new Error("service is the service's id (svc_…)");
  const envPath = `/v1/apps/services/${seg(service)}/environments/${seg(environment)}`;
  const svc = await call(fetchFn, api, "GET", `/v1/apps/services/${seg(service)}`, token);
  const kind = svc?.source?.kind;
  const cwd = input("path") || ".";
  const sha = input("revision") || env.GITHUB_SHA || "";
  const message = (input("message") || subject(exec, cwd) || `${env.GITHUB_WORKFLOW || "GitHub Actions"} #${env.GITHUB_RUN_NUMBER || "?"}`).slice(0, 500);
  // A retried step answers the deployment it started (a re-run attempt is a new deploy).
  const key = `github:${env.GITHUB_REPOSITORY || "?"}:${env.GITHUB_RUN_ID || Date.now()}:${env.GITHUB_RUN_ATTEMPT || "1"}:${environment}`.slice(0, 200);
  let deployment;
  if (input("image")) {
    if (kind !== "image") throw new Error(`image is for an image service; ${service} builds its ${kind} source`);
    deployment = await call(fetchFn, api, "POST", `${envPath}/deploys`, token, { image: input("image"), message }, { "idempotency-key": key });
  } else if (kind === "repo" || kind === "git") {
    // roll builds the commit from the repository itself: nothing leaves the runner.
    if (!/^[0-9a-f]{40}$/i.test(sha)) throw new Error("revision is a full commit id (GITHUB_SHA by default)");
    deployment = await call(fetchFn, api, "POST", `${envPath}/deploys`, token, { revision: sha.toLowerCase(), message }, { "idempotency-key": key });
  } else if (kind === "upload") {
    // The checkout's committed files under `path` (git archive: what's tracked at HEAD, never
    // .git, untracked files or what .gitignore keeps out), as `roll up` sends a tree.
    // Run from the top: from a folder, git archive also narrows the tree it's given to that folder.
    const git = (args, dir) => exec("git", args, { cwd: dir, maxBuffer: 512 << 20 });
    const top = git(["rev-parse", "--show-toplevel"], cwd).toString().trim();
    const prefix = git(["rev-parse", "--show-prefix"], cwd).toString().trim();
    const tgz = git(["archive", "--format=tar.gz", `HEAD:${prefix}`], top);
    const headers = { "content-type": "application/gzip", "idempotency-key": key, "x-roll-message": encodeURIComponent(message) };
    if (/^[0-9a-f]{40}$/i.test(sha)) headers["x-roll-revision"] = sha.toLowerCase();
    deployment = await call(fetchFn, api, "POST", `${envPath}/deploys`, token, Buffer.from(tgz), headers);
  } else if (kind === "image") {
    throw new Error("an image service deploys an image: pass image (a tag or digest of its repository)");
  } else {
    throw new Error(`${service} has a ${kind} source, which this action can't deploy`);
  }
  out.write(`roll apps: ${deployment.id} queued in ${environment}\n`);
  if (input("wait") !== "false") {
    const minutes = Number(input("timeout-minutes") || "30");
    const until = Date.now() + minutes * 60_000;
    let last = deployment.status;
    while (!DONE.has(deployment.status)) {
      if (Date.now() > until) throw new Error(`${deployment.id} is still ${deployment.status} after ${minutes} minutes`);
      await sleep(3000);
      deployment = await call(fetchFn, api, "GET", `${envPath}/deploys/${seg(deployment.id)}`, token);
      if (deployment.status !== last) out.write(`roll apps: ${deployment.id} is ${(last = deployment.status)}\n`);
    }
  }
  const envNow = await call(fetchFn, api, "GET", envPath, token);
  const url = envNow?.url || "";
  if (env.GITHUB_OUTPUT) {
    const d = delimiter(url);
    fs.appendFileSync(env.GITHUB_OUTPUT, `deployment-id=${deployment.id}\nstatus=${deployment.status}\nurl<<${d}\n${url}\n${d}\n`);
  }
  if (env.GITHUB_STEP_SUMMARY) fs.appendFileSync(env.GITHUB_STEP_SUMMARY, `**roll apps** ${environment}: \`${deployment.id}\` ${deployment.status}${url ? ` at ${url}` : ""}\n`);
  if (input("comment") === "true" && input("github-token") && DONE.has(deployment.status)) {
    const said = deployment.status === "active" ? `Deployed to **${environment}**${url ? `: ${url}` : ""}` : `The deploy to **${environment}** is ${deployment.status}`;
    await comment(fetchFn, env, input("github-token"), `<!-- roll-apps:${service}:${environment} -->`, `${said} (\`${deployment.id}\`, ${sha.slice(0, 12)}).`);
  }
  if (["failed", "canceled"].includes(deployment.status)) {
    // The build's lines are the owner's: printed only when asked (a public repository's job log
    // is public).
    if (input("show-logs") === "true") {
      const lines = await call(fetchFn, api, "GET", `${envPath}/deploys/${seg(deployment.id)}/logs?tail=80`, token).catch(() => []);
      for (const l of lines || []) out.write(`${l.text}\n`);
    }
    throw new Error(`${deployment.id} ${deployment.status}${input("show-logs") === "true" ? "" : " (its log is in roll; show-logs: true prints it here)"}`);
  }
  if (deployment.status === "superseded") out.write(`::notice::${deployment.id} was superseded by a newer deploy before it went live\n`);
  else out.write(`roll apps: ${deployment.id} ${deployment.status}${url ? ` at ${url}` : ""}\n`);
  return deployment;
}

module.exports = { run };

if (require.main === module) {
  run(process.env, process.stdout).catch((e) => {
    process.stdout.write(`::error::${String(e.message || e).replace(/\r?\n/g, " ")}\n`);
    process.exitCode = 1;
  });
}
