// Tests of the dashboard's migration from Elasticsearch or OpenSearch (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import { aliasOffer, defaultDest, destProblem, displayHost, droppedByReason, mergeStarted, partial, queuedPicks, queueRetryDelay, retryableStart, shouldStartQueued, sourceFrom, sourceProblem, taskPoll, taskProgress, type MigrateStarted, type SourceForm } from "./search-migrate";

const form = (f: Partial<SourceForm>): SourceForm => ({ host: "https://es.example:9243", auth: "api_key", apiKey: "", username: "", password: "", ...f });

test("only the chosen credentials are sent, and a URL holding them is refused", () => {
  assert.deepEqual(sourceFrom(form({ apiKey: " k== ", username: "elastic", password: "p" })), { host: "https://es.example:9243", api_key: "k==" });
  assert.deepEqual(sourceFrom(form({ auth: "basic", apiKey: "k", username: "elastic", password: "p w" })), { host: "https://es.example:9243", username: "elastic", password: "p w" });
  assert.deepEqual(sourceFrom(form({ auth: "none", apiKey: "k" })), { host: "https://es.example:9243" });
  assert.equal(sourceProblem(form({ apiKey: "k" })), null);
  assert.match(sourceProblem(form({ host: "https://elastic:pw@es.example" })) ?? "", /own fields/);
  assert.match(sourceProblem(form({ host: "ftp://es.example" })) ?? "", /https/);
  assert.match(sourceProblem(form({})) ?? "", /API key/);
  assert.equal(displayHost("my-dep.es.us-east-1.aws.elastic.cloud"), "https://my-dep.es.us-east-1.aws.elastic.cloud:443");
  assert.equal(displayHost("http://10.0.0.5:9200/"), "http://10.0.0.5:9200");
});

test("copies get free, valid names", () => {
  const taken = new Set(["logs", "logs-migrated"]);
  assert.equal(defaultDest("products", taken), "products");
  assert.equal(defaultDest("logs", taken), "logs-migrated-2");
  assert.match(destProblem("Logs", taken, []) ?? "", /lowercase/);
  assert.match(destProblem("logs", taken, []) ?? "", /exists/);
  assert.match(destProblem("a", taken, ["a"]) ?? "", /same name/);
  assert.equal(destProblem("fresh", taken, ["other"]), null);
});

test("a task's progress, end and failure", () => {
  const running = taskProgress({ completed: false, task: { status: { total: 200, created: 50, updated: 0, version_conflicts: 0, noops: 0 } } });
  assert.deepEqual([running.state, running.copied, running.fraction], ["running", 50, 0.25]);
  assert.equal(taskProgress({ completed: false, task: { status: { total: 0 } } }).fraction, null);
  const done = taskProgress({ completed: true, response: { total: 3, created: 3, failures: [] } });
  assert.deepEqual([done.state, done.fraction], ["done", 1]);
  const failed = taskProgress({
    completed: true,
    error: { type: "status_exception", reason: "body={...}", caused_by: { reason: "method [GET], host [https://es.example:443], URI [/], status line [HTTP/1.1 401 Unauthorized]" } },
  });
  assert.equal(failed.state, "failed");
  assert.match(failed.error ?? "", /^status_exception: .*401 Unauthorized/);
  const docFailed = taskProgress({ completed: true, response: { total: 2, created: 1, failures: [{ cause: { type: "mapper_parsing_exception", reason: "failed to parse field [n]" } }] } });
  assert.deepEqual([docFailed.state, docFailed.error], ["failed", "mapper_parsing_exception: failed to parse field [n]"]);
  assert.equal(taskProgress({ completed: true, response: { total: 9, created: 2, canceled: "by user request", failures: [] } }).state, "cancelled");
});

test("an alias is offered under the remote's name when the copy got another one", () => {
  const roll = new Set(["products-migrated"]);
  assert.equal(aliasOffer("products", "products-migrated", roll), "products");
  assert.equal(aliasOffer("products", "products", roll), null);
  assert.equal(aliasOffer("products", "x", new Set(["products"])), null);
  assert.deepEqual(
    droppedByReason([
      { setting: "index.uuid", value: "u", reason: "set by the cluster that made the index" },
      { setting: "index.blocks.write", value: "true", reason: "a block would stop the copy" },
      { setting: "index.creation_date", value: "1", reason: "set by the cluster that made the index" },
    ]),
    [
      { reason: "set by the cluster that made the index", settings: ["index.uuid", "index.creation_date"] },
      { reason: "a block would stop the copy", settings: ["index.blocks.write"] },
    ],
  );
});

test("a task _tasks no longer knows is lost, and its partial copy can go", () => {
  assert.deepEqual(taskPoll(404, '{"error":{"type":"resource_not_found_exception"}}'), { kind: "lost" });
  assert.deepEqual(taskPoll(502, "bad gateway"), { kind: "retry" });
  assert.deepEqual(taskPoll(200, "not json"), { kind: "retry" });
  const p = taskPoll(200, '{"completed":false,"task":{"status":{"total":10,"created":4}}}');
  assert.equal(p.kind, "task");
  if (p.kind === "task") assert.equal(taskProgress(p.body).copied, 4);
  assert.ok(partial("lost") && partial("failed") && partial("cancelled"));
  assert.ok(!partial("done") && !partial("running"));
});

test("copies past the free task slots wait, and start as earlier ones end", () => {
  const rows: MigrateStarted[] = [
    { index: "a", dest: "a", task: "n:1" },
    { index: "b", dest: "b", task: "n:2" },
    { index: "c", dest: "c", queued: true },
    { index: "d", dest: "d-copy", queued: true },
  ];
  assert.deepEqual(queuedPicks(rows), [{ index: "c", dest: "c" }, { index: "d", dest: "d-copy" }]);
  // Both slots taken (a task whose state isn't known yet counts as running).
  assert.equal(shouldStartQueued(rows, { "n:1": "running" }), false);
  // One ended: ask again.
  assert.equal(shouldStartQueued(rows, { "n:1": "done", "n:2": "running" }), true);
  const merged = mergeStarted(rows, [{ index: "c", dest: "c", task: "n:3" }, { index: "d", dest: "d-copy", queued: true }]);
  assert.deepEqual(merged.map((r) => [r.index, r.task ?? null, !!r.queued]), [["a", "n:1", false], ["b", "n:2", false], ["c", "n:3", false], ["d", null, true]]);
  assert.equal(shouldStartQueued(merged, { "n:1": "done", "n:2": "done", "n:3": "running" }), true);
  assert.equal(shouldStartQueued([{ index: "a", dest: "a", task: "n:1" }], {}), false, "nothing waits");
});

test("a start of waiting copies that fails is retried, with growing pauses, unless roll refused it", () => {
  assert.ok(retryableStart(429) && retryableStart(503) && retryableStart(undefined));
  assert.ok(!retryableStart(400) && !retryableStart(403) && !retryableStart(404));
  assert.deepEqual([0, 1, 2, 3, 4, 20].map(queueRetryDelay), [5_000, 10_000, 20_000, 40_000, 60_000, 60_000]);
});
