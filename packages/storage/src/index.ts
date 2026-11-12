// @roll/storage: storage-js, pointed at roll, plus roll's extras under `client.roll` and the CDN.
import { StorageClient, type StorageClientOptions, type TransformOptions } from "@supabase/storage-js";
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
  /**
   * The CDN in front of the API (`https://cdn.example.com`, the deployment's CDN base URL):
   * `getPublicUrl` and `cdn()` hand out its URLs for public and signed reads.
   */
  cdnUrl?: string;
}

export interface PublicUrlOptions {
  /** `true`, or a file name: download as an attachment. */
  download?: string | boolean;
  /** Transform the image (a `/render/image/public` URL). */
  transform?: TransformOptions | RollTransformOptions;
}

/** storage-js's transform options, with every format roll converts to. */
export type RollTransformOptions = Omit<TransformOptions, "format"> & { format?: "origin" | "webp" | "avif" | "jpeg" | "png" };

/** The developer API's paths the CDN serves (public and signed reads, immutable URLs). */
const CDN_PATHS = ["/object/public/", "/object/sign/", "/render/image/public/", "/render/image/sign/", "/v/"];
const MOUNT = "/storage/v1";

export class RollStorageClient extends StorageClient {
  /** roll's extras: versions, trash, branches, policies, rule presets and public URLs. */
  readonly roll: RollApi;
  /** The CDN's base URL, when one is configured. */
  readonly cdnUrl?: string;
  private readonly cdnClient?: StorageClient;

  constructor(url: string, headers: Record<string, string> = {}, fetch?: Fetch, opts?: StorageClientOptions & { cdnUrl?: string }) {
    super(url, headers, fetch, opts);
    this.roll = new RollApi(() => ({ url: this.url, headers: this.headers, fetch: this.fetch, throwOnError: this.shouldThrowOnError }));
    const cdn = opts?.cdnUrl?.replace(/\/+$/, "");
    if (cdn) {
      this.cdnUrl = cdn;
      this.cdnClient = new StorageClient(`${cdn}${MOUNT}`, {});
    }
  }

  /**
   * The public URL of an object, or of a transformed image with `transform`, through the CDN when
   * one is configured (else the API's own, as `from(bucket).getPublicUrl` gives).
   */
  getPublicUrl(bucket: string, path: string, options?: PublicUrlOptions): { data: { publicUrl: string } } {
    return (this.cdnClient ?? this).from(bucket).getPublicUrl(path, options as Parameters<ReturnType<StorageClient["from"]>["getPublicUrl"]>[1]);
  }

  /**
   * An URL of this API (a signed URL from `createSignedUrl`, with or without `transform`; a public
   * or immutable URL) as the CDN serves it. Unchanged without a CDN, or for anything the CDN
   * doesn't serve (authenticated requests always go to the API).
   */
  cdn(url: string): string {
    if (!this.cdnUrl) return url;
    const base = this.url.replace(/\/+$/, "");
    if (!url.startsWith(`${base}/`)) return url;
    const rest = url.slice(base.length);
    return CDN_PATHS.some((p) => rest.startsWith(p)) ? `${this.cdnUrl}${MOUNT}${rest}` : url;
  }
}

/**
 * A storage client for roll's developer API, e.g. `createClient("https://…/storage/v1", anonKey)`.
 * Sends `apikey: key` and `Authorization: Bearer <accessToken ?? key>`, like supabase-js.
 */
export function createClient(url: string, key: string, options: CreateClientOptions = {}): RollStorageClient {
  const { accessToken, fetch: custom, cdnUrl } = options;
  const headers: Record<string, string> = { apikey: key, ...options.headers };
  if (typeof accessToken !== "function") {
    if (!hasHeader(headers, "authorization")) headers.Authorization = `Bearer ${accessToken ?? key}`;
    return new RollStorageClient(url, headers, custom, { cdnUrl });
  }
  const base: Fetch = custom ?? ((...args) => fetch(...args));
  const withAuth: Fetch = async (input, init) => {
    const h = new Headers(init?.headers);
    if (!h.has("authorization")) h.set("authorization", `Bearer ${(await accessToken()) ?? key}`);
    return base(input, { ...init, headers: h });
  };
  return new RollStorageClient(url, headers, withAuth, { cdnUrl });
}

function hasHeader(headers: Record<string, string>, name: string) {
  return Object.keys(headers).some((k) => k.toLowerCase() === name);
}
