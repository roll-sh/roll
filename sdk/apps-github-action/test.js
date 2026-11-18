// The deploy action against a fake rolld and a fake GitHub API: `node --test sdk/apps-github-action`.
"use strict";

const assert = require("node:assert/strict");
const childProcess = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const zlib = require("node:zlib");
const test = require("node:test");
const { run } = require("./index.js");

const SHA = "a".repeat(40);

/** rolld with one service of `kind` whose deployment goes through `statuses`; GitHub's comments. */
function fake(kind, statuses) {
  const seen = { deploys: [], comments: [], logs: 0 };
  let polls = 0;
  const fetchFn = async (url, init = {}) => {
    const u = new URL(url);
    const h = init.headers || {};
    const json = (status, body) => new Response(JSON.stringify(body), { status });
    const method = init.method || "GET";
    if (u.host === "api.github.test") {
      assert.equal(h.authorization, "Bearer gh-token");
      if (method === "GET") return json(200, seen.comments);
      const body = JSON.parse(init.body).body;
      if (method === "POST") seen.comments.push({ id: seen.comments.length + 1, body });
      else seen.comments.find((c) => u.pathname.endsWith(`/${c.id}`)).body = body;
      return json(200, {});
    }
    // GitHub Actions' OIDC tokens, and roll's trusted-client sign-in that trades one.
    if (u.host === "token.actions.test") {
      assert.equal(h.authorization, "Bearer gh-oidc-request");
      return json(200, { value: `jwt-for-${u.searchParams.get("audience")}` });
    }
    if (method === "POST" && u.pathname === "/v1/access/oauth/token") {
      const f = new URLSearchParams(init.body);
      assert.equal(f.get("grant_type"), "urn:ietf:params:oauth:grant-type:jwt-bearer");
      if (f.get("client_id") !== "mcl_ci" || f.get("assertion") !== "jwt-for-https://roll.test") return json(400, { error: "invalid_grant", error_description: "the token's sub doesn't match the client's subject" });
      return json(200, { access_token: "rlt_ci", token_type: "Bearer", expires_in: 900 });
    }
    if (h.authorization !== "Bearer rlt_ci") return json(401, { error: "no token" });
    const env = "/v1/apps/services/svc_web/environments/production";
    if (method === "GET" && u.pathname === "/v1/apps/services/svc_web") return json(200, { id: "svc_web", source: { kind } });
    if (method === "POST" && u.pathname === `${env}/deploys`) {
      seen.deploys.push({ headers: h, body: init.body });
      return json(202, { id: "dep_1", status: "queued" });
    }
    if (method === "GET" && u.pathname === `${env}/deploys/dep_1`) return json(200, { id: "dep_1", status: statuses[Math.min(polls++, statuses.length - 1)] });
    if (method === "GET" && u.pathname === `${env}/deploys/dep_1/logs`) {
      seen.logs++;
      return json(200, [{ text: "npm ERR! missing script: build" }]);
    }
    if (method === "GET" && u.pathname === env) return json(200, { url: "https://web.roll.test" });
    return json(404, { error: `no route ${method} ${u.pathname}` });
  };
  return { fetchFn, seen };
}

function job(extra = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "roll-apps-action-"));
  const event = path.join(dir, "event.json");
  fs.writeFileSync(event, JSON.stringify({ pull_request: { number: 7, head: { sha: SHA } } }));
  const env = {
    "INPUT_API-URL": "https://roll.test/",
    INPUT_TOKEN: "rlt_ci",
    INPUT_SERVICE: "svc_web",
    INPUT_COMMENT: "true",
    "INPUT_GITHUB-TOKEN": "gh-token",
    GITHUB_SHA: "f".repeat(40), // a pull request's merge commit: not what deploys
    GITHUB_REPOSITORY: "acme/web",
    GITHUB_RUN_ID: "99",
    GITHUB_RUN_ATTEMPT: "1",
    GITHUB_EVENT_PATH: event,
    GITHUB_API_URL: "https://api.github.test",
    GITHUB_OUTPUT: path.join(dir, "out"),
    ...extra,
  };
  let printed = "";
  return { dir, env, out: { write: (s) => (printed += s) }, printed: () => printed };
}

const noSleep = async () => {};

test("a repo service deploys the workflow's commit, waits for it and comments once per pull request", async () => {
  const { fetchFn, seen } = fake("repo", ["building", "deploying", "active"]);
  const j = job();
  const exec = () => Buffer.from("fix the header\n");
  const d = await run(j.env, j.out, fetchFn, exec, noSleep);
  assert.equal(d.status, "active");
  assert.deepEqual(JSON.parse(seen.deploys[0].body), { revision: SHA, message: "fix the header" });
  assert.equal(seen.deploys[0].headers["idempotency-key"], "github:acme/web:99:1:production");
  assert.match(j.printed(), /::add-mask::rlt_ci/);
  assert.match(fs.readFileSync(j.env.GITHUB_OUTPUT, "utf8"), /deployment-id=dep_1\nstatus=active\nurl<<(roll_\w+)\nhttps:\/\/web\.roll\.test\n\1\n/);
  // A second run updates the same comment.
  await run({ ...j.env, GITHUB_RUN_ID: "100" }, j.out, fetchFn, exec, noSleep);
  assert.equal(seen.comments.length, 1);
  assert.match(seen.comments[0].body, /^<!-- roll-apps:svc_web:production -->\nDeployed to \*\*production\*\*: https:\/\/web\.roll\.test/);
});

test("an upload service sends the checkout's committed files, and a failure keeps its log unless asked", async () => {
  const j = job({ INPUT_COMMENT: "false" });
  const repo = path.join(j.dir, "repo");
  fs.mkdirSync(path.join(repo, "web"), { recursive: true });
  const git = (...a) => childProcess.execFileSync("git", ["-c", "user.name=a", "-c", "user.email=a@example.com", ...a], { cwd: repo });
  git("init", "-q");
  fs.writeFileSync(path.join(repo, "web", "index.html"), "hi");
  fs.writeFileSync(path.join(repo, "web", ".gitignore"), "secret.env\n");
  git("add", ".");
  git("commit", "-qm", "the site");
  fs.writeFileSync(path.join(repo, "web", "secret.env"), "TOKEN=kept-out");
  fs.writeFileSync(path.join(repo, "web", "untracked.txt"), "UNTRACKED-BYTES");
  const { fetchFn, seen } = fake("upload", ["failed"]);
  await assert.rejects(run({ ...j.env, INPUT_PATH: path.join(repo, "web") }, j.out, fetchFn, childProcess.execFileSync, noSleep), /dep_1 failed \(its log is in roll/);
  const sent = seen.deploys[0];
  assert.equal(sent.headers["content-type"], "application/gzip");
  assert.equal(decodeURIComponent(sent.headers["x-roll-message"]), "the site");
  const names = zlib.gunzipSync(sent.body).toString("latin1");
  assert.ok(names.includes("index.html") && !names.includes("web/index.html"), "what git tracks under web/, from web/");
  assert.ok(!names.includes("kept-out") && !names.includes("UNTRACKED-BYTES"), "neither ignored nor untracked files");
  assert.equal(seen.logs, 0);
  await assert.rejects(run({ ...j.env, INPUT_PATH: path.join(repo, "web"), "INPUT_SHOW-LOGS": "true" }, j.out, fetchFn, childProcess.execFileSync, noSleep), /dep_1 failed$/);
  assert.match(j.printed(), /npm ERR! missing script: build/);
});

test("with client-id the job signs in with its own OIDC token, masked, and nothing is stored", async () => {
  const { fetchFn, seen } = fake("repo", ["active"]);
  const oidc = { "INPUT_CLIENT-ID": "mcl_ci", ACTIONS_ID_TOKEN_REQUEST_URL: "https://token.actions.test/id?x=1", ACTIONS_ID_TOKEN_REQUEST_TOKEN: "gh-oidc-request" };
  const j = job({ INPUT_TOKEN: "", INPUT_COMMENT: "false", ...oidc });
  const d = await run(j.env, j.out, fetchFn, () => Buffer.from("x\n"), noSleep);
  assert.equal(d.status, "active");
  assert.equal(seen.deploys.length, 1);
  assert.ok(j.printed().includes("::add-mask::jwt-for-https://roll.test\n") && j.printed().includes("::add-mask::rlt_ci\n"), j.printed());
  const wrong = job({ INPUT_TOKEN: "", "INPUT_CLIENT-ID": "mcl_other", ACTIONS_ID_TOKEN_REQUEST_URL: oidc.ACTIONS_ID_TOKEN_REQUEST_URL, ACTIONS_ID_TOKEN_REQUEST_TOKEN: "gh-oidc-request" });
  await assert.rejects(run(wrong.env, wrong.out, fetchFn, () => Buffer.from("x\n"), noSleep), /didn't sign the job in as mcl_other: invalid_grant: the token's sub doesn't match/);
  const both = job({ "INPUT_CLIENT-ID": "mcl_ci" });
  await assert.rejects(run(both.env, both.out, fetchFn, () => Buffer.from("x\n"), noSleep), /client-id .* or token, not both/);
  const noPermission = job({ INPUT_TOKEN: "", "INPUT_CLIENT-ID": "mcl_ci" });
  await assert.rejects(run(noPermission.env, noPermission.out, fetchFn, () => Buffer.from("x\n"), noSleep), /id-token: write/);
});

test("a fork's pull request, with no token, is told why", async () => {
  const j = job({ INPUT_TOKEN: "", GITHUB_EVENT_NAME: "pull_request" });
  await assert.rejects(run(j.env, j.out, async () => assert.fail("nothing is fetched"), () => Buffer.from(""), noSleep), /fork's pull request/);
});
