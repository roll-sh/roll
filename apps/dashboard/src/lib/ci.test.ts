import { describe, expect, it } from "vitest";
import type { Check, Status } from "./code-api";
import {
  tokenLine,
  CI_SYSTEMS,
  checkRows,
  emptyWebhookForm,
  eventsLabel,
  mcpCommand,
  mcpJson,
  statusCurl,
  statusScript,
  statusToCheck,
  webhookBody,
  webhookErrors,
  webhookForm,
} from "./ci";

describe("webhook forms", () => {
  it("builds a create body with events in the contract's order", () => {
    const f = { ...emptyWebhookForm(), url: " https://ci.example.com/h ", events: ["push", "landed", "check"] as const };
    expect(webhookBody({ ...f, events: [...f.events] })).toEqual({ url: "https://ci.example.com/h", events: ["landed", "check", "push"], active: true });
  });

  it("sends every event as *, and the secret only when typed", () => {
    const f = { ...emptyWebhookForm(), url: "https://x.dev", all: true, secret: "  s3cret-s3cret-s3cret  " };
    expect(webhookBody(f)).toEqual({ url: "https://x.dev", events: ["*"], active: true, secret: "s3cret-s3cret-s3cret" });
  });

  it("sends only what changed on update", () => {
    const prev = { url: "https://x.dev", events: ["landed", "push"], active: true };
    expect(webhookBody(webhookForm(prev), prev)).toEqual({});
    expect(webhookBody({ ...webhookForm(prev), active: false }, prev)).toEqual({ active: false });
    expect(webhookBody({ ...webhookForm(prev), events: ["landed"] }, prev)).toEqual({ events: ["landed"] });
    expect(webhookBody({ ...webhookForm(prev), all: true }, prev)).toEqual({ events: ["*"] });
  });

  it("reads an existing webhook back into the form", () => {
    expect(webhookForm({ url: "https://x.dev", events: ["*"], active: false })).toEqual({ url: "https://x.dev", all: true, events: [], secret: "", active: false });
    // Events it doesn't know (a newer server) are dropped from the checkboxes.
    expect(webhookForm({ url: "u", events: ["landed", "deploy"], active: true }).events).toEqual(["landed"]);
  });

  it("validates url, events and secret", () => {
    expect(webhookErrors({ ...emptyWebhookForm(), url: "https://x.dev" })).toEqual({});
    expect(webhookErrors({ ...emptyWebhookForm(), url: "ftp://x.dev" }).url).toBeTruthy();
    expect(webhookErrors({ ...emptyWebhookForm(), url: "not a url" }).url).toBeTruthy();
    expect(webhookErrors({ ...emptyWebhookForm(), url: "https://x.dev", events: [] }).events).toBeTruthy();
    expect(webhookErrors({ ...emptyWebhookForm(), url: "https://x.dev", all: true, events: [] }).events).toBeUndefined();
    expect(webhookErrors({ ...emptyWebhookForm(), url: "https://x.dev", secret: "short" }).secret).toBeTruthy();
    expect(webhookErrors({ ...emptyWebhookForm(), url: "https://x.dev", secret: "x".repeat(201) }).secret).toBeTruthy();
  });

  it("labels events", () => {
    expect(eventsLabel(["*"])).toBe("all events");
    expect(eventsLabel(["landed", "push"])).toBe("landed, push");
    expect(eventsLabel([])).toBe("no events");
  });
});

describe("CI status snippets", () => {
  const api = "https://api.roll.sh/v1/code/repos/acme/api";
  const tc = CI_SYSTEMS.find((c) => c.name === "TeamCity")!;

  it("posts to the commit CI built, with the token and a JSON body that keeps CI variables", () => {
    const s = statusCurl(api, tc, "pending", "ci/teamcity", "running");
    expect(s).toContain(`"${api}/statuses/%build.vcs.number%"`);
    expect(s).toContain('-H "Authorization: Bearer $ROLL_TOKEN"');
    expect(s).toContain('-d "{\\"state\\":\\"pending\\",\\"context\\":\\"ci/teamcity\\",\\"target_url\\":\\"%teamcity.serverUrl%/viewLog.html?buildId=%teamcity.build.id%\\",\\"description\\":\\"running\\"}"');
  });

  it("covers every CI with its commit variable", () => {
    expect(CI_SYSTEMS.map((c) => c.commit)).toEqual(["%build.vcs.number%", "$GIT_COMMIT", "$BUILDKITE_COMMIT", "$GITHUB_SHA"]);
    for (const ci of CI_SYSTEMS) expect(statusCurl(api, ci, "success")).toContain(`/statuses/${ci.commit}"`);
  });

  it("scripts the start and the end of a run", () => {
    const s = statusScript(api, CI_SYSTEMS[1], "ci/jenkins");
    expect(s.match(/curl /g)).toHaveLength(2);
    expect(s).toContain('\\"state\\":\\"pending\\"');
    expect(s).toContain('\\"state\\":\\"$STATE\\"');
    expect(s).toContain("ci/jenkins");
  });
});

const check = (over: Partial<Check>): Check => ({
  name: "test",
  status: "success",
  source: "runner",
  url: null,
  summary: null,
  started_at: 1,
  finished_at: 2,
  ...over,
});
const status = (over: Partial<Status>): Status => ({
  context: "ci/teamcity",
  state: "success",
  target_url: "https://tc/1",
  description: "all green",
  creator: "ci",
  created_at: 5,
  updated_at: 6,
  ...over,
});

describe("checks with commit statuses", () => {
  it("adds who posted an external check, matched by context", () => {
    const rows = checkRows([check({ name: "ci/teamcity", source: "external", summary: "s" }), check({ name: "test" })], [status({})]);
    expect(rows[0]).toMatchObject({ name: "ci/teamcity", source: "CI status", creator: "ci", summary: "all green", url: "https://tc/1", updated: 6 });
    expect(rows[1]).toMatchObject({ name: "test", source: "roll", creator: null });
  });

  it("lists statuses that have no check yet", () => {
    const rows = checkRows([], [status({ context: "ci/jenkins", state: "pending" })]);
    expect(rows).toEqual([
      expect.objectContaining({ name: "ci/jenkins", status: "running", source: "CI status", creator: "ci" }),
    ]);
  });

  it("flags errors (not failing tests) from either side", () => {
    expect(checkRows([check({ name: "ci", source: "external", status: "failure" })], [status({ context: "ci", state: "error" })])[0].errored).toBe(true);
    expect(checkRows([check({ source: "external", status: "failure", summary: "error: runner lost" })], null)[0].errored).toBe(true);
    expect(checkRows([check({ status: "failure", summary: "3 tests failed" })], [])[0].errored).toBe(false);
  });

  it("doesn't take a status for a runner check of the same name", () => {
    const rows = checkRows([check({ name: "ci/teamcity", source: "runner" })], [status({})]);
    expect(rows).toHaveLength(2);
    expect(rows[0].source).toBe("roll");
  });

  it("maps states like the server", () => {
    expect(statusToCheck("pending")).toBe("running");
    expect(statusToCheck("success")).toBe("success");
    expect(statusToCheck("failure")).toBe("failure");
    expect(statusToCheck("error")).toBe("failure");
  });
});

describe("MCP configs", () => {
  const url = "https://api.roll.sh/v1/code/mcp";

  it("builds claude mcp add with a token, a placeholder, or none for an open daemon", () => {
    expect(mcpCommand(url, "rlc_abc")).toBe(`claude mcp add --transport http roll ${url} \\\n  --header "Authorization: Bearer rlc_abc"`);
    expect(mcpCommand(url)).toContain("Bearer $ROLL_CODE_TOKEN");
    expect(mcpCommand(url, null)).toBe(`claude mcp add --transport http roll ${url}`);
  });

  it("builds the JSON config", () => {
    expect(JSON.parse(mcpJson(url, "rlc_abc", "fixer-7"))).toEqual({
      mcpServers: { roll: { type: "http", url, headers: { Authorization: "Bearer rlc_abc", "X-Roll-Agent": "fixer-7" } } },
    });
    expect(JSON.parse(mcpJson(url)).mcpServers.roll.headers.Authorization).toBe("Bearer rlc_…");
    expect(JSON.parse(mcpJson(url, null)).mcpServers.roll).toEqual({ type: "http", url });
  });
});

describe("access tokens", () => {
  const ago = (t: number) => `${t}s`;
  it("shows the prefix, or the id for tokens from before prefixes", () => {
    expect(tokenLine({ id: "tok_1", name: "ci", created_at: 1, last_used_at: 5, prefix: "rlc_ab12" }, ago)).toEqual({ id: "rlc_ab12…", used: "used 5s" });
    expect(tokenLine({ id: "tok_9f", name: "old", created_at: 1, last_used_at: null, prefix: null }, ago)).toEqual({ id: "tok_9f", used: "never used" });
  });
});
