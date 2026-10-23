// @roll/storage: storage-js, pointed at roll, plus roll's extras under `client.roll`.
import { StorageClient, type StorageClientOptions } from "@supabase/storage-js";
import { RollApi } from "./roll.js";

export * from "@supabase/storage-js";
export * from "./roll.js";

type Fetch = typeof fetch;

export interface CreateClientOptions {
  /** A user's JWT, or a function returning one (resolved per request). Defaults to the key. */
  accessToken?: string | (() => Promise<string | null>);
  /** Extra headers sent with every request. */
  headers?: Record<string, string>;
  fetch?: Fetch;
}

export class RollStorageClient extends StorageClient {
  /** roll's extras: versions, trash, branches, policies and rule presets. */
  readonly roll: RollApi;

  constructor(url: string, headers: Record<string, string> = {}, fetch?: Fetch, opts?: StorageClientOptions) {
    super(url, headers, fetch, opts);
    this.roll = new RollApi(() => ({ url: this.url, headers: this.headers, fetch: this.fetch, throwOnError: this.shouldThrowOnError }));
  }
}

/**
 * A storage client for roll's developer API, e.g. `createClient("https://…/storage/v1", anonKey)`.
 * Sends `apikey: key` and `Authorization: Bearer <accessToken ?? key>`, like supabase-js.
 */
export function createClient(url: string, key: string, options: CreateClientOptions = {}): RollStorageClient {
  const { accessToken, fetch: custom } = options;
  const headers: Record<string, string> = { apikey: key, ...options.headers };
  if (typeof accessToken !== "function") {
    if (!hasHeader(headers, "authorization")) headers.Authorization = `Bearer ${accessToken ?? key}`;
    return new RollStorageClient(url, headers, custom);
  }
  const base: Fetch = custom ?? ((...args) => fetch(...args));
  const withAuth: Fetch = async (input, init) => {
    const h = new Headers(init?.headers);
    if (!h.has("authorization")) h.set("authorization", `Bearer ${(await accessToken()) ?? key}`);
    return base(input, { ...init, headers: h });
  };
  return new RollStorageClient(url, headers, withAuth);
}

function hasHeader(headers: Record<string, string>, name: string) {
  return Object.keys(headers).some((k) => k.toLowerCase() === name);
}
