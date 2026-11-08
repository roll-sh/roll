import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { FileLog, restartDelay, Supervisor, type DaemonState } from "./supervisor.ts";

const FAKE = new URL("../../test/fake-roll.mjs", import.meta.url).pathname;

async function until(check: () => boolean, ms = 5000): Promise<void> {
  const end = Date.now() + ms;
  while (!check()) {
    if (Date.now() > end) throw new Error("timed out waiting");
    await new Promise((r) => setTimeout(r, 10));
  }
}

const alive = (pid: number) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

describe("restartDelay", () => {
  it("doubles from the initial delay up to the cap", () => {
    const b = { initialMs: 1000, maxMs: 30_000, resetAfterMs: 60_000 };
    expect([0, 1, 2, 3, 4, 5, 6, 50].map((n) => restartDelay(b, n))).toEqual([1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]);
  });
});

describe("Supervisor (with a fake roll binary)", () => {
  let dir: string;
  let sup: Supervisor | null = null;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "roll-desktop-sup-"));
  });
  afterEach(async () => {
    await sup?.stop();
    sup = null;
    rmSync(dir, { recursive: true, force: true });
  });

  const make = (env: Record<string, string>, extra: Partial<ConstructorParameters<typeof Supervisor>[0]> = {}) => {
    const states: { state: DaemonState; t: number }[] = [];
    const log = new FileLog(join(dir, "agent.log"), "agent");
    sup = new Supervisor({
      command: process.execPath,
      args: [FAKE, "agent", "up"],
      env: { ...process.env, FAKE_ROLL_LOG: join(dir, "calls"), ...env },
      log,
      backoff: { initialMs: 40, maxMs: 160, resetAfterMs: 10_000 },
      stopGraceMs: 300,
      onState: (state) => states.push({ state, t: Date.now() }),
      ...extra,
    });
    return { states, log, sup };
  };
  const calls = () => readFileSync(join(dir, "calls"), "utf8").trim().split("\n");

  it("restarts a crashing daemon with doubling, capped delays", async () => {
    // A frozen clock: `at - now` is exactly the delay chosen (a real clock can tick in between).
    const { states, sup } = make({ FAKE_ROLL_EXIT: "1" }, { now: () => 0 });
    sup.start();
    await until(() => states.filter((s) => s.state.kind === "restarting").length >= 5);
    const restarts = states.flatMap(({ state }) => (state.kind === "restarting" ? [{ attempt: state.attempt, delay: state.at, why: state.lastExit }] : []));
    expect(restarts.slice(0, 5).map((r) => r.attempt)).toEqual([1, 2, 3, 4, 5]);
    expect(restarts.slice(0, 5).map((r) => r.delay)).toEqual([40, 80, 160, 160, 160]);
    expect(restarts[0].why).toBe("exited with code 1");
    expect(calls().length).toBeGreaterThanOrEqual(5);
    expect(calls()[0]).toMatch(/ agent up$/);
  });

  it("resets the backoff after a long enough run", async () => {
    const { states, sup } = make({ FAKE_ROLL_EXIT: "1", FAKE_ROLL_RUN_MS: "150" }, { backoff: { initialMs: 40, maxMs: 160, resetAfterMs: 100 } });
    sup.start();
    await until(() => states.filter((s) => s.state.kind === "restarting").length >= 3);
    const attempts = states.flatMap(({ state }) => (state.kind === "restarting" ? [state.attempt] : []));
    expect(attempts.slice(0, 3)).toEqual([1, 1, 1]);
  });

  it("stops cleanly with SIGTERM and logs the daemon's output", async () => {
    const { states, sup, log } = make({});
    sup.start();
    await until(() => sup.state.kind === "running");
    const pid = (sup.state as { pid: number }).pid;
    await until(() => readFileSync(log.file, "utf8").includes("connected to roll as"));
    await sup.stop();
    expect(sup.state.kind).toBe("stopped");
    expect(alive(pid)).toBe(false);
    const text = readFileSync(log.file, "utf8");
    expect(text).toContain("roll agent: connected to roll as acme/m_1");
    expect(text).toContain("roll agent: stopping");
    expect(text).toContain("exited with code 0");
    expect(states.map((s) => s.state.kind)).toEqual(["running", "stopping", "stopped"]);
  });

  it("kills a daemon that ignores SIGTERM after the grace period", async () => {
    const { sup, log } = make({ FAKE_ROLL_IGNORE_TERM: "1" });
    sup.start();
    await until(() => sup.state.kind === "running");
    await until(() => readFileSync(log.file, "utf8").includes("connected to roll"));
    const pid = (sup.state as { pid: number }).pid;
    const t = Date.now();
    await sup.stop();
    expect(Date.now() - t).toBeGreaterThanOrEqual(250);
    expect(alive(pid)).toBe(false);
    expect(readFileSync(log.file, "utf8")).toContain("killed by SIGKILL");
  });

  it("stop cancels a pending restart, and start is idempotent", async () => {
    const { states, sup } = make({ FAKE_ROLL_EXIT: "3" }, { backoff: { initialMs: 5000, maxMs: 5000, resetAfterMs: 10_000 } });
    sup.start();
    sup.start();
    await until(() => sup.state.kind === "restarting");
    await sup.stop();
    expect(sup.state.kind).toBe("stopped");
    await new Promise((r) => setTimeout(r, 100));
    expect(calls()).toHaveLength(1);
    expect(states.at(-1)?.state.kind).toBe("stopped");
  });

  it("reports a missing binary as an exit and keeps retrying", async () => {
    const { states, sup } = make({}, { command: join(dir, "no-such-roll") });
    sup.start();
    await until(() => states.filter((s) => s.state.kind === "restarting").length >= 2);
    const first = states.find((s) => s.state.kind === "restarting")!.state as Extract<DaemonState, { kind: "restarting" }>;
    expect(first.lastExit).toMatch(/ENOENT/);
  });
});

describe("FileLog", () => {
  it("rotates once it passes its size cap", () => {
    const dir = mkdtempSync(join(tmpdir(), "roll-desktop-log-"));
    try {
      const log = new FileLog(join(dir, "a.log"), "t", 100);
      for (let i = 0; i < 10; i++) log.output(`${"x".repeat(30)}\n`);
      log.close();
      expect(readFileSync(join(dir, "a.log"), "utf8").length).toBeLessThanOrEqual(100);
      expect(readFileSync(join(dir, "a.log.1"), "utf8").length).toBeGreaterThan(0);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
