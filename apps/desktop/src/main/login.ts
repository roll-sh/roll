/**
 * First run: `roll agent login --api <api>` starts a device login and prints where to approve
 * it; the shell shows that page in its own window (the user is signed in there, so approving is
 * one click) and waits for the CLI to receive the machine token and exit.
 */
import type { ChildProcess } from "node:child_process";
import { spawn as nodeSpawn } from "node:child_process";
import type { Log, Spawn } from "./supervisor.ts";

export interface LoginPrompt {
  /** The short code the user confirms (`ABCD-EFGH`). */
  code: string;
  /** The approve page, with the code filled in. */
  url: string;
}

export type LoginResult = { ok: true } | { ok: false; reason: string };

const CONNECT_URL = /https?:\/\/[^\s"'<>]+\/dashboard\/agents\/connect\?code=([A-Za-z0-9-]{1,32})/;
const MAX_LINE = 64 * 1024;

/**
 * Finds the prompt in one line of the CLI's output: a JSON line with `verification_uri` and
 * `user_code` (agent_proto::DeviceLogin), or any line holding the approve URL.
 */
export function parseLoginLine(line: string): LoginPrompt | null {
  const t = line.trim();
  if (t.startsWith("{")) {
    try {
      const o = JSON.parse(t) as Record<string, unknown>;
      if (typeof o.verification_uri === "string" && typeof o.user_code === "string") {
        const m = CONNECT_URL.exec(o.verification_uri);
        if (m) return { code: o.user_code, url: m[0] };
      }
    } catch {
      // Not JSON after all; try the URL below.
    }
  }
  const m = CONNECT_URL.exec(t);
  return m ? { code: m[1], url: m[0] } : null;
}

export interface LoginOptions {
  command: string;
  args: readonly string[];
  env?: NodeJS.ProcessEnv;
  log: Log;
  onPrompt: (prompt: LoginPrompt) => void;
  /** The codes live 15 minutes; give up a little after. */
  timeoutMs?: number;
  spawn?: Spawn;
}

export interface Login {
  done: Promise<LoginResult>;
  cancel(): void;
}

export function runLogin(o: LoginOptions): Login {
  const spawn = o.spawn ?? nodeSpawn;
  let child: ChildProcess;
  try {
    child = spawn(o.command, o.args, { env: o.env, stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
  } catch (e) {
    return { done: Promise.resolve({ ok: false, reason: (e as Error).message }), cancel() {} };
  }
  o.log.event(`starting ${o.command} ${o.args.join(" ")}`);
  let prompted = false;
  const feed = lineReader((line) => {
    if (prompted) return;
    const p = parseLoginLine(line);
    if (p) {
      prompted = true;
      o.onPrompt(p);
    }
  });
  const onData = (b: Buffer) => {
    o.log.output(b);
    feed(b.toString("utf8"));
  };
  child.stdout?.on("data", onData);
  child.stderr?.on("data", onData);
  const done = new Promise<LoginResult>((resolve) => {
    const timer = setTimeout(() => {
      o.log.event("login timed out");
      child.kill("SIGTERM");
    }, o.timeoutMs ?? 16 * 60 * 1000);
    child.on("error", (e) => {
      clearTimeout(timer);
      resolve({ ok: false, reason: e.message });
    });
    child.on("exit", (code, signal) => {
      clearTimeout(timer);
      o.log.event(`login exited ${signal ?? code}`);
      resolve(code === 0 ? { ok: true } : { ok: false, reason: signal ? `killed by ${signal}` : `exited with code ${code}` });
    });
  });
  return { done, cancel: () => child.kill("SIGTERM") };
}

/** Splits a stream of text into lines; a line longer than 64 KiB is dropped, never buffered. */
export function lineReader(onLine: (line: string) => void): (chunk: string) => void {
  let buf = "";
  let skipping = false;
  return (chunk) => {
    let start = 0;
    for (let i = chunk.indexOf("\n"); i !== -1; i = chunk.indexOf("\n", start)) {
      const piece = chunk.slice(start, i);
      if (!skipping && buf.length + piece.length <= MAX_LINE) onLine(buf + piece);
      buf = "";
      skipping = false;
      start = i + 1;
    }
    const rest = chunk.slice(start);
    if (skipping || buf.length + rest.length > MAX_LINE) {
      buf = "";
      skipping = true;
    } else buf += rest;
  };
}
