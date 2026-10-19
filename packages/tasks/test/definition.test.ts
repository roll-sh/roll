import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { VERSION, createFunction, parseDuration, toFunctionDefinition, toUnixMs } from "../src/index.js";

describe("durations", () => {
  it("parses numbers and strings", () => {
    assert.equal(parseDuration(0), 0);
    assert.equal(parseDuration(1500), 1500);
    assert.equal(parseDuration(1.4), 1);
    assert.equal(parseDuration("1500"), 1500);
    assert.equal(parseDuration("500ms"), 500);
    assert.equal(parseDuration("30s"), 30_000);
    assert.equal(parseDuration("1.5s"), 1500);
    assert.equal(parseDuration("5m"), 300_000);
    assert.equal(parseDuration("2h"), 7_200_000);
    assert.equal(parseDuration("7d"), 604_800_000);
    assert.equal(parseDuration("1w"), 604_800_000);
    assert.equal(parseDuration("1h 30m"), 5_400_000);
    assert.equal(parseDuration("1h30m"), 5_400_000);
    assert.equal(parseDuration(" 10 seconds "), 10_000);
    assert.equal(parseDuration("2 minutes"), 120_000);
    assert.equal(parseDuration("3D"), 3 * 86_400_000);
  });

  it("rejects nonsense", () => {
    for (const bad of ["", "soon", "5x", "m5", "-5s", "5s and", NaN, -1, Infinity]) {
      assert.throws(() => parseDuration(bad as never), TypeError, String(bad));
    }
  });

  it("converts times", () => {
    assert.equal(toUnixMs(new Date(1_700_000_000_000)), 1_700_000_000_000);
    assert.equal(toUnixMs(42), 42);
    assert.equal(toUnixMs("2026-01-01T00:00:00Z"), Date.UTC(2026, 0, 1));
    assert.throws(() => toUnixMs("tomorrow"), TypeError);
  });
});

describe("function definitions", () => {
  it("converts backoff and rejects bad ones", () => {
    const def = toFunctionDefinition({ id: "b", backoff: { strategy: "fixed", base: "500ms", max: "1s", jitter: 0 } });
    assert.deepEqual(def.backoff, { strategy: "fixed", base_ms: 500, max_ms: 1000, jitter: 0 });
    assert.deepEqual(toFunctionDefinition({ id: "d", backoff: {} }).backoff, { strategy: "exponential", base_ms: 2000, max_ms: 3_600_000 });
    assert.throws(() => toFunctionDefinition({ id: "x", backoff: { base: "2s", max: "1s" } }), TypeError);
    assert.throws(() => toFunctionDefinition({ id: "x", backoff: { jitter: 2 } }), TypeError);
  });

  it("converts a full config to the wire format", () => {
    const def = toFunctionDefinition({
      id: "checkout",
      name: "Checkout",
      triggers: [{ event: "order.created", if: "event.data.total > 10" }, { cron: "0 9 * * *", tz: "Europe/Budapest" }],
      retries: 5,
      concurrency: [2, { limit: 1, key: "event.data.user" }],
      rateLimit: { limit: 10, period: "1m", key: "event.data.user" },
      throttle: { limit: 2, period: "1s" },
      debounce: { period: "5s", key: "event.data.user", timeout: "1m" },
      batch: { maxSize: 100, timeout: "2s", key: "event.data.shop" },
      idempotency: "event.data.order",
      cancelOn: [{ event: "order.canceled", match: "data.order", if: "async.data.reason != 'x'", timeout: "1d" }],
      priority: { run: "event.data.vip ? 60 : 0" },
      timeouts: { start: "10m", finish: "1h" },
      singleton: { mode: "cancel", key: "event.data.user" },
    });
    assert.deepEqual(def, {
      id: "checkout",
      name: "Checkout",
      triggers: [{ event: "order.created", if: "event.data.total > 10" }, { cron: "0 9 * * *", tz: "Europe/Budapest" }],
      retries: 5,
      concurrency: [{ limit: 2 }, { limit: 1, key: "event.data.user" }],
      rate_limit: { limit: 10, period_ms: 60_000, key: "event.data.user" },
      throttle: { limit: 2, period_ms: 1000 },
      debounce: { period_ms: 5000, key: "event.data.user", timeout_ms: 60_000 },
      batch: { max_size: 100, timeout_ms: 2000, key: "event.data.shop" },
      idempotency: "event.data.order",
      cancel_on: [{ event: "order.canceled", match: "data.order", if: "async.data.reason != 'x'", timeout_ms: 86_400_000 }],
      priority: "event.data.vip ? 60 : 0",
      timeouts: { start_ms: 600_000, finish_ms: 3_600_000 },
      singleton: { mode: "cancel", key: "event.data.user" },
    });
    // Serialisable as is.
    assert.deepEqual(JSON.parse(JSON.stringify(def)), def);
  });

  it("accepts shorthands", () => {
    assert.deepEqual(toFunctionDefinition({ id: "a", trigger: { event: "x" } }), { id: "a", triggers: [{ event: "x" }] });
    assert.deepEqual(toFunctionDefinition({ id: "b", trigger: { cron: "* * * * *" }, concurrency: 3 }), {
      id: "b",
      triggers: [{ cron: "* * * * *" }],
      concurrency: [{ limit: 3 }],
    });
    assert.deepEqual(toFunctionDefinition({ id: "c", concurrency: { limit: 1, key: "event.data.k" }, cancelOn: { event: "stop" } }), {
      id: "c",
      triggers: [],
      concurrency: [{ limit: 1, key: "event.data.k" }],
      cancel_on: [{ event: "stop" }],
    });
    assert.deepEqual(toFunctionDefinition({ id: "d", retries: 0, timeouts: { finish: 500 } }), { id: "d", triggers: [], retries: 0, timeouts: { finish_ms: 500 } });
  });

  it("rejects bad configs", () => {
    assert.throws(() => toFunctionDefinition({ id: "has space" }), TypeError);
    assert.throws(() => toFunctionDefinition({ id: "" }), TypeError);
    assert.throws(() => toFunctionDefinition({ id: "x", triggers: [{ event: "a", cron: "* * * * *" } as never] }), TypeError);
    assert.throws(() => toFunctionDefinition({ id: "x", triggers: [{} as never] }), TypeError);
    assert.throws(() => toFunctionDefinition({ id: "x", concurrency: 0 }), TypeError);
    assert.throws(() => toFunctionDefinition({ id: "x", retries: -1 }), TypeError);
    assert.throws(() => toFunctionDefinition({ id: "x", throttle: { limit: 1, period: "soon" } }), TypeError);
    assert.throws(() => toFunctionDefinition({ id: "x", singleton: { mode: "queue" as never } }), TypeError);
    assert.throws(() => createFunction({ id: "x" }, undefined as never), TypeError);
  });

  it("createFunction carries the definition", () => {
    const f = createFunction({ id: "hello", trigger: { event: "hi" } }, () => "ok");
    assert.equal(f.id, "hello");
    assert.deepEqual(f.definition, { id: "hello", triggers: [{ event: "hi" }] });
    assert.ok(Object.isFrozen(f));
  });

  it("VERSION matches package.json", () => {
    const pkg = JSON.parse(readFileSync(new URL("../../package.json", import.meta.url), "utf8")) as { version: string };
    assert.equal(VERSION, pkg.version);
  });
});
