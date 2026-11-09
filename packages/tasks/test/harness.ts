// Integration harness: the real roll-tasks binary on a temp SQLite store, plus a node:http app
// that hosts programmable receivers and a served workflow app.
import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import http from "node:http";
import net, { type AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";

export const repoRoot = path.resolve(process.cwd(), "../..");

export async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.unref();
    s.on("error", reject);
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address() as AddressInfo;
      s.close(() => resolve(port));
    });
  });
}

/** Build (or reuse) the server binary. Returns its path, or the reason it's unavailable. */
export function serverBinary(): { bin?: string; skip?: string } {
  if (process.env.ROLL_TASKS_BIN) return { bin: process.env.ROLL_TASKS_BIN };
  const bin = path.join(repoRoot, "target/debug/roll-tasks");
  const build = spawnSync("cargo", ["build", "-q", "-p", "tasks", "--no-default-features", "--bin", "roll-tasks"], {
    cwd: repoRoot,
    stdio: ["ignore", "inherit", "inherit"],
  });
  if (build.error || build.status !== 0) {
    return existsSync(bin) ? { bin } : { skip: `couldn't build roll-tasks: ${build.error?.message ?? `cargo exited ${build.status}`}` };
  }
  return { bin };
}

export interface Server {
  url: string;
  proc: ChildProcess;
  logs: string[];
  stop(): Promise<void>;
}

export async function startServer(bin: string, extra: string[] = []): Promise<Server> {
  const dir = mkdtempSync(path.join(tmpdir(), "roll-tasks-sdk-"));
  const port = await freePort();
  const logs: string[] = [];
  const proc = spawn(bin, ["--listen", `127.0.0.1:${port}`, "--sqlite", path.join(dir, "t.db"), "--poll-ms", "25", ...extra], {
    env: { ...process.env, RUST_LOG: process.env.RUST_LOG ?? "warn", ROLL_TASKS_TOKEN: "" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  proc.stdout!.on("data", (d) => logs.push(String(d)));
  proc.stderr!.on("data", (d) => logs.push(String(d)));
  const url = `http://127.0.0.1:${port}`;
  const stop = async () => {
    if (proc.exitCode === null && proc.signalCode === null) {
      const exited = new Promise((r) => proc.once("exit", r));
      proc.kill("SIGKILL");
      await exited;
    }
    rmSync(dir, { recursive: true, force: true });
  };
  for (let i = 0; i < 200; i++) {
    if (proc.exitCode !== null) break;
    try {
      const r = await fetch(`${url}/v1/tasks/health`);
      if (r.ok) return { url, proc, logs, stop };
    } catch {
      // Not listening yet.
    }
    await delay(50);
  }
  await stop();
  throw new Error(`roll-tasks didn't start:\n${logs.join("")}`);
}

export interface Delivery {
  name: string;
  method: string;
  headers: http.IncomingHttpHeaders;
  body: Buffer;
  at: number;
}

export type Behaviour = (d: Delivery, n: number) => { status?: number; headers?: Record<string, string>; body?: string } | void;

/** A node:http server: `/recv/<name>` records deliveries; `/app` is handed to `app`. */
export class AppServer {
  readonly deliveries: Delivery[] = [];
  readonly behaviours = new Map<string, Behaviour>();
  app: ((req: http.IncomingMessage, res: http.ServerResponse) => void) | undefined;
  private server = http.createServer((req, res) => this.handle(req, res));
  base = "";

  async start(): Promise<this> {
    await new Promise<void>((r) => this.server.listen(0, "127.0.0.1", r));
    this.base = `http://127.0.0.1:${(this.server.address() as AddressInfo).port}`;
    return this;
  }

  url(name: string) {
    return `${this.base}/recv/${name}`;
  }

  received(name: string) {
    return this.deliveries.filter((d) => d.name === name);
  }

  private async handle(req: http.IncomingMessage, res: http.ServerResponse) {
    const u = new URL(req.url ?? "/", this.base);
    if (u.pathname.startsWith("/app") && this.app) return this.app(req, res);
    if (!u.pathname.startsWith("/recv/")) {
      res.statusCode = 404;
      return res.end();
    }
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    const d: Delivery = { name: u.pathname.slice(6), method: req.method ?? "", headers: req.headers, body: Buffer.concat(chunks), at: Date.now() };
    this.deliveries.push(d);
    const out = this.behaviours.get(d.name)?.(d, this.received(d.name).length) ?? {};
    res.statusCode = out.status ?? 200;
    for (const [k, v] of Object.entries(out.headers ?? {})) res.setHeader(k, v);
    res.end(out.body ?? "ok");
  }

  close() {
    this.server.closeAllConnections?.();
    return new Promise<void>((r) => this.server.close(() => r()));
  }
}

type Truthy<T> = Exclude<T, false | null | undefined | 0 | "">;

/** Poll until `check` is truthy. */
export async function eventually<T>(what: string, check: () => T | Promise<T>, timeoutMs = 8000): Promise<Truthy<T>> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  for (;;) {
    try {
      const v = await check();
      if (v) return v as Truthy<T>;
    } catch (e) {
      last = e;
    }
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}${last ? `: ${String(last)}` : ""}`);
    await delay(40);
  }
}
