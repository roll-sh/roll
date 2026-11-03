import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import {
  commandD,
  commandRequestD,
  createAgentsApi,
  daemonFrameD,
  deltaD,
  deviceLoginD,
  entityD,
  loginPollD,
  relayFrameD,
  replyD,
  stateD,
  streamEventD,
  type Command,
  type CommandType,
  type Entity,
} from "./agents-api";
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
    const files = readdirSync(DIR).filter((f) => f.endsWith(".json"));
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
      "thread.create": true,
      "thread.configure": true,
      "thread.fork": true,
      "thread.archive": true,
      "thread.unarchive": true,
      "thread.delete": true,
      "message.send": true,
      "message.unqueue": true,
      "run.interrupt": true,
      "request.respond": true,
      "checkpoint.rollback": true,
      "thread.publish": true,
      "terminal.open": true,
      "terminal.input": true,
      "terminal.resize": true,
      "terminal.close": true,
      "provider.refresh": true,
      "diff.get": true,
      "fs.list": true,
      "fs.read": true,
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
    expect(api.eventsUrl("acme", ["thr_a", "thr_b"])).toBe("http://roll.test/v1/code/agents/acme/events?threads=thr_a,thr_b");
    expect(api.eventsUrl("acme")).toBe("http://roll.test/v1/code/agents/acme/events");
  });
});
