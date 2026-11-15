// What the extension asks roll (docs/secrets.md, "HTTP API"): a folder's grid (which environments
// set which keys; no values) and, for "Move to roll", one write. A value goes only into that write's
// body: never into an error, a log or a message.
import type { GridLike } from "./logic.ts";

export type Conn = { api: string; token: string; project: string };

const enc = encodeURIComponent;

/** An answer that isn't a success: roll's own error text and its code (`approval_required`…). */
export class RollError extends Error {
  readonly status: number;
  readonly code?: string;
  constructor(message: string, status: number, code?: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

async function call<T>(c: Conn, method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${c.api.replace(/\/+$/, "")}/v1/secrets/projects/${enc(c.project)}${path}`, {
    method,
    headers: { authorization: `Bearer ${c.token}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
    body: body === undefined ? undefined : JSON.stringify(body),
    redirect: "error",
  });
  const text = await res.text();
  const json = text ? (JSON.parse(text) as Record<string, unknown>) : {};
  if (!res.ok) throw new RollError(typeof json.error === "string" ? json.error : `roll answered ${res.status}`, res.status, typeof json.code === "string" ? json.code : undefined);
  return json as T;
}

/** The folder's keys across environments: names and which environments set them. */
export const grid = (c: Conn, path: string) => call<GridLike>(c, "GET", `/grid?path=${enc(path)}`);

/** Sets one key (a change in roll's history); 409 `approval_required` on a protected environment. */
export const setValue = (c: Conn, env: string, path: string, key: string, value: string) =>
  call<unknown>(c, "PUT", `/environments/${enc(env)}/secrets/${enc(key)}?path=${enc(path)}`, { value });
