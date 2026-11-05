import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import {
  coming,
  commandD,
  commandRequestD,
  createAgentsApi,
  daemonFrameD,
  deltaD,
  deviceLoginD,
  entityD,
  loginPollD,
  machineD,
  machineLoginD,
  machineState,
  pendingLoginD,
  reachable,
  shareD,
  shareSummaryD,
  relayFrameD,
  replyD,
  stateD,
  streamEventD,
  type Command,
  type CommandType,
  type Entity,
  type Machine,
  type MachineState,
  type SandboxStatus,
  type StreamEvent,
} from "./agents-api";
import { httpTransport } from "./agents-client";
import { decode, type Decoder } from "./decode";

const DIR = join(__dirname, "..", "..", "..", "..", "crates", "agent-proto", "fixtures");

/** Every fixture file and the decoder for its items. A new file fails until it's listed here. */
const decoders: Record<string, Decoder<unknown>> = {
  "command_requests.json": commandRequestD,
  "commands.json": commandD,
  "daemon_frames.json": daemonFrameD,
  "deltas.json": deltaD,
  "device_logins.json": deviceLoginD,
  "entities.json": entityD,
  "login_polls.json": loginPollD,
  "machine_logins.json": machineLoginD,
  "pending_logins.json": pendingLoginD,
  "share_summaries.json": shareSummaryD,
  "shares.json": shareD,
  "relay_frames.json": relayFrameD,
  "replies.json": replyD,
  "state.json": stateD,
  "stream.json": streamEventD,
};

const load = (file: string): unknown[] => JSON.parse(readFileSync(join(DIR, file), "utf8"));

/** Drop what Rust skips when empty, so a decoded record compares with what Rust wrote. */
function prune(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(prune);
  if (v && typeof v === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, x] of Object.entries(v)) {
      if (x === null || x === undefined || (Array.isArray(x) && x.length === 0)) continue;
      out[k] = prune(x);
    }
    return out;
  }
  return v;
}

describe("agent-proto fixtures", () => {
  it("every fixture file has a decoder", () => {
    // automation*.json are automations-api.test.ts's.
    const files = readdirSync(DIR).filter((f) => f.endsWith(".json") && !f.startsWith("automation"));
    expect(files.sort()).toEqual(Object.keys(decoders).sort());
  });

  for (const [file, d] of Object.entries(decoders)) {
    it(`${file} decodes strictly and keeps every field`, () => {
      for (const item of load(file)) {
        const decoded = decode(d, item, { strict: true });
        expect(prune(decoded)).toEqual(prune(item));
      }
    });
  }

  it("commands serialize exactly as Rust writes them", () => {
    for (const item of load("commands.json")) {
      expect(JSON.parse(JSON.stringify(decode(commandD, item, { strict: true })))).toEqual(item);
    }
  });

  it("cover every command and entity kind the dashboard knows", () => {
    const commands = new Set(load("commands.json").map((c) => (c as Command).type));
    const all: Record<CommandType, true> = {
      "project.add": true,
      "project.remove": true,
      "project.update": true,
      "project.pull": true,
      "thread.create": true,
      "thread.configure": true,
      "thread.fork": true,
      "thread.archive": true,
      "thread.unarchive": true,
      "thread.delete": true,
      "thread.settle": true,
      "thread.unsettle": true,
      "thread.snooze": true,
      "thread.unsnooze": true,
      "thread.visit": true,
      "thread.auto_settle": true,
      "thread.change_ended": true,
      "message.send": true,
      "message.unqueue": true,
      "message.edit": true,
      "message.reorder": true,
      "message.promote": true,
      "run.interrupt": true,
      "request.respond": true,
      "checkpoint.rollback": true,
      "thread.publish": true,
      "terminal.open": true,
      "terminal.input": true,
      "terminal.resize": true,
      "terminal.close": true,
      "provider.refresh": true,
      "machine.configure": true,
      "diff.get": true,
      "fs.list": true,
      "fs.read": true,
      "fs.write": true,
      "git.status": true,
      "terminal.scrollback": true,
    };
    expect([...commands].sort()).toEqual(Object.keys(all).sort());
    const kinds = new Set(load("entities.json").map((e) => (e as Entity).kind));
    expect([...kinds].sort()).toEqual(["checkpoint", "node", "project", "removed", "request", "run", "terminal", "thread"]);
  });

  it("strict decoding rejects unknown keys and tags; lenient decoding ignores keys", () => {
    const [project] = load("entities.json") as Record<string, unknown>[];
    expect(() => decode(entityD, { ...project, colour: "red" }, { strict: true })).toThrow(/colour: unknown key/);
    expect(decode(entityD, { ...project, colour: "red" })).toMatchObject({ kind: "project", name: "roll" });
    expect(() => decode(entityD, { ...project, kind: "galaxy" })).toThrow(/unknown kind/);
  });

  it("fills serde defaults", () => {
    const thread = (load("entities.json") as Record<string, unknown>[]).find((e) => e.kind === "thread")!;
    const { queued: _q, usage: _u, lineage: _l, ...bare } = thread;
    const t = decode(entityD, bare);
    expect(t).toMatchObject({ queued: [], lineage: null, usage: { input_tokens: 0, cost_usd: null }, archived_at: null });
  });
});

describe("agents api", () => {
  const reply = (status: number, body: unknown) =>
    new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

  it("retries a command with the same id, and returns relay errors as replies", async () => {
    const seen: string[] = [];
    let n = 0;
    const fetcher = vi.fn(async (_url: string | URL | Request, init?: RequestInit) => {
      seen.push(JSON.parse(String(init?.body)).id);
      n++;
      if (n === 1) throw new TypeError("network");
      if (n === 2) return reply(502, { error: "bad gateway" });
      return reply(200, { type: "created", id: "thr_1" });
    });
    const api = createAgentsApi("http://roll.test", "rlc_x", undefined, fetcher as typeof fetch);
    const out = await api.command("acme", "mch_1", { id: "cmd_1", command: { type: "run.interrupt", thread: "thr_1" } });
    expect(out).toEqual({ type: "created", id: "thr_1" });
    expect(seen).toEqual(["cmd_1", "cmd_1", "cmd_1"]);
    const [url, init] = fetcher.mock.calls[0];
    expect(url).toBe("http://roll.test/v1/code/agents/acme/machines/mch_1/commands");
    expect((init?.headers as Record<string, string>).authorization).toBe("Bearer rlc_x");

    const offline = createAgentsApi("http://roll.test", undefined, undefined, (async () =>
      reply(503, { type: "error", code: "machine_offline", message: "not connected" })) as typeof fetch);
    expect(await offline.command("acme", "mch_1", { id: "cmd_2", command: { type: "provider.refresh" } })).toEqual({
      type: "error",
      code: "machine_offline",
      message: "not connected",
    });
  });

  it("asks for the detail of the threads it follows", () => {
    const api = createAgentsApi("http://roll.test");
    expect(api.eventsUrl("acme", ["thr_a", "thr_b"], 42)).toBe("http://roll.test/v1/code/agents/acme/events?threads=thr_a,thr_b&after=42");
    expect(api.eventsUrl("acme")).toBe("http://roll.test/v1/code/agents/acme/events");
  });

  it("approves and denies device logins for an owner", async () => {
    const calls: [string, unknown][] = [];
    const api = createAgentsApi("http://roll.test", undefined, undefined, (async (url: string, init?: RequestInit) => {
      calls.push([url, init?.body ? JSON.parse(String(init.body)) : null]);
      return new Response(null, { status: 204 });
    }) as typeof fetch);
    await api.approveLogin("acme", "WXYZ-2345", "ada-mbp");
    await api.denyLogin("acme", "BCDF-GHJK");
    expect(calls).toEqual([
      ["http://roll.test/v1/code/agents/acme/login/approve", { user_code: "WXYZ-2345", name: "ada-mbp" }],
      ["http://roll.test/v1/code/agents/acme/login/deny", { user_code: "BCDF-GHJK" }],
    ]);
  });

  it("streams events over HTTP and resumes after the newest cursor", async () => {
    const stream = load("stream.json") as StreamEvent[];
    const urls: string[] = [];
    const lastIds: (string | undefined)[] = [];
    let n = 0;
    const fetcher = (async (url: string, init?: RequestInit) => {
      urls.push(url);
      lastIds.push((init?.headers as Record<string, string>)["last-event-id"]);
      if (n++ > 0) return new Promise<Response>(() => {});
      const body = stream
        .map((e) => `event: ${e.type}\n${"cursor" in e ? `id: ${e.cursor}\n` : ""}data: ${JSON.stringify(e)}\n\n`)
        .concat("event: entity\nid: 99\ndata: {\"type\":\"entity\",\"kind\":\"galaxy\"}\n\n")
        .join("");
      return new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } });
    }) as typeof fetch;
    const api = createAgentsApi("http://roll.test", "rlc_x", undefined, fetcher);
    let cursor = 5;
    const seen: (string | number | null)[] = [];
    const stop = httpTransport("acme", api).stream(
      ["thr_a"],
      () => cursor,
      (ev, id) => {
        seen.push(ev.type, id);
        if (id !== null) cursor = id;
      },
      () => {},
    );
    await vi.waitFor(() => expect(urls).toHaveLength(2), { timeout: 3000 });
    stop();
    // Every known event decoded (the unknown one dropped), ids are cursors, deltas have none.
    expect(seen).toEqual([
      ...["entity", 7, "machine", 8, "machine_removed", 9, "delta", null, "canvas", 10, "settings", 11],
      ...["automation", 12, "automation_removed", 13, "automation_run", 14, "notice", null],
    ]);
    expect(urls[0]).toBe("http://roll.test/v1/code/agents/acme/events?threads=thr_a&after=5");
    expect(urls[1]).toBe("http://roll.test/v1/code/agents/acme/events?threads=thr_a&after=14");
    expect(lastIds).toEqual(["5", "14"]);
  });
});

describe("machineState", () => {
  const m = (online: boolean, sandbox: Machine["sandbox"]): Machine => ({ id: "mch_1", name: "box", online, info: null, sandbox, appearance: {}, created_at: 0, last_seen: null });
  const sb = (status: SandboxStatus, stopped_reason?: "idle" | "owner") => ({ backend: "agent37", size: "standard", status, hourly_cents: 3, ...(stopped_reason ? { stopped_reason } : {}) });

  it("never calls a sandbox that wakes on its own offline", () => {
    expect(machineState(m(true, sb("running")))).toBe("online");
    expect(machineState(m(false, null))).toBe("offline");
    expect(machineState(m(false, sb("stopped", "idle")))).toBe("asleep");
    expect(machineState(m(false, sb("stopped", "owner")))).toBe("stopped");
    expect(machineState(m(false, sb("starting")))).toBe("waking");
    expect(machineState(m(false, sb("creating")))).toBe("booting");
    expect(machineState(m(false, sb("running")))).toBe("connecting");
    expect(machineState(m(false, sb("failed")))).toBe("offline");
    expect(machineState(m(false, sb("deleting")))).toBe("offline");
    // A command reaches it now or once it has woken; a deleted or failed one, or an offline computer, never.
    expect([sb("stopped", "idle"), sb("stopped", "owner"), sb("starting")].every((s) => reachable(m(false, s)))).toBe(true);
    expect([reachable(m(false, null)), reachable(m(false, sb("deleting"))), reachable(undefined)]).toEqual([false, false, false]);
    expect(["waking", "booting", "connecting"].every((s) => coming(s as MachineState))).toBe(true);
    expect(coming("asleep")).toBe(false);
  });
});

describe("identity on the wire", () => {
  const p = { kind: "project", id: "prj_1", name: "roll", path: "/r", created_at: 0, updated_at: 0 };
  it("a colour or icon from a newer daemon shows the default instead of dropping the record", () => {
    const e = decode(entityD, { ...p, settings: { auto_pull: false, color: "chartreuse", icon: { kind: "lucide", name: "rocket" } } });
    expect(e.kind === "project" && [e.settings.color, e.settings.icon]).toEqual([undefined, undefined]);
    const m = decode(machineD, { id: "mch_1", name: "b", online: true, created_at: 0, appearance: { color: "teal", kind: "toaster" } });
    expect(m.appearance).toEqual({ color: "teal", kind: undefined });
  });
  it("a logo is only ever a data:image URL", () => {
    const logo = (url: string) => {
      const e = decode(entityD, { ...p, logo: { path: "x.svg", url } });
      return e.kind === "project" ? e.logo : undefined;
    };
    expect(logo("data:image/png;base64,iVBORw0KGgo=")).toEqual({ path: "x.svg", url: "data:image/png;base64,iVBORw0KGgo=" });
    expect(logo("https://evil.example/x.svg")).toBeNull();
    expect(logo("javascript:alert(1)")).toBeNull();
    expect(logo("data:text/html;base64,PGgxPg==")).toBeNull();
  });
});
