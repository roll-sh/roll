import { describe, expect, it } from "vitest";
import {
  describeSchedule,
  recoveryProgress,
  repositoryBody,
  repositoryProblem,
  RESTORE_DEFAULTS,
  restoreProblem,
  restoreRequest,
  slmPolicyBody,
  slmRows,
  snapshotRows,
} from "./search-snapshots";

describe("repositories", () => {
  it("registers with only the settings that were given or differ from the default", () => {
    expect(repositoryBody("s3", { bucket: " backups ", base_path: "", compress: true, readonly: true })).toEqual({ type: "s3", settings: { bucket: "backups", readonly: true } });
    expect(repositoryProblem("nightly", "s3", { bucket: "" }, [])).toBe("Bucket is required.");
    expect(repositoryProblem("nightly", "fs", { location: "b" }, ["nightly"])).toBe("nightly is registered already.");
  });
});

describe("snapshots", () => {
  it("lists every repository's snapshots newest first, with their size and failures", () => {
    const rows = snapshotRows({
      r1: [
        { snapshot: "old", uuid: "a", state: "SUCCESS", start_time_in_millis: 1, duration_in_millis: 5 },
        {
          snapshot: "new",
          uuid: "b",
          state: "PARTIAL",
          start_time_in_millis: 9,
          duration_in_millis: 200,
          index_details: { a: { size_in_bytes: 100 }, b: { size_in_bytes: 23 } },
          failures: [{ index: "b", shard_id: 0, reason: "IndexShardSnapshotFailedException" }],
          metadata: { policy: "nightly" },
        },
      ],
    });
    expect(rows.map((r) => [r.name, r.sizeBytes, r.policy])).toEqual([
      ["new", 123, "nightly"],
      ["old", null, null],
    ]);
    expect(rows[0].failures).toEqual(["b[0]: IndexShardSnapshotFailedException"]);
  });
});

describe("restore", () => {
  it("sends only what differs from the defaults", () => {
    const r = restoreRequest("r1", "s 1", { indices: ["logs-a", "logs-b"], rename: { pattern: "(.+)", replacement: "restored-$1" }, includeAliases: false, includeGlobalState: false, partial: true });
    expect(r).toEqual({
      path: "/_snapshot/r1/s%201/_restore",
      body: { indices: "logs-a,logs-b", rename_pattern: "(.+)", rename_replacement: "restored-$1", include_aliases: false, partial: true },
    });
    expect(restoreRequest("r1", "s1", RESTORE_DEFAULTS).body).toEqual({});
  });

  it("refuses names that are open, unless the rename moves them", () => {
    expect(restoreProblem(RESTORE_DEFAULTS, ["logs-a", ".kibana"], ["logs-a"])).toMatch(/^logs-a is open/);
    expect(restoreProblem({ ...RESTORE_DEFAULTS, rename: { pattern: "(.+)", replacement: "restored-$1" } }, ["logs-a"], ["logs-a"])).toBeNull();
  });

  it("follows the restored shards' bytes until every one is done", () => {
    const shard = (stage: string, got: number) => ({ stage, index: { size: { total_in_bytes: 100, recovered_in_bytes: got } } });
    expect(recoveryProgress({ a: { shards: [shard("INDEX", 50), shard("DONE", 100)] }, b: { shards: [shard("DONE", 100)] } })).toEqual([
      { index: "a", percent: 75, done: false },
      { index: "b", percent: 100, done: true },
    ]);
  });
});

describe("snapshot lifecycle policies", () => {
  it("says a schedule in words and keeps what it can't", () => {
    expect(describeSchedule("0 30 1 * * ?")).toBe("Daily at 01:30 UTC");
    expect(describeSchedule("0 0 2 ? * SUN")).toBe("Sundays at 02:00 UTC");
    expect(describeSchedule("0 0/30 * * * ?")).toBe("Every 30 minutes");
    expect(describeSchedule("30m")).toBe("Every 30 minutes");
    expect(describeSchedule("0 0 1-5 * * ?")).toBe("0 0 1-5 * * ?");
  });

  it("marks a policy failing while its last failure is newer than its last success", () => {
    const policy = { name: "<n-{now/d}>", schedule: "1h", repository: "r1" };
    const rows = slmRows({
      ok: { policy, last_success: { snapshot_name: "a", time: 5 }, last_failure: { snapshot_name: "b", time: 3 } },
      bad: { policy, last_success: { snapshot_name: "a", time: 5 }, last_failure: { snapshot_name: "b", time: 7 } },
    });
    expect(rows.map((r) => [r.id, r.failing])).toEqual([
      ["bad", true],
      ["ok", false],
    ]);
  });

  it("builds a policy with retention only when one is set", () => {
    const f = { id: "nightly", name: "<nightly-{now/d}>", schedule: "0 30 1 * * ?", repository: "r1", indices: "logs-*, metrics-*", includeGlobalState: false, expireAfter: "", minCount: "", maxCount: "" };
    expect(slmPolicyBody(f)).toEqual({ schedule: "0 30 1 * * ?", name: "<nightly-{now/d}>", repository: "r1", config: { indices: ["logs-*", "metrics-*"], include_global_state: false } });
    expect(slmPolicyBody({ ...f, expireAfter: "30d", maxCount: "50" }).retention).toEqual({ expire_after: "30d", max_count: 50 });
  });
});
