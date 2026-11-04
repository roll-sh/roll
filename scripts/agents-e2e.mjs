// The roll code agents end to end (`pnpm test:agents`, after the unit tests): the real binaries on
// free ports with a throwaway home. rolld --code-only; an owner; `roll agent login` approved
// through the API (after looking the pending login up); `ROLL_AGENT_FAKE=1 roll agent up`; then
// over HTTP and SSE: project add → thread create → a streamed run → its approval answered → the
// checkpoint and its diff → a share → the share read anonymously; the dashboard in a production
// build against that rolld (`/dashboard/agents`, the share page, its preview image, a missing
// share's 404); and `roll agent logout`, which stops `up`.
//
// Usage: node scripts/agents-e2e.mjs <dir with roll and rolld> [--no-dashboard]
// `AGENTS_E2E_KEEP=1` leaves everything running after the checks (Ctrl-C ends it), to look at.
// Children are stopped by the PIDs this script started; their logs are printed on failure.

import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { createServer } from "node:net";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";

const [binDir, ...flags] = process.argv.slice(2);
if (!binDir) {
  console.error("usage: node scripts/agents-e2e.mjs <target/debug> [--no-dashboard]");
  process.exit(2);
}
const ROOT = resolve(import.meta.dirname, "..");
const ROLL = resolve(binDir, "roll");
const ROLLD = resolve(binDir, "rolld");
const withDashboard = !flags.includes("--no-dashboard");
const base = process.env.AGENTS_E2E_DIR || join(homedir(), ".cache");
mkdirSync(base, { recursive: true });
const dir = mkdtempSync(join(base, "roll-agents-e2e-"));
const children = [];
const t0 = Date.now();

function log(...a) {
  console.log(`agents e2e [${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);
}

function fail(msg) {
  throw new Error(msg);
}

function check(cond, msg) {
  if (!cond) fail(msg);
}

async function freePort() {
  return new Promise((res, rej) => {
    const s = createServer();
    s.once("error", rej);
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => res(port));
    });
  });
}

/** Clean environment for the children: nothing from the developer's roll setup leaks in. */
function env(extra = {}) {
  const e = { ...process.env };
  for (const k of Object.keys(e)) if (k.startsWith("ROLL_") || k.startsWith("VITE_ROLL_")) delete e[k];
  return { ...e, NO_COLOR: "1", RUST_LOG: "warn", ...extra };
}

function start(name, cmd, args, opts = {}) {
  const p = spawn(cmd, args, { stdio: ["ignore", "pipe", "pipe"], ...opts });
  const c = { name, p, out: "", err: "", exited: null };
  p.stdout.on("data", (d) => (c.out += d));
  p.stderr.on("data", (d) => (c.err += d));
  p.on("exit", (code, signal) => (c.exited = { code, signal }));
  children.push(c);
  return c;
}

async function exited(c, ms) {
  const end = Date.now() + ms;
  while (!c.exited && Date.now() < end) await sleep(50);
  return c.exited;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(what, fn, ms = 20_000, every = 100) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e;
    }
    await sleep(every);
  }
  fail(`timed out waiting for ${what}${last ? `: ${last.message}` : ""}`);
}

let API;
async function req(method, path, body, headers = {}) {
  const res = await fetch(`${API}${path}`, {
    method,
    headers: { ...(body === undefined ? {} : { "content-type": "application/json" }), ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = text;
  }
  return { status: res.status, json, headers: res.headers, text };
}

let nextCmd = 0;
async function command(machine, cmd) {
  const r = await req("POST", `/v1/code/agents/acme/machines/${machine}/commands`, { id: `e2e_${++nextCmd}`, command: cmd });
  check(r.status === 200, `${cmd.type}: ${r.status} ${r.text}`);
  return r.json;
}

/** An SSE stream as an async queue of {id, event, data}. */
function sse(path, headers = {}) {
  const ctrl = new AbortController();
  const queue = [];
  const history = [];
  let wake = null;
  let ended = null;
  (async () => {
    try {
      const res = await fetch(`${API}${path}`, { headers: { accept: "text/event-stream", ...headers }, signal: ctrl.signal });
      check(res.status === 200, `SSE ${path}: ${res.status}`);
      const dec = new TextDecoder();
      let buf = "";
      for await (const chunk of res.body) {
        buf += dec.decode(chunk, { stream: true });
        let i;
        while ((i = buf.indexOf("\n\n")) >= 0) {
          const block = buf.slice(0, i);
          buf = buf.slice(i + 2);
          const ev = { id: null, event: "message", data: "" };
          for (const line of block.split("\n")) {
            if (line.startsWith("id:")) ev.id = line.slice(3).trim();
            else if (line.startsWith("event:")) ev.event = line.slice(6).trim();
            else if (line.startsWith("data:")) ev.data += line.slice(5).trim();
          }
          if (!ev.data) continue;
          ev.data = JSON.parse(ev.data);
          queue.push(ev);
          history.push(ev);
          wake?.();
        }
      }
    } catch (e) {
      if (!ctrl.signal.aborted) ended = e;
    }
    ended ??= new Error("stream ended");
    wake?.();
  })();
  return {
    close: () => ctrl.abort(),
    /** The next event `pred` accepts (others are kept in `seen`). */
    async until(what, pred, ms = 20_000) {
      const end = Date.now() + ms;
      for (;;) {
        const i = queue.findIndex(pred);
        if (i >= 0) return queue.splice(0, i + 1).pop();
        if (ended) fail(`${what}: ${ended.message}`);
        if (Date.now() > end) fail(`timed out waiting for ${what}`);
        await new Promise((r) => {
          wake = r;
          setTimeout(r, 200);
        });
      }
    },
    /** An event `pred` accepts, already received or still to come. */
    async any(what, pred, ms) {
      return history.find(pred) ?? this.until(what, pred, ms);
    },
  };
}

function git(cwd, ...args) {
  execFileSync("git", args, { cwd, stdio: "ignore", env: { ...process.env, GIT_AUTHOR_NAME: "e2e", GIT_AUTHOR_EMAIL: "e2e@roll.test", GIT_COMMITTER_NAME: "e2e", GIT_COMMITTER_EMAIL: "e2e@roll.test" } });
}

async function main() {
  const apiPort = await freePort();
  const webPort = await freePort();
  API = `http://127.0.0.1:${apiPort}`;
  const WEB = `http://127.0.0.1:${webPort}`;
  log(`home ${dir}; rolld on ${API}${withDashboard ? `, dashboard on ${WEB}` : ""}`);

  // ── rolld ──
  const rolld = start("rolld", ROLLD, ["--code-only", "--home", join(dir, "rolld"), "--api", `127.0.0.1:${apiPort}`], {
    env: env({ ROLL_WEB_URL: WEB, ROLL_CODE_PUBLIC_URL: API }),
  });
  await until("rolld to answer", async () => (await req("GET", "/v1/code")).status === 200);
  const owner = await req("POST", "/v1/code/owners", { name: "acme", kind: "org" });
  check(owner.status === 201, `owner: ${owner.status} ${owner.text}`);
  log("rolld up, owner acme");

  // ── roll agent login, approved through the API ──
  const agentEnv = env({ ROLL_API_URL: API, ROLL_AGENT_CONFIG: join(dir, "agent.json"), ROLL_AGENT_HOME: join(dir, "agent") });
  const login = start("roll agent login", ROLL, ["agent", "login", "--name", "e2e-box"], { env: agentEnv });
  const code = await until("login to print its code", () => /confirm the code ([A-Z]{4}-[A-Z]{4})/.exec(login.out)?.[1]);
  const pending = await req("GET", `/v1/code/agents/acme/login/${code}`);
  check(pending.status === 200 && pending.json.name === "e2e-box" && pending.json.hostname && pending.json.os, `pending login: ${pending.text}`);
  check((await req("POST", "/v1/code/agents/acme/login/approve", { user_code: code })).status === 204, "approve");
  const done = await exited(login, 20_000);
  check(done?.code === 0, `login exited ${JSON.stringify(done)}: ${login.err}`);
  const saved = JSON.parse(readFileSync(join(dir, "agent.json"), "utf8"));
  check(saved.token.startsWith("rlm_") && saved.owner === "acme", "agent.json");
  const machine = saved.machine;
  log(`login approved: ${machine} (${pending.json.hostname}, ${pending.json.os})`);

  // ── roll agent up with the fake providers ──
  const up = start("roll agent up", ROLL, ["agent", "up"], { env: { ...agentEnv, ROLL_AGENT_FAKE: "1" } });
  const online = await until("the machine to come online", async () => {
    const m = (await req("GET", "/v1/code/agents/acme/machines")).json?.find((x) => x.id === machine);
    return m?.online && m.info ? m : null;
  });
  const providers = online.info.providers.map((p) => p.id).sort();
  check(JSON.stringify(providers) === JSON.stringify(["echo", "fake"]), `providers: ${providers}`);
  log("daemon online with the fake providers");

  // ── a project in a git repo, a thread ──
  const repo = join(dir, "repo");
  mkdirSync(repo);
  git(repo, "init", "-q", "-b", "main");
  writeFileSync(join(repo, "README.md"), "e2e\n");
  git(repo, "add", ".");
  git(repo, "commit", "-q", "-m", "first");
  const prj = await command(machine, { type: "project.add", path: repo, name: "repo" });
  check(prj.type === "created", `project.add: ${JSON.stringify(prj)}`);
  const thr = await command(machine, { type: "thread.create", project: prj.id, provider: "fake", runtime_mode: "ask", title: "Write the demo file" });
  check(thr.type === "created", `thread.create: ${JSON.stringify(thr)}`);
  const thread = thr.id;
  // The reply can beat the record to the relay: the snapshot has it a moment later.
  const state = await until("the thread in the snapshot", async () => {
    const r = await req("GET", `/v1/code/agents/acme/state?threads=${thread}`);
    return r.status === 200 && r.json.entities.some((m) => m.entity.id === thread) ? r : null;
  });

  // ── the run, streamed ──
  const events = sse(`/v1/code/agents/acme/events?threads=${thread}&after=${state.json.cursor}`);
  const sent = await command(machine, { type: "message.send", thread, text: "hello from e2e" });
  check(sent.type === "created" || sent.type === "ok", `message.send: ${JSON.stringify(sent)}`);
  await events.until("a streamed text delta", (e) => e.event === "delta" && e.data.delta.type === "text" && e.data.delta.thread === thread);
  const asked = await events.until("the approval request", (e) => e.event === "entity" && e.data.entity.kind === "request" && e.data.entity.status === "pending");
  check(asked.id && Number(asked.id) > state.json.cursor, "durable events carry their cursor");
  const waiting = await events.until("the thread waiting", (e) => e.event === "entity" && e.data.entity.kind === "thread" && e.data.entity.status === "waiting", 10_000).catch(() => null);
  log(`streamed; approval ${asked.data.entity.id} pending${waiting ? ", thread waiting" : ""}`);
  check(!existsSync(join(repo, "ROLL_DEMO.md")), "nothing written before the approval");
  const answered = await command(machine, { type: "request.respond", request: asked.data.entity.id, response: { type: "decision", decision: "accept" } });
  check(answered.type === "ok", `request.respond: ${JSON.stringify(answered)}`);
  const finished = await events.until("the run to complete with a checkpoint", (e) => e.event === "entity" && e.data.entity.kind === "run" && e.data.entity.status === "completed" && e.data.entity.checkpoint);
  const run = finished.data.entity;
  await events.any("the checkpoint record", (e) => e.event === "entity" && e.data.entity.kind === "checkpoint" && e.data.entity.run === run.id);
  events.close();
  check(readFileSync(join(repo, "ROLL_DEMO.md"), "utf8") === "hello from e2e\n", "the agent wrote the file");
  check(run.stat && run.stat.files === 1 && run.stat.additions === 1, `run stat ${JSON.stringify(run.stat)}`);

  // ── the checkpoint's diff ──
  const diff = await command(machine, { type: "diff.get", thread, run: run.id });
  check(diff.type === "diff" && diff.files.some((f) => f.path === "ROLL_DEMO.md" && f.change === "add" && f.diff.includes("+hello from e2e")), `diff: ${JSON.stringify(diff).slice(0, 400)}`);
  log("run completed; checkpoint diff has ROLL_DEMO.md");

  // A reconnect with both cursors resumes after the newer.
  const replay = sse(`/v1/code/agents/acme/events?threads=${thread}&after=${state.json.cursor}`, { "last-event-id": String(Number(finished.id) - 1) });
  const first = await replay.until("the resumed stream", (e) => e.id !== null);
  check(Number(first.id) >= Number(finished.id), `resumed at ${first.id}, after ${Number(finished.id) - 1}`);
  replay.close();

  // ── a share, read anonymously ──
  const shared = await req("POST", `/v1/code/agents/acme/threads/${thread}/shares`, {});
  check(shared.status === 201 && shared.json.url === `${WEB}/agents/s/${shared.json.id}`, `share: ${shared.status} ${shared.text}`);
  const share = await req("GET", `/v1/code/agents/shares/${shared.json.id}`);
  check(share.status === 200 && share.headers.get("cache-control") === "no-store", `share read: ${share.status}`);
  const kinds = new Set(share.json.entities.map((e) => e.kind));
  for (const k of ["project", "thread", "run", "node", "request", "checkpoint"]) check(kinds.has(k), `the share has a ${k}`);
  check(!share.text.includes(homedir()), "the share has no home directory in it");
  check(share.json.entities.some((e) => e.kind === "node" && e.body.type === "tool" && e.body.files?.some((f) => f.path === "ROLL_DEMO.md")), "the share has the edit");
  log(`shared as ${shared.json.id} (${shared.json.bytes} bytes)`);

  // ── the dashboard, production build, against this rolld ──
  if (withDashboard) {
    const dash = join(ROOT, "apps/dashboard");
    log("building the dashboard (vite build)…");
    // Into `dist/` with this rolld's address built into the client; the server reads ROLL_API_URL.
    const webEnv = env({ VITE_ROLL_API: API, ROLL_API_URL: API });
    execFileSync("pnpm", ["exec", "vite", "build"], { cwd: dash, env: webEnv, stdio: ["ignore", "inherit", "inherit"] });
    start("dashboard", "node", ["server.mjs", "-H", "127.0.0.1", "-p", String(webPort)], { cwd: dash, env: webEnv });
    const page = async (path) => {
      const r = await fetch(`${WEB}${path}`, { redirect: "manual" });
      return { status: r.status, text: await r.text(), type: r.headers.get("content-type") };
    };
    await until("the dashboard to answer", async () => (await page("/dashboard/agents")).status === 200, 60_000, 500);
    const canvas = await page("/dashboard/agents");
    check(canvas.text.includes("<title>Agents · roll</title>"), "/dashboard/agents has its title");
    const sp = await page(`/agents/s/${shared.json.id}`);
    check(sp.status === 200, `share page: ${sp.status}`);
    check(sp.text.includes("<title>Write the demo file · roll</title>"), "the share page is titled after the thread");
    check(sp.text.includes('property="og:title" content="Write the demo file"'), "the share page has OpenGraph metadata");
    check(sp.text.includes('name="robots" content="noindex, nofollow"'), "the share page isn't indexed");
    const og = /property="og:image" content="([^"]+)"/.exec(sp.text)?.[1];
    check(og, "the share page has a preview image");
    check(og.startsWith(`${WEB}/`), `the preview image is on the host the page was asked on: ${og}`);
    const img = await fetch(og);
    check(img.status === 200 && img.headers.get("content-type") === "image/png", `preview image: ${img.status} ${img.headers.get("content-type")}`);
    check((await img.arrayBuffer()).byteLength > 1000, "the preview image has content");
    const missing = await page("/agents/s/shr_000000000000000000000000");
    check(missing.status === 404, `a missing share: ${missing.status}`);
    log("dashboard: /dashboard/agents, the share page, its preview image, a missing share's 404");
  }

  if (process.env.AGENTS_E2E_KEEP) {
    log(`kept running: ${WEB}/agents/s/${shared.json.id} and ${WEB}/dashboard/agents?owner=acme; Ctrl-C to stop`);
    await new Promise((r) => process.once("SIGINT", r));
  }

  // ── logout ──
  const out = execFileSync(ROLL, ["agent", "logout"], { env: agentEnv }).toString();
  check(out.includes("Removed e2e-box"), `logout: ${out}`);
  const stopped = await exited(up, 20_000);
  check(stopped?.code === 1 && up.err.includes("roll agent login"), `up after logout: ${JSON.stringify(stopped)} ${up.err.slice(-300)}`);
  check(((await req("GET", "/v1/code/agents/acme/machines")).json ?? []).length === 0, "the machine is gone");
  const after = await req("GET", `/v1/code/agents/shares/${shared.json.id}`);
  check(after.status === 200, "a share outlives its machine");
  log("logged out: up stopped, the machine is gone, the share stays");
  rolld.p.kill("SIGTERM");
  await exited(rolld, 10_000);
}

let ok = false;
try {
  await main();
  ok = true;
} catch (e) {
  console.error(`agents e2e: FAILED: ${e.message}`);
  for (const c of children) {
    console.error(`--- ${c.name} (exit ${JSON.stringify(c.exited)}) stderr ---\n${c.err.slice(-4000)}\n--- stdout ---\n${c.out.slice(-2000)}`);
  }
} finally {
  for (const c of children) if (!c.exited) c.p.kill("SIGTERM");
  await sleep(300);
  for (const c of children) if (!c.exited) c.p.kill("SIGKILL");
  if (ok) rmSync(dir, { recursive: true, force: true });
  else console.error(`agents e2e: kept ${dir} for a look`);
}
console.log(ok ? `agents e2e: all green in ${((Date.now() - t0) / 1000).toFixed(1)}s` : "");
process.exit(ok ? 0 : 1);
