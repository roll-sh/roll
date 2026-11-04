import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import type { MenuItemConstructorOptions } from "electron";
import { lineReader, parseLoginLine, runLogin, type LoginPrompt } from "./login.ts";
import { appMenu, trayMenu, type MenuActions } from "./menu.ts";
import { activityLine, Announcer, noticeFor, parseDaemonLine, parseNotice, parseStatus, trayModel, type TrayInput } from "./status.ts";
import { FileLog } from "./supervisor.ts";

const FAKE = new URL("../../test/fake-roll.mjs", import.meta.url).pathname;

describe("status from the page", () => {
  it("parses and bounds what the page sends", () => {
    const s = parseStatus({
      running: 2.7,
      pending: [
        { thread: "t1", title: "Fix build", request: "r1", kind: "approval", summary: "cargo test" },
        { thread: "t2", title: "x".repeat(1000), request: "r2", kind: "question" },
        { thread: "", request: "r3" },
        { thread: "t4", request: "y".repeat(500) },
        "nope",
      ],
      machines: [{ id: "m1", online: true }, { id: "m2", online: "yes" }, {}],
    });
    expect(s).toEqual({
      running: 2,
      pending: [
        { thread: "t1", title: "Fix build", request: "r1", kind: "approval", summary: "cargo test" },
        { thread: "t2", title: "x".repeat(200), request: "r2", kind: "question" },
      ],
      machines: [
        { id: "m1", online: true },
        { id: "m2", online: false },
      ],
    });
    expect(parseStatus(null)).toBeNull();
    expect(parseStatus({ running: -3, pending: Array.from({ length: 1000 }, (_, i) => ({ thread: "t", request: `r${i}` })) })?.pending).toHaveLength(200);
    expect(parseNotice({ title: "Done", body: "ok", thread: "t1", extra: 1 })).toEqual({ title: "Done", body: "ok", thread: "t1" });
    expect(parseNotice({ body: "no title" })).toBeNull();
  });

  it("announces each pending request once, and not the backlog at launch", () => {
    const a = new Announcer();
    const p = (request: string) => ({ thread: "t", title: "T", request, kind: "approval" as const });
    expect(a.fresh({ running: 1, pending: [p("r1")] })).toEqual([]);
    expect(a.fresh({ running: 1, pending: [p("r1"), p("r2")] })).toEqual([p("r2")]);
    expect(a.fresh({ running: 1, pending: [p("r1"), p("r2")] })).toEqual([]);
    expect(a.fresh({ running: 1, pending: [p("r2")] })).toEqual([]);
    expect(a.fresh({ running: 1, pending: [p("r2"), p("r1")] })).toEqual([p("r1")]);
  });

  it("words notifications by kind and links them to the thread", () => {
    expect(noticeFor({ thread: "t1", title: "Fix build", request: "r", kind: "approval", summary: "cargo test" })).toEqual({
      title: "Approval needed",
      body: "Fix build: cargo test",
      thread: "t1",
    });
    expect(noticeFor({ thread: "t1", title: "", request: "r", kind: "question" })).toEqual({ title: "A question is waiting", body: "An agent", thread: "t1" });
  });
});

describe("tray", () => {
  const base: TrayInput = {
    daemon: { kind: "running", pid: 1, since: 0 },
    link: "connecting",
    missingBinary: false,
    external: false,
    login: { kind: "signed_in" },
    paused: false,
    status: { running: 3, pending: [{ thread: "t", title: "T", request: "r", kind: "approval" }], machines: [{ id: "m1", online: true }] },
    machineId: "m1",
    now: 0,
  };

  it("says connected, with what is running and waiting", () => {
    const m = trayModel(base);
    expect(m).toMatchObject({ machine: "Connected", running: 3, pending: 1, canPause: true, canSignIn: false });
    expect(m.tooltip).toBe("roll — Connected · 3 running · 1 waiting");
    expect(activityLine(m)).toBe("3 agents running · 1 waiting for you");
  });

  it("covers every machine state", () => {
    const line = (i: Partial<TrayInput>) => trayModel({ ...base, ...i }).machine;
    expect(line({ status: { running: 0, pending: [], machines: [{ id: "m1", online: false }] } })).toBe("Connecting…");
    expect(line({ status: null, link: "connected" })).toBe("Connected");
    expect(line({ link: "reconnecting" })).toBe("Offline · reconnecting");
    expect(line({ daemon: { kind: "restarting", attempt: 2, at: 4200, lastExit: "x" } })).toBe("Offline · retrying in 5 s");
    expect(line({ daemon: { kind: "stopped" } })).toBe("Offline");
    expect(line({ paused: true })).toBe("Paused");
    expect(line({ login: { kind: "signed_out" } })).toBe("Not signed in");
    expect(line({ login: { kind: "signing_in", code: null } })).toBe("Signing in…");
    expect(line({ login: { kind: "signing_in", code: "ABCD-EFGH" } })).toBe("Waiting for approval · ABCD-EFGH");
    expect(line({ missingBinary: true })).toBe("roll CLI not found");
    expect(line({ external: true })).toBe("Connected");
    expect(line({ external: true, machineId: null })).toBe("Managed outside roll");
    expect(trayModel({ ...base, login: { kind: "signed_out" } })).toMatchObject({ canPause: false, canSignIn: true });
    expect(trayModel({ ...base, status: null }).tooltip).toBe("roll — Connecting…");
  });

  const clicks: string[] = [];
  const actions = new Proxy({} as MenuActions, {
    get: (_t, k: string) => (k === "checkForUpdates" ? null : () => clicks.push(k)),
  });
  const labels = (items: MenuItemConstructorOptions[]) => items.map((i) => i.label ?? i.role ?? i.type);
  const click = (items: MenuItemConstructorOptions[], label: string) => {
    const item = items.find((i) => i.label === label);
    if (!item) throw new Error(`no ${label}`);
    (item.click as () => void)();
  };

  it("builds the tray menu: status, open agents or roll, pause or resume, quit", () => {
    const m = trayModel(base);
    const items = trayMenu(m, actions);
    expect(labels(items)).toEqual([
      "Connected",
      "3 agents running · 1 waiting for you",
      "separator",
      "Open agents",
      "Open roll",
      "Pause agents",
      "separator",
      "Quit roll",
    ]);
    click(items, "Pause agents");
    click(items, "Open agents");
    click(items, "Open roll");
    expect(labels(trayMenu(trayModel({ ...base, paused: true }), actions))).toContain("Resume agents");
    expect(labels(trayMenu(trayModel({ ...base, login: { kind: "signed_out" } }), actions))).toContain("Connect this machine…");
    expect(clicks).toEqual(["pause", "openAgents", "openRoll"]);
  });

  it("builds the app menu with the usual roles and shortcuts", () => {
    const mac = appMenu("darwin", trayModel(base), actions);
    expect(labels(mac)).toEqual(["roll", "File", "editMenu", "View", "Go", "Agents", "windowMenu", "help"]);
    const linux = appMenu("linux", trayModel(base), actions);
    expect(labels(linux)).toEqual(["File", "editMenu", "View", "Go", "Agents", "windowMenu", "help"]);
    const file = linux[0].submenu as MenuItemConstructorOptions[];
    expect(file.find((i) => i.label === "Quit")?.accelerator).toBe("Ctrl+Q");
    const agents = linux.find((i) => i.label === "Agents")?.submenu as MenuItemConstructorOptions[];
    expect(agents.slice(0, 2).map((i) => [i.label, i.accelerator])).toEqual([
      ["Open roll", "CmdOrCtrl+1"],
      ["Open agents", "CmdOrCtrl+2"],
    ]);
    clicks.length = 0;
    click(agents, "Open agents");
    click(agents, "Open roll");
    expect(clicks).toEqual(["openAgents", "openRoll"]);
  });
});

describe("daemon output", () => {
  it("tracks the link from what roll agent up prints", () => {
    expect(parseDaemonLine("roll agent: connected to roll as acme/mch_1")).toBe("connected");
    expect(parseDaemonLine("roll agent: connection refused; retrying in 2.0 s")).toBe("reconnecting");
    expect(parseDaemonLine("roll agent: laptop (mch_1) for acme, data in /home/ada/.local/share/roll/agent")).toBeNull();
    expect(parseDaemonLine("some provider output; retrying in 2.0 s")).toBeNull();
  });
});

describe("first-run login", () => {
  it("finds the prompt in JSON or plain output", () => {
    const url = "https://roll.sh/dashboard/agents/connect?code=ABCD-EFGH";
    expect(parseLoginLine(JSON.stringify({ device_code: "d", user_code: "ABCD-EFGH", verification_uri: url, interval: 5, expires_in: 900 }))).toEqual({ code: "ABCD-EFGH", url });
    expect(parseLoginLine(`  ${url}`)).toEqual({ code: "ABCD-EFGH", url });
    expect(parseLoginLine("Starting device login…")).toBeNull();
    expect(parseLoginLine("https://roll.sh/elsewhere?code=X")).toBeNull();
  });

  it("splits lines across chunks and drops overlong ones", () => {
    const lines: string[] = [];
    const feed = lineReader((l) => lines.push(l));
    feed("one\ntw");
    feed("o\n");
    feed("x".repeat(70_000));
    feed("still too long\nthree\n");
    expect(lines).toEqual(["one", "two", "three"]);
  });

  it("runs `roll agent login`, shows the code and finishes when it is approved", async () => {
    const dir = mkdtempSync(join(tmpdir(), "roll-desktop-login-"));
    try {
      const agentJson = join(dir, "agent.json");
      const prompts: LoginPrompt[] = [];
      const log = new FileLog(join(dir, "agent.log"), "agent");
      const login = runLogin({
        command: process.execPath,
        args: [FAKE, "agent", "login", "--json", "--api", "https://api.roll.sh"],
        env: { ...process.env, FAKE_ROLL_LOGIN: "approve", FAKE_ROLL_AGENT_JSON: agentJson, FAKE_ROLL_LOG: join(dir, "calls") },
        log,
        onPrompt: (p) => prompts.push(p),
      });
      expect(await login.done).toEqual({ ok: true });
      expect(prompts).toEqual([{ code: "ABCD-EFGH", url: "https://roll.sh/dashboard/agents/connect?code=ABCD-EFGH" }]);
      expect(existsSync(agentJson)).toBe(true);
      expect(readFileSync(join(dir, "calls"), "utf8")).toMatch(/ agent login --json --api https:\/\/api\.roll\.sh\n$/);
      expect(readFileSync(log.file, "utf8")).toContain('"status":"approved"');
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("reads the prompt from the CLI's text output too", async () => {
    const dir = mkdtempSync(join(tmpdir(), "roll-desktop-login-"));
    try {
      const prompts: LoginPrompt[] = [];
      const login = runLogin({
        command: process.execPath,
        args: [FAKE, "agent", "login"],
        env: { ...process.env, FAKE_ROLL_LOGIN: "approve", FAKE_ROLL_AGENT_JSON: join(dir, "agent.json") },
        log: new FileLog(join(dir, "agent.log"), "agent"),
        onPrompt: (p) => prompts.push(p),
      });
      expect(await login.done).toEqual({ ok: true });
      expect(prompts).toEqual([{ code: "ABCD-EFGH", url: "https://roll.sh/dashboard/agents/connect?code=ABCD-EFGH" }]);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("reports a denied login", async () => {
    const dir = mkdtempSync(join(tmpdir(), "roll-desktop-login-"));
    try {
      const login = runLogin({
        command: process.execPath,
        args: [FAKE, "agent", "login"],
        env: { ...process.env, FAKE_ROLL_LOGIN: "deny" },
        log: new FileLog(join(dir, "agent.log"), "agent"),
        onPrompt: () => {},
      });
      expect(await login.done).toEqual({ ok: false, reason: "exited with code 1" });
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
