/**
 * roll storage: the control-plane API (`/v1/storage/...`, crates/s3-control). The S3 API itself is
 * served on its own port (see `StorageInfo.endpoint`); the dashboard only uses the control plane.
 * Times are Unix milliseconds. Contract: docs/storage/architecture.md.
 */

import { call, ROLL_API } from "./roll-api";

export interface StoragePricing {
  usd_per_gb_month: number;
  usd_per_million_class_a: number;
  usd_per_million_class_b: number;
  usd_per_gb_egress_internet: number;
  usd_per_gb_egress_internal: number;
}

export interface StorageInfo {
  /** S3 endpoint, e.g. `http://127.0.0.1:59000`. */
  endpoint: string;
  region: string;
  virtual_host_domain: string | null;
  storage: string;
  storage_class: string;
  recover_days_default: number;
  pricing: StoragePricing;
  buckets: number;
  objects: number;
  bytes: number;
  scrub: { last_at: number | null; missing: number; mismatched: number };
  /** The deployment's canonical owner id (64 hex chars) and 12-digit account id, as in ACLs and ARNs. */
  owner_id: string;
  account_id: string;
  /** Block Public Access applies to every bucket (`ROLL_S3_BLOCK_PUBLIC_ACCESS`). */
  block_public_access: boolean;
}

export interface WebsiteConfig {
  index_document: string | null;
  error_document: string | null;
  redirect_all: { host_name: string; protocol: string | null } | null;
  routing_rules: unknown[];
}

export interface CorsRule {
  /** S3's optional rule ID (`null` when unset; may be left out when writing). */
  id?: string | null;
  allowed_origins: string[];
  allowed_methods: string[];
  allowed_headers: string[];
  expose_headers: string[];
  max_age_s: number | null;
}

export interface LifecycleRule {
  id: string;
  prefix: string;
  enabled: boolean;
  expire_days: number | null;
  abort_multipart_days: number | null;
}

export interface Bucket {
  id: string;
  name: string;
  region: string;
  public: boolean;
  recover_days: number;
  created_at: number;
  parent: { id: string; name: string; at: number } | null;
  branches: string[];
  objects: number;
  bytes: number;
  trash_bytes: number;
  cors: CorsRule[];
  lifecycle: LifecycleRule[];
  endpoint: string;
  /** Path-style URL of the bucket. */
  url: string;
  virtual_host_url: string | null;
  tags: Record<string, string>;
  website: WebsiteConfig | null;
}

export interface BucketPatch {
  public?: boolean;
  recover_days?: number;
  cors?: CorsRule[];
  lifecycle?: LifecycleRule[];
  /** Replaces every bucket tag; `{}` removes them. */
  tags?: Record<string, string>;
}

export interface ObjectSummary {
  key: string;
  size: number;
  etag: string;
  content_type: string | null;
  last_modified: number;
}

export interface ObjectInfo extends ObjectSummary {
  metadata: Record<string, string>;
  tags: Record<string, string>;
  cache_control: string | null;
  content_disposition: string | null;
  content_encoding: string | null;
  content_language: string | null;
  expires: string | null;
  checksum: { algorithm: string; type: string; value: string } | null;
  parts: number;
}

export interface ObjectList {
  objects: ObjectSummary[];
  prefixes: string[];
  next_cursor: string | null;
}

export interface TrashEntry {
  key: string;
  size: number;
  etag: string;
  deleted_at: number;
  expires_at: number;
}

export type Permission = "read" | "write" | "admin";

export interface AccessKey {
  /** The access key id. */
  id: string;
  name: string;
  permission: Permission;
  buckets: string[] | null;
  created_at: number;
  last_used_at: number | null;
}

export interface CreatedKey extends AccessKey {
  secret_access_key: string;
}

export interface StorageCost {
  storage_usd: number;
  class_a_usd: number;
  class_b_usd: number;
  egress_usd: number;
  total_usd: number;
  projected_month_usd: number;
}

export interface StorageUsage {
  bucket: string;
  days: number;
  class_a: number;
  class_b: number;
  bytes_in: number;
  egress_internet_bytes: number;
  egress_internal_bytes: number;
  gb_months: number;
  storage_bytes: number;
  objects: number;
  daily: { day: string; class_a: number; class_b: number; egress_bytes: number; storage_bytes: number; usd: number }[];
  cost: StorageCost;
}

export interface StorageUsageAll {
  days: number;
  cost: StorageCost;
  /** Every bucket's daily rows, summed (including buckets deleted within the window). */
  daily: StorageUsage["daily"];
  buckets: StorageUsage[];
}

const bucket = (name: string) => `/v1/storage/buckets/${encodeURIComponent(name)}`;
const q = (params: Record<string, string | number | undefined | null>) => {
  const s = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v != null && v !== "") s.set(k, String(v));
  const out = s.toString();
  return out ? `?${out}` : "";
};

export const storageApi = {
  info: () => call<StorageInfo>("GET", "/v1/storage"),

  buckets: () => call<Bucket[]>("GET", "/v1/storage/buckets"),
  bucket: (name: string) => call<Bucket>("GET", bucket(name)),
  createBucket: (name: string, opts: { public?: boolean; recover_days?: number } = {}) =>
    call<Bucket>("POST", "/v1/storage/buckets", { name, ...opts }),
  updateBucket: (name: string, patch: BucketPatch) => call<Bucket>("PATCH", bucket(name), patch),
  deleteBucket: (name: string, force = false) =>
    call<{ deleted: string }>("DELETE", `${bucket(name)}${force ? "?force=true" : ""}`),
  branch: (name: string, branch: string) => call<Bucket>("POST", `${bucket(name)}/branches`, { name: branch }),

  objects: (name: string, opts: { prefix?: string; delimiter?: string; cursor?: string | null; limit?: number } = {}) =>
    call<ObjectList>(
      "GET",
      `${bucket(name)}/objects${q({ prefix: opts.prefix, delimiter: opts.delimiter, cursor: opts.cursor, limit: opts.limit })}`,
    ),
  object: (name: string, key: string) => call<ObjectInfo>("GET", `${bucket(name)}/object${q({ key })}`),
  deleteObject: (name: string, key: string) => call<{ deleted: string }>("DELETE", `${bucket(name)}/object${q({ key })}`),
  deleteMany: (name: string, sel: { keys?: string[]; prefix?: string }) =>
    call<{ deleted: number }>("POST", `${bucket(name)}/delete`, sel),
  /** Direct link to the bytes (served with `Content-Disposition: attachment`). */
  downloadUrl: (name: string, key: string) => `${ROLL_API}${bucket(name)}/download${q({ key })}`,
  presign: (name: string, key: string, method: "GET" | "PUT" = "GET", expires_s = 3600) =>
    call<{ url: string; method: string; expires_at: number }>("POST", `${bucket(name)}/presign`, { key, method, expires_s }),

  trash: (name: string, opts: { prefix?: string; cursor?: string | null; limit?: number } = {}) =>
    call<{ objects: TrashEntry[]; next_cursor: string | null }>(
      "GET",
      `${bucket(name)}/trash${q({ prefix: opts.prefix, cursor: opts.cursor, limit: opts.limit })}`,
    ),
  restore: (name: string, key: string, deleted_at: number) =>
    call<ObjectInfo>("POST", `${bucket(name)}/trash/restore`, { key, deleted_at }),

  usage: (name: string, days = 30) => call<StorageUsage>("GET", `${bucket(name)}/usage?days=${days}`),
  usageAll: (days = 30) => call<StorageUsageAll>("GET", `/v1/storage/usage?days=${days}`),

  keys: () => call<AccessKey[]>("GET", "/v1/storage/keys"),
  createKey: (name: string, permission: Permission, buckets: string[] | null) =>
    call<CreatedKey>("POST", "/v1/storage/keys", { name, permission, buckets: buckets ?? undefined }),
  deleteKey: (id: string) => call<{ deleted: string }>("DELETE", `/v1/storage/keys/${encodeURIComponent(id)}`),
};

/**
 * Upload one file through the control plane (`PUT .../upload?key=`), reporting progress. XHR rather
 * than fetch: fetch can't report upload progress.
 */
export function uploadObject(
  name: string,
  key: string,
  file: Blob,
  onProgress: (loaded: number, total: number) => void,
  signal?: AbortSignal,
): Promise<ObjectInfo> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(new Error("upload cancelled"));
      return;
    }
    const xhr = new XMLHttpRequest();
    xhr.open("PUT", `${ROLL_API}${bucket(name)}/upload${q({ key })}`);
    if (file.type) xhr.setRequestHeader("content-type", file.type);
    xhr.upload.onprogress = (e) => onProgress(e.loaded, e.lengthComputable ? e.total : file.size);
    xhr.onload = () => {
      let data: { error?: string } | null = null;
      try {
        data = xhr.responseText ? JSON.parse(xhr.responseText) : null;
      } catch {
        data = null;
      }
      if (xhr.status >= 200 && xhr.status < 300) resolve(data as ObjectInfo);
      else reject(new Error(data?.error ?? `${xhr.status} ${xhr.statusText}`));
    };
    xhr.onerror = () => reject(new Error("upload failed: rolld unreachable"));
    xhr.onabort = () => reject(new Error("upload cancelled"));
    signal?.addEventListener("abort", () => xhr.abort(), { once: true });
    xhr.send(file);
  });
}

const RESERVED_PREFIXES = ["xn--", "sthree-", "amzn-s3-demo-"];
const RESERVED_SUFFIXES = ["-s3alias", "--ol-s3", ".mrap", "--x-s3", "--table-s3"];

/** A dotted-quad IPv4 address, as Rust's `Ipv4Addr` parses it: 0–255, no leading zeros. */
const IPV4 = /^(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)$/;

/**
 * S3 bucket naming rules: 3–63 chars of a-z, 0-9, `-` and `.`, starting and ending alphanumeric.
 * Mirrors the server's check (crates/s3-store/src/validate.rs `bucket_name`).
 */
export function bucketNameError(name: string): string | null {
  if (name.length < 3 || name.length > 63) return "Use 3–63 characters.";
  if (!/^[a-z0-9.-]+$/.test(name)) return "Only lowercase letters, digits, - and . are allowed.";
  if (!/^[a-z0-9]/.test(name) || !/[a-z0-9]$/.test(name)) return "Start and end with a letter or digit.";
  if (name.includes("..")) return "No two adjacent periods.";
  if (IPV4.test(name)) return "Can't look like an IP address.";
  const prefix = RESERVED_PREFIXES.find((p) => name.startsWith(p));
  if (prefix) return `Can't start with ${prefix} (reserved).`;
  const suffix = RESERVED_SUFFIXES.find((x) => name.endsWith(x));
  if (suffix) return `Can't end with ${suffix} (reserved).`;
  return null;
}

export const RECOVER_OPTIONS = [0, 1, 3, 7, 14, 30];

export const PERMISSIONS: { value: Permission; label: string; hint: string }[] = [
  { value: "read", label: "read", hint: "GET, HEAD, list" },
  { value: "write", label: "write", hint: "read + PUT, DELETE, copy, multipart" },
  { value: "admin", label: "admin", hint: "write + create/delete buckets, bucket config" },
];

/** Dollars, keeping fractions of a cent visible: storage bills are often tiny. */
export function usd(n: number) {
  if (n === 0) return "$0";
  if (n < 0.0001) return "<$0.0001";
  if (n < 0.01) return `$${n.toFixed(4)}`;
  return `$${n.toFixed(2)}`;
}

/** Relative time for Unix milliseconds. */
export function agoMs(ms: number) {
  const s = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

/** Time remaining until Unix milliseconds. */
export function inMs(ms: number) {
  const s = Math.max(0, Math.floor((ms - Date.now()) / 1000));
  if (s < 3600) return `${Math.max(1, Math.floor(s / 60))}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

export function formatDate(ms: number) {
  return new Date(ms).toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

/** The last path segment of a key (or of a prefix, keeping its trailing `/`). */
export function baseName(key: string, prefix: string) {
  return key.startsWith(prefix) ? key.slice(prefix.length) : key;
}
