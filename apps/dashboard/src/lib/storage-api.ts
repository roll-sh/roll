/**
 * roll storage: the control-plane API (`/v1/storage/...`, crates/s3-control). The S3 API itself is
 * served on its own port (see `StorageInfo.endpoint`); the dashboard only uses the control plane.
 * Times are Unix milliseconds. Contract: docs/storage/architecture.md.
 */

import { ROLL_API } from "./roll-api";
import { accountQuery, assertSessionLive, clearSession, storageCall as call, storageHeaders, StorageError } from "./storage-session";

export interface StoragePricing {
  usd_per_gb_month: number;
  usd_per_million_class_a: number;
  usd_per_million_class_b: number;
  usd_per_gb_egress_internet: number;
  usd_per_gb_egress_internal: number;
  /** Bytes the CDN (Cloudflare) served. */
  usd_per_gb_egress_cached: number;
}

/** The gateway's hot-object cache (`GET /v1/storage` `cache`). */
export interface StorageCacheStats {
  enabled: boolean;
  memory_hits: number;
  disk_hits: number;
  coalesced: number;
  misses: number;
  requests: number;
  gets_avoided: number;
  s3_gets: number;
  bytes_saved: number;
  memory_bytes: number;
  memory_capacity: number;
  disk_bytes: number;
  disk_capacity: number;
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
  cache?: StorageCacheStats | null;
  cdn?: { base_url: string | null } | null;
  buckets: number;
  objects: number;
  bytes: number;
  scrub: { last_at: number | null; missing: number; mismatched: number };
  /** The deployment's canonical owner id (64 hex chars) and 12-digit account id, as in ACLs and ARNs. */
  owner_id: string;
  account_id: string;
  /** Block Public Access applies to every bucket (`ROLL_S3_BLOCK_PUBLIC_ACCESS`). */
  block_public_access: boolean;
  /** `<bucket>.<website_domain>` serves a bucket's website (`ROLL_S3_WEBSITE_DOMAIN`); `null`: none. */
  website_domain: string | null;
}

export interface WebsiteRoutingRule {
  condition: { key_prefix_equals: string | null; http_error_code_returned_equals: number | null } | null;
  redirect: {
    host_name: string | null;
    protocol: string | null;
    http_redirect_code: string | number | null;
    replace_key_prefix_with: string | null;
    replace_key_with: string | null;
  };
}

/** Index and error documents are set here or through the S3 API; redirects only through the S3 API. */
export interface WebsiteConfig {
  index_document: string | null;
  error_document: string | null;
  redirect_all: { host_name: string; protocol: "http" | "https" | null } | null;
  routing_rules: WebsiteRoutingRule[];
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

export interface LifecycleTransition {
  days: number | null;
  /** Unix ms, midnight UTC. */
  date: number | null;
  storage_class: string;
}

export interface LifecycleNoncurrentTransition {
  noncurrent_days: number;
  newer_noncurrent_versions: number | null;
  storage_class: string;
}

/**
 * S3's full lifecycle rule model. The optional fields may be left out when writing; the server
 * always returns them (except `legacy_prefix`, only when set). Transitions are stored, never acted
 * on: data is already in Intelligent-Tiering.
 */
export interface LifecycleRule {
  id: string;
  prefix: string;
  enabled: boolean;
  /** Expire current versions this many days after they were written. */
  expire_days: number | null;
  abort_multipart_days: number | null;
  /** Only versions with all of these tags. */
  tags?: Record<string, string>;
  size_greater_than?: number | null;
  size_less_than?: number | null;
  /** Expire current versions on this date (Unix ms, midnight UTC). */
  expire_at?: number | null;
  /** Remove delete markers that have no noncurrent versions behind them. */
  expire_delete_markers?: boolean;
  /** Delete versions noncurrent for this many days for good (to the trash) … */
  noncurrent_days?: number | null;
  /** … except the newest this many. */
  newer_noncurrent_versions?: number | null;
  transitions?: LifecycleTransition[];
  noncurrent_transitions?: LifecycleNoncurrentTransition[];
  /** Shown as S3's legacy `<Rule><Prefix>` form. */
  legacy_prefix?: boolean;
}

/** `null`: never versioned. A versioned bucket can be suspended, never made unversioned again. */
export type Versioning = "Enabled" | "Suspended" | null;

/** Object Lock, if on, with the default retention of new versions (if any). */
export interface ObjectLockConfig {
  mode: "GOVERNANCE" | "COMPLIANCE" | null;
  days: number | null;
  years: number | null;
}

/** An IAM policy document, as JSON. */
export type IamPolicy = Record<string, unknown>;

/** One notification configuration: events under a key filter go to a destination (by ARN). */
export interface NotificationConfig {
  id: string;
  /** S3's element (queue, topic, function); follows the ARN when left out. */
  type: "queue" | "topic" | "function";
  /** `arn:roll:queues:<vhost>:<queue>`, `arn:roll:streams:<stream>:<topic>`, `arn:roll:webhook:<url>` or an AWS ARN. */
  destination: string;
  events: string[];
  prefix: string | null;
  suffix: string | null;
}

/** A configuration as written: the id is generated and the type taken from the ARN when left out. */
export interface NotificationInput {
  id?: string;
  type?: NotificationConfig["type"];
  destination: string;
  events: string[];
  prefix?: string | null;
  suffix?: string | null;
}

export interface BucketNotifications {
  configurations: NotificationConfig[];
  event_bridge: boolean;
  /** Signs `arn:roll:webhook` deliveries (`X-Roll-Signature`); created with the first webhook. */
  webhook_secret: string | null;
  /** Undelivered events by destination ARN (only destinations with some). */
  backlog: Record<string, { pending: number; last_error: string | null }>;
}

export interface NotificationsPatch {
  /** Replaces them all (`[]` removes); left out: unchanged. */
  configurations?: NotificationInput[];
  event_bridge?: boolean;
  /** Don't send each destination an `s3:TestEvent` first. */
  skip_destination_validation?: boolean;
  rotate_webhook_secret?: boolean;
}

/** A replication rule to another roll bucket, with its status. */
export interface ReplicationRule {
  id: string;
  priority: number | null;
  enabled: boolean;
  /** `""`: every key. */
  prefix: string;
  tags: Record<string, string>;
  delete_markers: boolean;
  existing_objects: boolean;
  /** A roll bucket. */
  destination: string;
  storage_class: string | null;
  owner_override: boolean;
  /** Versions waiting for this destination (at most 1000 counted). */
  pending: number;
  last_error: string | null;
  /** Objects written before the rule are still being queued. */
  backfilling: boolean;
}

export interface ReplicationRuleInput {
  destination: string;
  id?: string;
  priority?: number | null;
  /** Default true. */
  enabled?: boolean;
  prefix?: string;
  tags?: Record<string, string>;
  delete_markers?: boolean;
  existing_objects?: boolean;
  storage_class?: string | null;
  owner_override?: boolean;
}

/** S3's Block Public Access settings. */
export interface PublicAccessBlock {
  block_public_acls: boolean;
  ignore_public_acls: boolean;
  block_public_policy: boolean;
  restrict_public_buckets: boolean;
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
  versioning: Versioning;
  object_lock: ObjectLockConfig | null;
  /** Own noncurrent versions (delete markers included) and their bytes, billed as storage. */
  versions: number;
  version_bytes: number;
  cors: CorsRule[];
  lifecycle: LifecycleRule[];
  endpoint: string;
  /** Path-style URL of the bucket. */
  url: string;
  virtual_host_url: string | null;
  /** The CDN (Cloudflare): off unless `enabled`; `mutable` URLs by key, `immutable` by version. */
  cdn?: { enabled: boolean; mode: "mutable" | "immutable" };
  /** Where the bucket's objects are public: the CDN's base when it's on, else `url`. */
  public_url?: string;
  /**
   * Where the developer API serves the bucket's public objects (`<url>/<key>`; image renditions
   * under `render/image/public/`), through the CDN when `api_cdn`; `null` when the API isn't served.
   */
  api_public_url?: string | null;
  api_cdn?: boolean;
  tags: Record<string, string>;
  website: WebsiteConfig | null;
  /** `<scheme>://<bucket>.<website domain>`, when the deployment has one and the bucket a website. */
  website_url: string | null;
  notifications: BucketNotifications;
  replication: { rules: ReplicationRule[] } | null;
  /** The bucket policy. */
  policy: IamPolicy | null;
  /** Whether the policy grants anonymous access. */
  policy_public: boolean;
  /** The bucket's own block (`null`: none); the deployment's applies on top. */
  public_access_block: PublicAccessBlock | null;
  /** Largest object, in bytes (`null`: no limit). Enforced on every write path. */
  file_size_limit: number | null;
  /** Media types objects may have (`image/png`, `image/*`); `null`: any. */
  allowed_mime_types: string[] | null;
}

export interface BucketPatch {
  public?: boolean;
  recover_days?: number;
  cors?: CorsRule[];
  lifecycle?: LifecycleRule[];
  /** Replaces every bucket tag; `{}` removes them. */
  tags?: Record<string, string>;
  /** Turn versioning on, or suspend it. There's no way back to never versioned. */
  versioning?: "Enabled" | "Suspended";
  notifications?: NotificationsPatch;
  /** Replaces every rule; `null` (or no rules) removes replication. */
  replication?: { rules: ReplicationRuleInput[] } | null;
  /** Index and error documents (routing rules set through the S3 API are kept); `null` removes. */
  website?: { index_document: string; error_document?: string | null } | null;
  /** The bucket policy (object or JSON text); `null` removes it. */
  policy?: IamPolicy | string | null;
  public_access_block?: PublicAccessBlock | null;
  /** Bytes, or text such as `"20MB"` / `"1.5GB"` (decimal units); `null` removes the limit. */
  file_size_limit?: number | string | null;
  /** `null` removes the list (any type). */
  allowed_mime_types?: string[] | null;
}

/** A bucket's upload limits, as written. */
export interface UploadLimits {
  file_size_limit: number | string | null;
  allowed_mime_types: string[] | null;
}

// --- Developer API (Supabase-Storage-compatible REST API) --------------------------------------

/** An OIDC issuer whose ID tokens the developer API accepts as user tokens. */
export interface ApiOidcProvider {
  /** Exactly the tokens' `iss`. */
  issuer: string;
  /** Accepted `aud` values. */
  audiences: string[];
  /** Default: the issuer's discovery document. */
  jwks_uri?: string | null;
}

export interface ApiAuth {
  /** The HS256 secret is write-only: only whether one is set. */
  jwt_secret_set: boolean;
  jwt_issuer: string | null;
  jwt_audience: string | null;
  oidc: ApiOidcProvider[];
}

/** A change to the auth providers: left out, unchanged; `null`, removed. */
export interface ApiAuthPatch {
  /** 32–512 characters. */
  jwt_secret?: string | null;
  jwt_issuer?: string | null;
  jwt_audience?: string | null;
  /** Replaces the list. */
  oidc?: ApiOidcProvider[];
}

/** A rules preset: a bucket policy generated for one bucket. */
export interface RulePreset {
  id: string;
  name: string;
  description: string;
}

/** The developer API of the account the dashboard acts in (its "project"). */
export interface StorageApiView {
  account_id: string;
  /** e.g. `http://host:59001/storage/v1`; `null` when the API listener isn't running. */
  endpoint: string | null;
  /** The API's public and signed reads (and image renditions) through the CDN: `<CDN>/storage/v1`. */
  cdn_endpoint?: string | null;
  /** Public: safe in browsers; requests act as `anon` unless a user token is sent. */
  anon_key: string;
  /** Secret: full access, for servers only. */
  service_key: string;
  key_generation: number;
  /** Unix ms. */
  keys_issued_at: number;
  auth: ApiAuth;
  presets: RulePreset[];
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
  /** `"null"` outside versioned buckets (and for objects from before versioning). */
  version_id: string;
  lock: ObjectLock | null;
}

/** One version's Object Lock: a retention until `retain_until` (Unix ms), and/or a legal hold. */
export interface ObjectLock {
  mode: "GOVERNANCE" | "COMPLIANCE" | null;
  retain_until: number | null;
  /** `null`: never set. */
  legal_hold: boolean | null;
}

/** An entry of a version listing: a version or a delete marker. */
export interface ObjectVersion {
  key: string;
  version_id: string;
  is_latest: boolean;
  delete_marker: boolean;
  size: number;
  etag: string;
  last_modified: number;
}

export interface VersionList {
  versions: ObjectVersion[];
  next_cursor: string | null;
}

/** `version_id` and `delete_marker` only when there's a version to name. */
export interface DeletedObject {
  deleted: string;
  version_id?: string;
  delete_marker?: boolean;
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
  /** The version it was (`"null"` outside versioned buckets). */
  version_id: string;
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

export interface OidcProvider {
  /** Exactly the tokens' `iss`. */
  issuer: string;
  /** Accepted `aud` values (client ids). */
  audiences: string[];
  /** A pattern (`*`, `?`) the token's `sub` must match. */
  subject?: string;
  /** Default: the issuer's discovery document. */
  jwks_uri?: string;
}

/** Who may assume a role. */
export interface Trust {
  /** Permanent access key ids (AssumeRole). */
  keys: string[];
  /** OIDC providers (AssumeRoleWithWebIdentity). */
  oidc: OidcProvider[];
  external_id?: string;
}

export interface Role {
  name: string;
  /** `RR…` */
  id: string;
  arn: string;
  description: string;
  permission: Permission;
  buckets: string[] | null;
  policy: IamPolicy | null;
  trust: Trust;
  max_session_s: number;
  generation: number;
  /** Sessions issued until then (ms) are revoked; 0: never. */
  revoked_before: number;
  created_at: number;
  updated_at: number;
}

export interface RoleSpec {
  description?: string;
  permission: Permission;
  /** `null`: every bucket. */
  buckets?: string[] | null;
  policy?: IamPolicy | string | null;
  trust?: Trust;
  /** 900–43200, default 3600. */
  max_session_s?: number;
}

export interface SessionRequest {
  session_name?: string;
  /** 60 up to the role's max_session_s (default 3600). */
  duration_s?: number;
  /** At most the role's. */
  permission?: Permission;
  /** Within the role's. */
  buckets?: string[];
  policy?: IamPolicy | string;
  tags?: Record<string, string>;
  transitive_tag_keys?: string[];
  source_identity?: string;
}

/** Temporary credentials, returned once. */
export interface Session {
  /** `RT…` */
  access_key_id: string;
  secret_access_key: string;
  session_token: string;
  /** Unix ms. */
  expiration: number;
  role_arn: string;
  assumed_role_arn: string;
  permission: Permission;
  buckets: string[] | null;
  endpoint: string;
  region: string;
  env: {
    AWS_ACCESS_KEY_ID: string;
    AWS_SECRET_ACCESS_KEY: string;
    AWS_SESSION_TOKEN: string;
    AWS_ENDPOINT_URL: string;
    AWS_REGION: string;
  };
}

export interface StorageCost {
  storage_usd: number;
  class_a_usd: number;
  class_b_usd: number;
  egress_usd: number;
  egress_cached_usd: number;
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
  /** Served by the CDN. */
  egress_cached_bytes: number;
  /** Pulled by the CDN on its misses (not billed). */
  egress_cdn_origin_bytes: number;
  gb_months: number;
  storage_bytes: number;
  objects: number;
  daily: { day: string; class_a: number; class_b: number; egress_bytes: number; egress_cached_bytes: number; storage_bytes: number; usd: number }[];
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
const role = (name: string) => `/v1/storage/roles/${encodeURIComponent(name)}`;
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
  createBucket: (name: string, opts: { public?: boolean; recover_days?: number; object_lock?: boolean } = {}) =>
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
  /** Versions (newest first within a key, delete markers included), a page at a time. */
  versions: (name: string, opts: { prefix?: string; cursor?: string | null; limit?: number } = {}) =>
    call<VersionList>("GET", `${bucket(name)}/versions${q({ prefix: opts.prefix, cursor: opts.cursor, limit: opts.limit })}`),
  /** The current version, or `versionId`'s. */
  object: (name: string, key: string, versionId?: string | null) =>
    call<ObjectInfo>("GET", `${bucket(name)}/object${q({ key, version_id: versionId })}`),
  /**
   * Without `versionId`: S3's DeleteObject (to the trash, or a delete marker in a versioned bucket).
   * With it: that version for good (to the trash for `recover_days`; never one under Object Lock).
   */
  deleteObject: (name: string, key: string, versionId?: string | null) =>
    call<DeletedObject>("DELETE", `${bucket(name)}/object${q({ key, version_id: versionId })}`),
  deleteMany: (name: string, sel: { keys?: string[]; prefix?: string }) =>
    call<{ deleted: number }>("POST", `${bucket(name)}/delete`, sel),
  /** Direct link to the bytes (served with `Content-Disposition: attachment`). */
  downloadUrl: (name: string, key: string, versionId?: string | null) =>
    `${ROLL_API}${bucket(name)}/download${q({ key, version_id: versionId, ...accountQuery() })}`,
  /** A small rendition of an image (WebP when the browser takes it), for previews. */
  thumbnailUrl: (name: string, key: string, width = 200, versionId?: string | null) =>
    `${ROLL_API}${bucket(name)}/thumbnail${q({ key, width, version_id: versionId, ...accountQuery() })}`,
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

  /** Replace the notification configurations (validated, and each destination sent a test event, unless skipped). */
  setNotifications: (name: string, configurations: NotificationInput[], opts: { skipValidation?: boolean } = {}) =>
    call<Bucket>("PATCH", bucket(name), {
      notifications: { configurations, ...(opts.skipValidation ? { skip_destination_validation: true } : {}) },
    }),
  /** A new webhook signing secret; the configurations stay as they are. */
  rotateWebhookSecret: (name: string) =>
    call<Bucket>("PATCH", bucket(name), { notifications: { rotate_webhook_secret: true } }),
  /** Replace the replication rules; an empty list removes replication. */
  setReplication: (name: string, rules: ReplicationRuleInput[]) =>
    call<Bucket>("PATCH", bucket(name), { replication: rules.length ? { rules } : null }),
  setWebsite: (name: string, website: { index_document: string; error_document?: string | null } | null) =>
    call<Bucket>("PATCH", bucket(name), { website }),
  setPolicy: (name: string, policy: IamPolicy | string | null) => call<Bucket>("PATCH", bucket(name), { policy }),
  setPublicAccessBlock: (name: string, block: PublicAccessBlock | null) =>
    call<Bucket>("PATCH", bucket(name), { public_access_block: block }),
  /** Both upload limits at once (`null` removes either). */
  setUploadLimits: (name: string, limits: UploadLimits) => call<Bucket>("PATCH", bucket(name), limits),

  /** The developer API: endpoint, keys, auth providers and rule presets. */
  devApi: () => call<StorageApiView>("GET", "/v1/storage/api"),
  /** New anon and service keys; the old ones stop working at once. */
  rotateApiKeys: () => call<StorageApiView>("POST", "/v1/storage/api/keys/rotate"),
  setApiAuth: (patch: ApiAuthPatch) => call<StorageApiView>("PUT", "/v1/storage/api/auth", patch),
  /** The policy a preset would give the bucket (nothing is applied). */
  rulePreview: (name: string, preset: string) =>
    call<{ preset: string; policy: IamPolicy }>("GET", `${bucket(name)}/rules/${encodeURIComponent(preset)}`),
  /** Replace the bucket's policy with the preset's. */
  applyRule: (name: string, preset: string) => call<Bucket>("POST", `${bucket(name)}/rules`, { preset }),

  roles: () => call<Role[]>("GET", "/v1/storage/roles"),
  role: (name: string) => call<Role>("GET", role(name)),
  createRole: (name: string, spec: RoleSpec) => call<Role>("POST", "/v1/storage/roles", { name, ...spec }),
  /** Change some fields (`null` clears buckets and policy). The role's sessions stop working. */
  updateRole: (name: string, patch: Partial<RoleSpec>) => call<Role>("PATCH", role(name), patch),
  deleteRole: (name: string) => call<{ deleted: string }>("DELETE", role(name)),
  /** Sessions issued until now stop working. */
  revokeRole: (name: string) => call<Role>("POST", `${role(name)}/revoke`),
  mintSession: (name: string, req: SessionRequest = {}) => call<Session>("POST", `${role(name)}/sessions`, req),
  /** A new current session-token key; tokens of the previous key keep working, older ones stop. */
  rotateSessionKey: () => call<{ key_id: number | string }>("POST", "/v1/storage/session-keys/rotate"),
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
    let auth: Record<string, string>;
    try {
      assertSessionLive();
      auth = storageHeaders();
    } catch (e) {
      reject(e);
      return;
    }
    for (const [k, v] of Object.entries(auth)) xhr.setRequestHeader(k, v);
    xhr.upload.onprogress = (e) => onProgress(e.loaded, e.lengthComputable ? e.total : file.size);
    xhr.onload = () => {
      let data: { error?: string } | null = null;
      try {
        data = xhr.responseText ? JSON.parse(xhr.responseText) : null;
      } catch {
        data = null;
      }
      if (xhr.status >= 200 && xhr.status < 300) resolve(data as ObjectInfo);
      else {
        if (xhr.status === 401 && "authorization" in auth) clearSession(true);
        const code = (data as { code?: string } | null)?.code ?? null;
        reject(new StorageError(data?.error ?? `${xhr.status} ${xhr.statusText}`, xhr.status, code));
      }
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

/**
 * S3's bucket tag rules, as the server checks them (crates/s3-store/src/config.rs
 * `validate_bucket_tags`): at most 50, keys 1–128 characters and unique, values up to 256, no
 * `aws:` prefix. `null` when the tags are fine.
 */
export function bucketTagsError(tags: [string, string][]): string | null {
  if (tags.length > 50) return "At most 50 tags.";
  const seen = new Set<string>();
  for (const [k, v] of tags) {
    const keyLen = [...k].length;
    if (keyLen === 0) return "Every tag needs a key.";
    if (keyLen > 128) return "Tag keys are at most 128 characters.";
    if ([...v].length > 256) return `Tag values are at most 256 characters (${k}).`;
    if (k.toLowerCase().startsWith("aws:")) return "Keys starting with aws: are reserved.";
    if (seen.has(k)) return `Duplicate key ${k}.`;
    seen.add(k);
  }
  return null;
}

/** A lifecycle rule with every optional field at the server's default, for comparing drafts. */
export function normalizeLifecycleRule(r: LifecycleRule): Required<LifecycleRule> {
  return {
    ...r,
    tags: r.tags ?? {},
    size_greater_than: r.size_greater_than ?? null,
    size_less_than: r.size_less_than ?? null,
    expire_at: r.expire_at ?? null,
    expire_delete_markers: r.expire_delete_markers ?? false,
    noncurrent_days: r.noncurrent_days ?? null,
    newer_noncurrent_versions: r.newer_noncurrent_versions ?? null,
    transitions: r.transitions ?? [],
    noncurrent_transitions: r.noncurrent_transitions ?? [],
    legacy_prefix: r.legacy_prefix ?? false,
  };
}

/** Whether a rule does anything (S3 refuses a rule without an action). */
export function lifecycleHasAction(r: LifecycleRule) {
  const n = normalizeLifecycleRule(r);
  return (
    n.expire_days != null ||
    n.expire_at != null ||
    n.abort_multipart_days != null ||
    n.expire_delete_markers ||
    n.noncurrent_days != null ||
    n.transitions.length > 0 ||
    n.noncurrent_transitions.length > 0
  );
}

/**
 * The parts of a rule the dashboard shows but doesn't edit (filters by tag and size, dates,
 * delete markers, transitions), as short phrases. They're kept as they are on save.
 */
export function lifecycleExtras(r: LifecycleRule): string[] {
  const n = normalizeLifecycleRule(r);
  const day = (ms: number) => new Date(ms).toISOString().slice(0, 10);
  const out: string[] = [];
  for (const [k, v] of Object.entries(n.tags)) out.push(`tag ${k}=${v}`);
  if (n.size_greater_than != null) out.push(`size > ${n.size_greater_than} B`);
  if (n.size_less_than != null) out.push(`size < ${n.size_less_than} B`);
  if (n.expire_at != null) out.push(`expire on ${day(n.expire_at)}`);
  if (n.newer_noncurrent_versions != null) out.push(`keep newest ${n.newer_noncurrent_versions} noncurrent`);
  if (n.expire_delete_markers) out.push("remove lone delete markers");
  for (const t of n.transitions) out.push(`to ${t.storage_class} ${t.days != null ? `after ${t.days}d` : t.date != null ? `on ${day(t.date)}` : ""}`.trim());
  for (const t of n.noncurrent_transitions) out.push(`noncurrent to ${t.storage_class} after ${t.noncurrent_days}d`);
  return out;
}

/** How a bucket's versioning reads in the UI. */
export function versioningLabel(v: Versioning) {
  return v === "Enabled" ? "enabled" : v === "Suspended" ? "suspended" : "off";
}

/** A default retention, e.g. "30 days, governance"; `null` without one. */
export function retentionLabel(lock: ObjectLockConfig | null): string | null {
  if (!lock?.mode) return null;
  const n = lock.days ?? lock.years;
  if (n == null) return null;
  const unit = lock.days != null ? "day" : "year";
  return `${n} ${unit}${n === 1 ? "" : "s"}, ${lock.mode.toLowerCase()}`;
}

/** Whether a version can't be deleted for good now: a legal hold, or a retention still running. */
export function isLocked(lock: ObjectLock | null, now = Date.now()) {
  if (!lock) return false;
  return lock.legal_hold === true || (lock.mode != null && lock.retain_until != null && lock.retain_until > now);
}

// --- Events ------------------------------------------------------------------------------------

/** S3's event types a configuration can name (`s3:TestEvent` is only ever sent, never subscribed to). */
export const EVENT_TYPES = [
  "s3:ObjectCreated:*",
  "s3:ObjectCreated:Put",
  "s3:ObjectCreated:Post",
  "s3:ObjectCreated:Copy",
  "s3:ObjectCreated:CompleteMultipartUpload",
  "s3:ObjectRemoved:*",
  "s3:ObjectRemoved:Delete",
  "s3:ObjectRemoved:DeleteMarkerCreated",
  "s3:ObjectRestore:*",
  "s3:ObjectRestore:Post",
  "s3:ObjectRestore:Completed",
  "s3:ObjectRestore:Delete",
  "s3:ReducedRedundancyLostObject",
  "s3:Replication:*",
  "s3:Replication:OperationFailedReplication",
  "s3:Replication:OperationNotTracked",
  "s3:Replication:OperationMissedThreshold",
  "s3:Replication:OperationReplicatedAfterThreshold",
  "s3:LifecycleExpiration:*",
  "s3:LifecycleExpiration:Delete",
  "s3:LifecycleExpiration:DeleteMarkerCreated",
  "s3:LifecycleTransition",
  "s3:IntelligentTiering",
  "s3:ObjectTagging:*",
  "s3:ObjectTagging:Put",
  "s3:ObjectTagging:Delete",
  "s3:ObjectAcl:Put",
] as const;

/** The event families most configurations want, for a compact picker. */
export const COMMON_EVENTS: readonly string[] = [
  "s3:ObjectCreated:*",
  "s3:ObjectRemoved:*",
  "s3:ObjectTagging:*",
  "s3:ObjectAcl:Put",
  "s3:ObjectRestore:*",
  "s3:LifecycleExpiration:*",
  "s3:Replication:*",
];

/** Where events go, in the forms people think in. */
export type Destination =
  | { kind: "queue"; vhost: string; queue: string }
  | { kind: "stream"; stream: string; topic: string }
  | { kind: "webhook"; url: string }
  | { kind: "arn"; arn: string };

/** The ARN a configuration names for a destination. The queue's vhost is percent-encoded (`/` → `%2F`). */
export function destinationArn(d: Destination): string {
  switch (d.kind) {
    case "queue":
      return `arn:roll:queues:${encodeURIComponent(d.vhost.trim() || "/")}:${d.queue.trim()}`;
    case "stream":
      return `arn:roll:streams:${d.stream.trim()}:${d.topic.trim()}`;
    case "webhook":
      return `arn:roll:webhook:${d.url.trim()}`;
    case "arn":
      return d.arn.trim();
  }
}

/** A destination ARN back in its friendly form (anything else is a raw ARN). */
export function parseDestination(arn: string): Destination {
  const after = (p: string) => (arn.startsWith(p) ? arn.slice(p.length) : null);
  const q = after("arn:roll:queues:");
  if (q != null) {
    const i = q.indexOf(":");
    if (i > 0 && i < q.length - 1) {
      try {
        return { kind: "queue", vhost: decodeURIComponent(q.slice(0, i)), queue: q.slice(i + 1) };
      } catch {
        return { kind: "arn", arn };
      }
    }
  }
  const st = after("arn:roll:streams:");
  if (st != null) {
    const i = st.indexOf(":");
    if (i > 0 && i < st.length - 1) return { kind: "stream", stream: st.slice(0, i), topic: st.slice(i + 1) };
  }
  const w = after("arn:roll:webhook:");
  if (w) return { kind: "webhook", url: w };
  return { kind: "arn", arn };
}

/** How a destination reads: its kind and a short label. */
export function describeDestination(arn: string): { kind: Destination["kind"]; label: string } {
  const d = parseDestination(arn);
  switch (d.kind) {
    case "queue":
      return { kind: "queue", label: d.vhost === "/" ? d.queue : `${d.vhost}/${d.queue}` };
    case "stream":
      return { kind: "stream", label: `${d.stream}/${d.topic}` };
    case "webhook":
      return { kind: "webhook", label: d.url };
    case "arn":
      return { kind: "arn", label: d.arn };
  }
}

/** What's wrong with a destination before it's sent (`null`: fine; the server checks the rest). */
export function destinationError(d: Destination): string | null {
  switch (d.kind) {
    case "queue":
      return d.queue.trim() ? null : "Name the queue.";
    case "stream":
      if (!d.stream.trim() || !d.topic.trim()) return "Name the stream and the topic.";
      return d.stream.includes(":") ? "A stream name can't contain a colon." : null;
    case "webhook": {
      let u: URL;
      try {
        u = new URL(d.url.trim());
      } catch {
        return "Enter a full URL, e.g. https://example.com/hooks/s3.";
      }
      if (u.protocol !== "https:" && u.protocol !== "http:") return "Webhooks are http(s) URLs.";
      if (u.username || u.password) return "Leave credentials out of the URL.";
      return null;
    }
    case "arn":
      return /^arn:[^:]+:[^:]+:/.test(d.arn.trim()) ? null : "Enter an ARN, e.g. arn:aws:sqs:us-east-1:123456789012:jobs.";
  }
}

/** A stored configuration as written back unchanged, for replacing the list without it or with more. */
export function notificationInput(c: NotificationConfig): NotificationInput {
  return { id: c.id, type: c.type, destination: c.destination, events: c.events, prefix: c.prefix, suffix: c.suffix };
}

/** A secret with only its start visible. */
export function maskSecret(secret: string, visible = 10) {
  return `${secret.slice(0, visible)}${"•".repeat(16)}`;
}

// --- Replication -------------------------------------------------------------------------------

/** A rule as written back, without its status. */
export function replicationInput(r: ReplicationRule): ReplicationRuleInput {
  return {
    id: r.id,
    priority: r.priority,
    enabled: r.enabled,
    prefix: r.prefix,
    tags: r.tags,
    delete_markers: r.delete_markers,
    existing_objects: r.existing_objects,
    destination: r.destination,
    storage_class: r.storage_class,
    owner_override: r.owner_override,
  };
}

/** How a rule is doing, in a few words. */
export function replicationStatus(r: ReplicationRule): string {
  if (!r.enabled) return "disabled";
  if (r.last_error) return "retrying";
  if (r.backfilling) return "copying existing objects";
  if (r.pending > 0) return `${r.pending >= 1000 ? "1000+" : r.pending} pending`;
  return "up to date";
}

// --- Access ------------------------------------------------------------------------------------

export const NO_BLOCK: PublicAccessBlock = {
  block_public_acls: false,
  ignore_public_acls: false,
  block_public_policy: false,
  restrict_public_buckets: false,
};

/** The block in force: the bucket's own, with the deployment-wide block (all four) on top. */
export function effectiveBlock(own: PublicAccessBlock | null, deploymentWide: boolean): PublicAccessBlock {
  if (deploymentWide) {
    return { block_public_acls: true, ignore_public_acls: true, block_public_policy: true, restrict_public_buckets: true };
  }
  return own ?? NO_BLOCK;
}

/**
 * A bucket policy as typed: `null` when it's a JSON object with statements (the server checks the
 * rest), else what's wrong. Empty text is fine: it means no policy.
 */
export function policyError(text: string): string | null {
  if (!text.trim()) return null;
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch (e) {
    return `Not valid JSON: ${e instanceof Error ? e.message : String(e)}`;
  }
  if (typeof v !== "object" || v === null || Array.isArray(v)) return "A policy is a JSON object.";
  if (!("Statement" in v)) return "A policy needs a Statement.";
  return null;
}

/** A starting policy: anyone may read the bucket's objects. */
export function publicReadPolicy(bucketName: string): IamPolicy {
  return {
    Version: "2012-10-17",
    Statement: [
      { Sid: "PublicRead", Effect: "Allow", Principal: "*", Action: "s3:GetObject", Resource: `arn:aws:s3:::${bucketName}/*` },
    ],
  };
}

// --- Website -----------------------------------------------------------------------------------

/** The website URL a bucket would have (`null` without a website domain), with the endpoint's scheme and port. */
export function websiteUrlFor(bucketName: string, info: Pick<StorageInfo, "endpoint" | "website_domain">): string | null {
  if (!info.website_domain) return null;
  try {
    const u = new URL(info.endpoint);
    return `${u.protocol}//${bucketName}.${info.website_domain}${u.port ? `:${u.port}` : ""}`;
  } catch {
    return null;
  }
}

/** S3's rule for an index document: a non-empty suffix without `/`. */
export function indexDocumentError(v: string): string | null {
  if (!v.trim()) return "Name the index document, e.g. index.html.";
  if (v.includes("/")) return "The index document is a suffix like index.html, without /.";
  return null;
}

// --- Roles -------------------------------------------------------------------------------------

/** Longest-session choices (15 min to 12 h). */
export const MAX_SESSION_OPTIONS = [900, 1800, 3600, 7200, 14400, 28800, 43200];

/** Session lengths to mint, up to the role's maximum (control-plane sessions may be short). */
export function sessionDurations(max: number): number[] {
  return [300, 900, 3600, 7200, 14400, 28800, 43200].filter((d) => d <= max);
}

/** Seconds as `45s`, `15m`, `1h`, `1h 30m`. */
export function durationLabel(s: number): string {
  if (s < 60) return `${s}s`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h === 0) return `${m}m`;
  return m ? `${h}h ${m}m` : `${h}h`;
}

/** Who may assume a role, in a line. */
export function trustSummary(t: Trust): string {
  const parts: string[] = [];
  if (t.keys.length) parts.push(`${t.keys.length} key${t.keys.length === 1 ? "" : "s"}`);
  for (const p of t.oidc) parts.push(p.issuer.replace(/^https?:\/\//, "").replace(/\/$/, ""));
  if (t.external_id) parts.push("external id");
  return parts.length ? parts.join(" · ") : "control-plane sessions only";
}

/** A session's credentials as `.env` lines. */
export function sessionEnv(s: Session): string {
  return Object.entries(s.env)
    .map(([k, v]) => `${k}=${v}`)
    .join("\n");
}

// --- Upload limits -----------------------------------------------------------------------------

/** Decimal units, as the server parses `"20MB"`. */
export const SIZE_UNITS = [
  { unit: "B", bytes: 1 },
  { unit: "KB", bytes: 1e3 },
  { unit: "MB", bytes: 1e6 },
  { unit: "GB", bytes: 1e9 },
] as const;
export type SizeUnit = (typeof SIZE_UNITS)[number]["unit"];

/** A byte count in the largest unit it fits with at most 3 decimals: 1500000000 → 1.5 GB. */
export function splitSize(bytes: number): { value: string; unit: SizeUnit } {
  for (const { unit, bytes: b } of [...SIZE_UNITS].reverse()) {
    if (b > 1 && bytes >= b && bytes % (b / 1000) === 0) return { value: String(Number((bytes / b).toFixed(3))), unit };
  }
  return { value: String(bytes), unit: "B" };
}

/** A size limit for display, in decimal units: `20 MB`, `1.5 GB`, `no limit`. */
export function formatSizeLimit(bytes: number | null): string {
  if (bytes == null) return "no limit";
  const { value, unit } = splitSize(bytes);
  return `${value} ${unit}`;
}

/** A typed size in a unit as bytes (`null`: empty, no limit), or what's wrong with it. */
export function sizeLimitBytes(value: string, unit: SizeUnit): { bytes: number | null; error: null } | { bytes: null; error: string } {
  const v = value.trim();
  if (!v) return { bytes: null, error: null };
  if (!/^\d+(\.\d+)?$/.test(v)) return { bytes: null, error: "Enter a number, e.g. 20 or 1.5." };
  const bytes = Math.round(Number(v) * SIZE_UNITS.find((u) => u.unit === unit)!.bytes);
  if (!Number.isSafeInteger(bytes)) return { bytes: null, error: "That's too large." };
  return { bytes, error: null };
}

const TOKEN = /^[A-Za-z0-9!#$%&'*+.^_`|~-]+$/;

/**
 * An allowed-types entry: `null` when it's `type/subtype` or `type/*` (parameters are allowed and
 * dropped by the server), else what's wrong. Mirrors crates/s3-store/src/api.rs `media_type`.
 */
export function mimeTypeError(s: string): string | null {
  const [essence, ...params] = s.split(";");
  const e = essence.trim();
  const slash = e.indexOf("/");
  const t = e.slice(0, slash);
  const sub = e.slice(slash + 1);
  if (slash < 0 || !TOKEN.test(t) || !TOKEN.test(sub) || t.includes("*") || (sub.includes("*") && sub !== "*")) {
    return `${s.trim()} isn't a media type like image/png or image/*.`;
  }
  for (const p of params) {
    const kv = p.trim();
    if (!kv) continue;
    const eq = kv.indexOf("=");
    const v = kv.slice(eq + 1);
    const quoted = v.length >= 2 && v.startsWith('"') && v.endsWith('"');
    if (eq < 0 || !TOKEN.test(kv.slice(0, eq)) || !(TOKEN.test(v) || quoted)) return `${s.trim()} has a malformed parameter.`;
  }
  return null;
}

/** An allowed-types entry as the server stores it: lowercase `type/subtype`, parameters dropped. */
export function normalizeMimeType(s: string): string {
  return s.split(";")[0].trim().toLowerCase();
}

// --- Developer API -----------------------------------------------------------------------------

/** An HS256 secret as typed: `null` when it's 32–512 characters. */
export function jwtSecretError(s: string): string | null {
  if (s.length < 32) return `At least 32 characters (${s.length} so far).`;
  if (s.length > 512) return "At most 512 characters.";
  return null;
}

const isHttpUrl = (v: string) => {
  try {
    const u = new URL(v);
    return u.protocol === "https:" || u.protocol === "http:";
  } catch {
    return false;
  }
};

/** An OIDC issuer as typed: `null` when the issuer (and the JWKS URI, if any) are URLs and it has 1–20 audiences. */
export function apiOidcError(p: ApiOidcProvider): string | null {
  if (!p.issuer.trim()) return "Every issuer needs its URL.";
  if (!isHttpUrl(p.issuer.trim())) return `${p.issuer} isn't a URL.`;
  const auds = p.audiences.map((a) => a.trim()).filter(Boolean);
  if (auds.length === 0 || auds.length > 20) return `${p.issuer.trim()}: give 1–20 audiences (the tokens' aud).`;
  if (auds.some((a) => a.length > 255)) return `${p.issuer.trim()}: an audience is at most 255 characters.`;
  if (p.jwks_uri?.trim() && !isHttpUrl(p.jwks_uri.trim())) return `${p.jwks_uri} isn't a URL.`;
  return null;
}

export const DEV_API_CLIENTS = ["@roll/storage", "@supabase/storage-js"] as const;
export type DevApiClient = (typeof DEV_API_CLIENTS)[number];

/** Quickstart code for a developer-API client. */
export function devApiSnippet(client: DevApiClient, endpoint: string, anonKey: string, cdnEndpoint?: string | null): string {
  const upload = "const { data, error } = await storage.from('avatars').upload('public/avatar.png', file)";
  if (client === "@roll/storage") {
    const cdn = cdnEndpoint?.replace(/\/storage\/v1\/?$/, "");
    const create = cdn ? `createClient('${endpoint}', '${anonKey}', { cdnUrl: '${cdn}' })` : `createClient('${endpoint}', '${anonKey}')`;
    return [
      "import { createClient } from '@roll/storage'",
      "",
      `const storage = ${create}`,
      "",
      upload,
      "",
      `// A 200 px thumbnail${cdn ? " from the CDN" : ""}`,
      "const { data: thumb } = storage.getPublicUrl('avatars', 'public/avatar.png', { transform: { width: 200 } })",
    ].join("\n");
  }
  return [
    "import { StorageClient } from '@supabase/storage-js'",
    "",
    `const storage = new StorageClient('${endpoint}', {`,
    `  apikey: '${anonKey}',`,
    `  Authorization: 'Bearer ${anonKey}',`,
    "})",
    "",
    upload,
  ].join("\n");
}
