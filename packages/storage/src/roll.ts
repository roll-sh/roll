// roll's extras on top of the Supabase Storage API: versions, trash, branches and bucket policies.
// Errors are parsed exactly like storage-js parses them and every call returns `{ data, error }`.
import { type Bucket, type FileObjectV2, StorageApiError, type StorageError, StorageUnknownError, isStorageError } from "@supabase/storage-js";

export type Result<T> = { data: T; error: null } | { data: null; error: StorageError };

export interface PageOptions {
  prefix?: string;
  limit?: number;
  cursor?: string | null;
}

export interface ObjectVersion {
  name: string;
  version_id: string;
  is_latest: boolean;
  is_delete_marker: boolean;
  size: number;
  etag: string;
  last_modified: string;
}

export interface VersionsPage {
  versions: ObjectVersion[];
  next_cursor: string | null;
}

export interface TrashItem {
  name: string;
  size: number;
  etag: string;
  /** Unix milliseconds. */
  deleted_at: number;
  /** Unix milliseconds; the item is purged after this. */
  expires_at: number;
  version_id: string | null;
}

export interface TrashPage {
  items: TrashItem[];
  next_cursor: string | null;
}

/** An S3-style bucket policy document (`{ Version, Statement: [...] }`). */
export type PolicyDocument = Record<string, unknown>;

export type RulesPreset = "public-read" | "authenticated-read" | "user-folders" | "owner-only";

export interface PublicUrlRequest {
  /** Transform the image: a rendition's URL. */
  transform?: { width?: number; height?: number; resize?: "cover" | "contain" | "fill"; quality?: number; format?: "origin" | "webp" | "avif" | "jpeg" | "png" };
  /**
   * An immutable URL of the object's current version (`/storage/v1/v/<token>/…`): cached for a
   * year, never stale (a new version is a new URL). Default: the bucket's CDN mode.
   */
  immutable?: boolean;
}

export interface PublicUrl {
  publicUrl: string;
  /** The URL is the CDN's (the bucket has it on). */
  cdn: boolean;
  immutable: boolean;
}

/** What the extras need from the client; read on every call so `setHeader` and friends apply. */
export interface Connection {
  url: string;
  headers: Record<string, string>;
  fetch: typeof fetch;
  throwOnError: boolean;
}

export class RollApi {
  constructor(private readonly conn: () => Connection) {}

  /** Every version and delete marker in a bucket with versioning, newest first per name. */
  versions(bucket: string, options: PageOptions = {}): Promise<Result<VersionsPage>> {
    return this.request("POST", `/roll/versions/${seg(bucket)}`, page(options));
  }

  /** Deleted objects still recoverable from the trash. */
  trash(bucket: string, options: PageOptions = {}): Promise<Result<TrashPage>> {
    return this.request("POST", `/roll/trash/${seg(bucket)}`, page(options));
  }

  /** Put a trashed object back at `path`; `deletedAt` is the item's `deleted_at`. */
  restore(bucket: string, path: string, deletedAt: number): Promise<Result<FileObjectV2>> {
    return this.request("POST", `/roll/trash/${seg(bucket)}/restore`, { path, deletedAt });
  }

  /** The bucket's branches. */
  branches(bucket: string): Promise<Result<Bucket[]>> {
    return this.request("GET", `/roll/branches/${seg(bucket)}`);
  }

  /** Branch a bucket: a zero-copy bucket that starts with the parent's objects. */
  createBranch(bucket: string, name: string): Promise<Result<Bucket>> {
    return this.request("POST", `/roll/branches/${seg(bucket)}`, { name });
  }

  getPolicy(bucket: string): Promise<Result<{ policy: PolicyDocument | null }>> {
    return this.request("GET", `/roll/policy/${seg(bucket)}`);
  }

  setPolicy(bucket: string, policy: PolicyDocument): Promise<Result<{ message: string }>> {
    return this.request("PUT", `/roll/policy/${seg(bucket)}`, policy);
  }

  deletePolicy(bucket: string): Promise<Result<{ message: string }>> {
    return this.request("DELETE", `/roll/policy/${seg(bucket)}`);
  }

  /**
   * The URL to hand out for an object (the caller must be able to read it): the CDN's when the
   * bucket has it on, optionally immutable or of a transformed image. Public buckets' URLs work
   * for anyone.
   */
  publicUrl(bucket: string, path: string, options: PublicUrlRequest = {}): Promise<Result<PublicUrl>> {
    return this.request("POST", `/roll/url/${seg(bucket)}`, { path, ...options });
  }

  /** Replace the bucket's policy with a preset; returns the policy it wrote. */
  applyRules(bucket: string, preset: RulesPreset): Promise<Result<{ policy: PolicyDocument }>> {
    return this.request("POST", `/roll/rules/${seg(bucket)}`, { preset });
  }

  private async request<T>(method: "GET" | "POST" | "PUT" | "DELETE", path: string, body?: unknown): Promise<Result<T>> {
    const { url, headers, fetch, throwOnError } = this.conn();
    try {
      const init: RequestInit = { method, headers: { ...headers } };
      if (body !== undefined) {
        init.headers = { ...headers, "content-type": "application/json" };
        init.body = JSON.stringify(body);
      }
      let res: Response;
      try {
        res = await fetch(`${url}${path}`, init);
      } catch (e) {
        throw new StorageUnknownError(message(e), e);
      }
      if (!res.ok) throw await apiError(res);
      return { data: (await res.json()) as T, error: null };
    } catch (e) {
      if (throwOnError || !isStorageError(e)) throw e;
      return { data: null, error: e };
    }
  }
}

const seg = (s: string) => encodeURIComponent(s);

function page(o: PageOptions) {
  return { prefix: o.prefix ?? "", limit: o.limit, cursor: o.cursor ?? null };
}

/** The error storage-js would build for this response. */
async function apiError(res: Response): Promise<StorageApiError> {
  const status = Number.isFinite(res.status) ? res.status : 500;
  try {
    const err = (await res.json()) as { statusCode?: string; code?: string } | null;
    return new StorageApiError(message(err), status, err?.statusCode || err?.code || String(status), "storage", err?.code);
  } catch {
    return new StorageApiError(res.statusText || `HTTP ${status} error`, status, String(status));
  }
}

function message(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "object" && err !== null) {
    const e = err as Record<string, unknown>;
    for (const k of ["msg", "message", "error_description", "error"]) if (typeof e[k] === "string") return e[k] as string;
    const nested = e.error as Record<string, unknown> | null | undefined;
    if (typeof nested === "object" && nested !== null && typeof nested.message === "string") return nested.message;
  }
  return JSON.stringify(err);
}
