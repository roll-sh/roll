// Integration harness: the real storage-api example server (crates/s3-api) on a temp directory.
import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { createHash, createHmac, randomUUID } from "node:crypto";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { deflateSync } from "node:zlib";
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

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});

function crc32(b: Uint8Array): number {
  let c = 0xffffffff;
  for (const x of b) c = CRC_TABLE[(c ^ x) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

/** A `w`×`h` RGB PNG (a gradient), made without image libraries. */
export function png(w: number, h: number): Uint8Array {
  const raw = Buffer.alloc((w * 3 + 1) * h);
  for (let y = 0; y < h; y++) {
    raw[y * (w * 3 + 1)] = 0;
    for (let x = 0; x < w; x++) raw.set([x & 0xff, y & 0xff, (x + y) & 0xff], y * (w * 3 + 1) + 1 + x * 3);
  }
  const chunk = (type: string, data: Buffer) => {
    const td = Buffer.concat([Buffer.from(type, "latin1"), data]);
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(td));
    return Buffer.concat([len, td, crc]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr.set([8, 2, 0, 0, 0], 8);
  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  return new Uint8Array(Buffer.concat([sig, chunk("IHDR", ihdr), chunk("IDAT", deflateSync(raw)), chunk("IEND", Buffer.alloc(0))]));
}

/** A PNG's or WebP's format and size, read from its header. */
export function imageSize(b: Uint8Array): { format: "png" | "webp" | "jpeg" | "other"; width: number; height: number } {
  const buf = Buffer.from(b);
  if (buf.subarray(1, 4).toString("latin1") === "PNG") return { format: "png", width: buf.readUInt32BE(16), height: buf.readUInt32BE(20) };
  if (buf.subarray(0, 4).toString("latin1") === "RIFF" && buf.subarray(8, 12).toString("latin1") === "WEBP") {
    const kind = buf.subarray(12, 16).toString("latin1");
    if (kind === "VP8 ") return { format: "webp", width: buf.readUInt16LE(26) & 0x3fff, height: buf.readUInt16LE(28) & 0x3fff };
    if (kind === "VP8L") {
      const bits = buf.readUInt32LE(21);
      return { format: "webp", width: (bits & 0x3fff) + 1, height: ((bits >> 14) & 0x3fff) + 1 };
    }
    if (kind === "VP8X") return { format: "webp", width: buf.readUIntLE(24, 3) + 1, height: buf.readUIntLE(27, 3) + 1 };
  }
  if (buf[0] === 0xff && buf[1] === 0xd8) return { format: "jpeg", width: 0, height: 0 };
  return { format: "other", width: 0, height: 0 };
}
