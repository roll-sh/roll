// Tests of the dashboard's migration from Elasticsearch or OpenSearch (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import { aliasOffer, defaultDest, destProblem, displayHost, droppedByReason, sourceFrom, sourceProblem, taskProgress, type SourceForm } from "./search-migrate";

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
