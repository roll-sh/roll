// The action against a fake rolld and a fake GitHub OIDC endpoint: `node --test sdk/github-action`.
"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");
const { run } = require("./index.js");

function fakeFetch(values) {
  const seen = [];
  const fetchFn = async (url, init = {}) => {
    const u = new URL(url);
    const auth = (init.headers || {}).authorization;
    seen.push(`${init.method || "GET"} ${u.pathname}`);
    const json = (status, body) => new Response(JSON.stringify(body), { status, statusText: status === 200 ? "OK" : "No" });
    if (u.host === "github.oidc") {
      assert.equal(auth, "Bearer gh-request");
      assert.equal(u.searchParams.get("audience"), "https://roll.test");
      return json(200, { value: "the-oidc-token" });
    }
    if (u.pathname === "/v1/secrets/login") {
      const b = JSON.parse(init.body);
      return b.identity === "idn_ci" && b.token === "the-oidc-token" ? json(200, { token: "rlt_workload", project: "prj_a" }) : json(401, { error: "no identity takes this token" });
    }
    if (auth !== "Bearer rlt_workload") return json(401, { error: "no token" });
    if (u.pathname === "/v1/secrets/projects/prj_a/environments/staging/values") return json(200, { values, problems: [] });
    return json(404, { error: "no route" });
  };
  return { fetchFn, seen };
}

test("a job logs in with its OIDC token, masks every value and exports them", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "roll-action-"));
  const ghEnv = path.join(dir, "env");
  const ghOut = path.join(dir, "out");
  const values = { DB_URL: "postgres://db/shop", PEM: "-----BEGIN KEY-----\nabc\n-----END KEY-----" };
  const { fetchFn } = fakeFetch(values);
  let printed = "";
  const out = { write: (s) => (printed += s) };
  const env = {
    "INPUT_API-URL": "https://roll.test/",
    INPUT_IDENTITY: "idn_ci",
    INPUT_ENVIRONMENT: "staging",
    INPUT_EXPORT: "true",
    ACTIONS_ID_TOKEN_REQUEST_URL: "https://github.oidc/token?x=1",
    ACTIONS_ID_TOKEN_REQUEST_TOKEN: "gh-request",
    GITHUB_ENV: ghEnv,
    GITHUB_OUTPUT: ghOut,
  };
  assert.deepEqual(await run(env, out, fetchFn), ["DB_URL", "PEM"]);
  // Every value line, the OIDC token and the roll token are masked before anything else is printed.
  for (const secret of ["the-oidc-token", "rlt_workload", "postgres://db/shop", "-----BEGIN KEY-----", "abc", "-----END KEY-----"]) {
    assert.ok(printed.includes(`::add-mask::${secret}\n`), secret);
  }
  assert.ok(!printed.replace(/::add-mask::.*\n/g, "").includes("postgres://"), "no value printed unmasked");
  // Exported with delimiters a value can't close early, multi-line values whole.
  const exported = fs.readFileSync(ghEnv, "utf8");
  const parsed = {};
  for (const m of exported.matchAll(/^([A-Z_]+)<<(roll_[0-9a-f]+)\n([\s\S]*?)\n\2\n/gm)) parsed[m[1]] = m[3];
  assert.deepEqual(parsed, values);
  assert.match(fs.readFileSync(ghOut, "utf8"), /^keys<<(roll_[0-9a-f]+)\nDB_URL,PEM\n\1\n$/);
  // An identity that doesn't take the token: the job fails, saying why, with no value anywhere.
  const bad = { ...env, ACTIONS_ID_TOKEN_REQUEST_URL: "https://github.oidc/token", INPUT_IDENTITY: "idn_other" };
  await assert.rejects(run(bad, out, fetchFn), /no identity takes this token/);
});
