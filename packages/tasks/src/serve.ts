import { SignatureError } from "./errors.js";
import { signingKeysFromEnv, verifySignature } from "./signature.js";
import { VERSION } from "./version.js";
import { type CallRequest, type FunctionDefinition, type WorkflowFunction, executeFunction } from "./workflow.js";

export const SDK = `roll-tasks-js/${VERSION}`;

export interface ServeOptions {
  /** The app's id in roll. Letters, digits, `-`, `_`, `.`. */
  appId: string;
  functions: WorkflowFunction<any, any>[];
  /**
   * `whsec_…` secrets (current and next) to verify calls with. Default: `ROLL_TASKS_SIGNING_KEY`
   * and `ROLL_TASKS_SIGNING_KEY_NEXT`. Without any, calls aren't verified.
   */
  signingKeys?: string | string[];
  /** Set `false` to skip verification (local development). `true` requires signing keys. */
  verify?: boolean;
  toleranceSeconds?: number;
}

export type FetchHandler = (req: Request) => Promise<Response>;

/** What `GET` on the app URL returns; roll reads it when you sync the app. */
export interface AppDescription {
  id: string;
  functions: FunctionDefinition[];
  sdk: string;
}

const json = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "x-roll-sdk": SDK } });

/**
 * Serve functions over the Fetch API (Next.js route handlers, Bun, Deno, Cloudflare Workers,
 * Hono …). Point roll at the URL with `tasks.apps.sync({ url })`.
 *
 * ```ts
 * // app/api/roll/route.ts
 * const handler = serve({ appId: "shop", functions: [welcome] });
 * export { handler as GET, handler as POST };
 * ```
 */
export function serve(opts: ServeOptions): FetchHandler & { description: AppDescription } {
  if (!opts || typeof opts.appId !== "string" || !/^[A-Za-z0-9_.-]{1,128}$/.test(opts.appId)) {
    throw new TypeError(`serve needs an appId of letters, digits, - _ . (got ${JSON.stringify(opts?.appId)})`);
  }
  const byId = new Map<string, WorkflowFunction>();
  for (const f of opts.functions ?? []) {
    if (byId.has(f.id)) throw new TypeError(`function ${f.id} is defined twice`);
    byId.set(f.id, f);
  }
  const keys = opts.signingKeys === undefined ? signingKeysFromEnv() : (Array.isArray(opts.signingKeys) ? opts.signingKeys : [opts.signingKeys]).filter(Boolean);
  if (opts.verify === true && keys.length === 0) {
    throw new TypeError("serve({ verify: true }) needs signingKeys (or ROLL_TASKS_SIGNING_KEY)");
  }
  const verify = opts.verify !== false && keys.length > 0;
  const description: AppDescription = { id: opts.appId, functions: [...byId.values()].map((f) => f.definition), sdk: SDK };

  const handler = async (req: Request): Promise<Response> => {
    if (req.method === "GET" || req.method === "HEAD") return json(200, description);
    if (req.method !== "POST") return json(405, { error: `method ${req.method} not allowed` });

    const body = new Uint8Array(await req.arrayBuffer());
    if (verify) {
      try {
        await verifySignature({ secrets: keys, headers: req.headers, body, toleranceSeconds: opts.toleranceSeconds });
      } catch (e) {
        if (e instanceof SignatureError) return json(401, { error: `invalid signature: ${e.message}` });
        throw e;
      }
    }
    let call: CallRequest;
    try {
      call = JSON.parse(new TextDecoder().decode(body)) as CallRequest;
    } catch {
      return json(400, { error: "body isn't JSON" });
    }
    const fnId = new URL(req.url).searchParams.get("fnId") ?? call?.function_id;
    const fn = fnId ? byId.get(fnId) : undefined;
    if (!fn) return json(404, { error: `no function ${JSON.stringify(fnId)} in app ${opts.appId}` });
    try {
      return json(200, await executeFunction(fn, call));
    } catch (e) {
      // Only SDK bugs end up here; the handler's own errors are reported as `status: "error"`.
      return json(500, { error: e instanceof Error ? e.message : String(e) });
    }
  };
  return Object.assign(handler, { description });
}

/** The parts of `node:http`'s `IncomingMessage` the adapter uses. */
export interface NodeRequestLike extends AsyncIterable<Uint8Array | string> {
  method?: string;
  url?: string;
  headers: Record<string, string | string[] | undefined>;
  /** `encrypted` is read to tell https from http. */
  socket?: object | null;
}

/** The parts of `node:http`'s `ServerResponse` the adapter uses. */
export interface NodeResponseLike {
  statusCode: number;
  setHeader(name: string, value: string | string[]): unknown;
  end(chunk?: Uint8Array | string): unknown;
}

/**
 * Adapt a Fetch handler for `node:http` (and Express, which passes the same objects).
 *
 * ```ts
 * http.createServer(toNodeHandler(serve({ appId: "shop", functions }))).listen(3000);
 * ```
 */
export function toNodeHandler(handler: FetchHandler): (req: NodeRequestLike, res: NodeResponseLike) => Promise<void> {
  return async (req, res) => {
    try {
      const chunks: Uint8Array[] = [];
      for await (const c of req) chunks.push(typeof c === "string" ? new TextEncoder().encode(c) : c);
      const headers = new Headers();
      for (const [k, v] of Object.entries(req.headers)) {
        if (v === undefined) continue;
        for (const one of Array.isArray(v) ? v : [v]) headers.append(k, one);
      }
      const proto = headers.get("x-forwarded-proto")?.split(",")[0]?.trim() || ((req.socket as { encrypted?: boolean } | null | undefined)?.encrypted ? "https" : "http");
      const host = headers.get("x-forwarded-host") ?? headers.get("host") ?? "localhost";
      const method = (req.method ?? "GET").toUpperCase();
      let body: Uint8Array | undefined;
      if (method !== "GET" && method !== "HEAD") {
        body = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
        let o = 0;
        for (const c of chunks) {
          body.set(c, o);
          o += c.length;
        }
      }
      const request = new Request(`${proto}://${host}${req.url ?? "/"}`, { method, headers, body: body as BodyInit | undefined });
      const response = await handler(request);
      res.statusCode = response.status;
      response.headers.forEach((value, key) => res.setHeader(key, value));
      res.end(new Uint8Array(await response.arrayBuffer()));
    } catch (e) {
      res.statusCode = 500;
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify({ error: e instanceof Error ? e.message : String(e) }));
    }
  };
}
