// Integration harness: the real storage-api example server (crates/s3-api) on a temp directory.
import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { createHash, createHmac, randomUUID } from "node:crypto";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { after, before } from "node:test";
import { type RollStorageClient, StorageApiError, createClient } from "../src/index.js";

// This file runs from packages/storage/.test-build/test.
export const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../..");

/** Build (or reuse) the server binary. Returns its path, or the reason it's unavailable. */
export function serverBinary(): { bin?: string; skip?: string } {
  const env = process.env.ROLL_STORAGE_API_BIN;
  if (env) return existsSync(env) ? { bin: env } : { skip: `ROLL_STORAGE_API_BIN=${env} doesn't exist` };
  const bin = path.join(repoRoot, "target/debug/examples/storage-api");
  const build = spawnSync("cargo", ["build", "-q", "-p", "s3-api", "--example", "storage-api"], {
    cwd: repoRoot,
    stdio: ["ignore", "inherit", "inherit"],
  });
  if (build.error || build.status !== 0) {
    return existsSync(bin) ? { bin } : { skip: `couldn't build storage-api: ${build.error?.message ?? `cargo exited ${build.status}`}` };
  }
  return { bin };
}

export interface Ready {
  url: string;
  s3: string;
  account: string;
  anon_key: string;
  service_key: string;
  jwt_secret: string;
}

export interface Server extends Ready {
  proc: ChildProcess;
  logs: string[];
  stop(): Promise<void>;
}

export async function startServer(bin: string, extra: string[] = []): Promise<Server> {
  const dir = mkdtempSync(path.join(tmpdir(), "roll-storage-js-"));
  const logs: string[] = [];
  const proc = spawn(bin, ["--dir", dir, "--port", "0", "--s3-port", "0", ...extra], {
    env: { ...process.env, RUST_LOG: process.env.RUST_LOG ?? "warn" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  proc.stderr!.on("data", (d) => logs.push(String(d)));
  const stop = async () => {
    if (proc.exitCode === null && proc.signalCode === null) {
      const exited = new Promise((r) => proc.once("exit", r));
      proc.kill("SIGKILL");
      await exited;
    }
    rmSync(dir, { recursive: true, force: true });
  };
  try {
    const ready = await new Promise<Ready>((resolve, reject) => {
      let out = "";
      const timer = setTimeout(() => reject(new Error("timed out after 30s")), 30_000);
      proc.stdout!.on("data", (d) => {
        out += String(d);
        const nl = out.indexOf("\n");
        if (nl < 0) return;
        clearTimeout(timer);
        try {
          resolve(JSON.parse(out.slice(0, nl)) as Ready);
        } catch (e) {
          reject(new Error(`bad ready line ${JSON.stringify(out.slice(0, nl))}: ${String(e)}`));
        }
      });
      proc.once("exit", (code, signal) => {
        clearTimeout(timer);
        reject(new Error(`exited (${code ?? signal}) before it was ready`));
      });
      proc.once("error", reject);
    });
    return { ...ready, proc, logs, stop };
  } catch (e) {
    await stop();
    throw new Error(`storage-api didn't start: ${(e as Error).message}\n${logs.join("")}`);
  }
}

/** A fresh, valid bucket id (bucket ids are global across the deployment). */
export const bucketId = (tag = "t") => `${tag}-${randomUUID().slice(0, 8)}`;

const b64url = (s: string | Buffer) => Buffer.from(s).toString("base64url");

/** An HS256 JWT, as Supabase Auth would issue it. */
export function signJwt(secret: string, claims: Record<string, unknown>): string {
  const head = b64url(JSON.stringify({ alg: "HS256", typ: "JWT" }));
  const body = b64url(JSON.stringify(claims));
  const sig = createHmac("sha256", secret).update(`${head}.${body}`).digest("base64url");
  return `${head}.${body}.${sig}`;
}

/** A signed-in user's access token. */
export function userToken(secret: string, sub: string = randomUUID(), extra: Record<string, unknown> = {}) {
  const token = signJwt(secret, { sub, role: "authenticated", aud: "authenticated", exp: Math.floor(Date.now() / 1000) + 3600, ...extra });
  return { sub, token };
}

export const text = async (b: Blob | null | undefined) => (b ? await b.text() : undefined);

export interface Env {
  server: Server;
  url: string;
  /** Service key: bypasses rules. */
  service: RollStorageClient;
  /** Anon key, no user. */
  anon: RollStorageClient;
  /** A client for a new signed-in user (or `sub`). */
  user(sub?: string): { sub: string; token: string; client: RollStorageClient };
  /** A private bucket made with the service key. */
  bucket(options?: Parameters<RollStorageClient["createBucket"]>[1], tag?: string): Promise<string>;
}

/** Start one server for this test file; `skip` is false when it's available. */
export function setup(): { skip: string | false; env: Env } {
  const binary = serverBinary();
  const skip = binary.skip ?? false;
  if (skip) console.log(`# skipping: ${skip}`);
  const env = {} as Env;
  if (!skip) {
    before(async () => {
      const server = await startServer(binary.bin!);
      env.server = server;
      env.url = server.url;
      env.service = createClient(server.url, server.service_key);
      env.anon = createClient(server.url, server.anon_key);
      env.user = (sub) => {
        const t = userToken(server.jwt_secret, sub);
        return { ...t, client: createClient(server.url, server.anon_key, { accessToken: t.token }) };
      };
      env.bucket = async (options = { public: false }, tag) => {
        const id = bucketId(tag);
        const { error } = await env.service.createBucket(id, options);
        if (error) throw error;
        return id;
      };
    });
    after(async () => {
      await env.server?.stop();
    });
  }
  return { skip, env };
}

/** `{ data, error }` succeeded; returns `data`. */
export function ok<R extends { data: unknown; error: unknown }>(res: R, what = "request"): NonNullable<R["data"]> {
  if (res.error) assert.fail(`${what} failed: ${JSON.stringify(res.error)}`);
  return res.data as NonNullable<R["data"]>;
}

/** `{ data, error }` failed with this Supabase-style `statusCode` (e.g. "404"). */
export function fails(res: { data: unknown; error: unknown }, statusCode: string, what = "request"): StorageApiError {
  assert.ok(res.error, `${what} should fail with ${statusCode}, got data ${JSON.stringify(res.data)}`);
  assert.ok(res.error instanceof StorageApiError, `${what}: expected a StorageApiError, got ${String(res.error)}`);
  assert.equal(res.error.statusCode, statusCode, `${what}: ${JSON.stringify(res.error)}`);
  assert.equal(res.data, null);
  return res.error;
}

export const sha256 = (b: Uint8Array) => createHash("sha256").update(b).digest("hex");
