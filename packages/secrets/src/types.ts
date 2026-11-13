/**
 * The roll secrets HTTP API's shapes (docs/secrets.md, "Shapes"): what `crates/roll-secrets`
 * answers and takes. The dashboard's client (apps/dashboard/src/lib/secrets-api.ts) and this
 * package's both use them; change them with the contract.
 */

export type Action = "read" | "reveal" | "write" | "delete" | "admin";
export type SecretType = "shared" | "personal";
export type LinkMode = "live" | "managed" | "dynamic";

export type Environment = {
  id: string;
  slug: string;
  name: string;
  position: number;
  kind: "standard" | "ephemeral";
  base: string | null;
  labels: Record<string, string>;
  expires_at: number | null;
  protected: boolean;
  approvals: number;
  approvers: string[];
  created_at: number;
  secrets: number;
  /** Keys unchanged for longer are flagged stale (insights); null: never. */
  max_age_days: number | null;
  /** The host opened change requests are announced to (the URL itself stays sealed). */
  notify: string | null;
  /** An ephemeral environment that may read a protected base (else it never does). */
  trusted: boolean;
};
/** `PUT …/environments/{env}`: what it should be; `replace` also deletes its other own keys at `path`. */
export type Ensure = Omit<NewEnvironment, "slug"> & { replace?: boolean };
export type Ensured = { environment: Environment; created: boolean; change: Change | null };
export type NewEnvironment = {
  slug: string;
  name?: string;
  kind?: "standard" | "ephemeral";
  base?: string;
  /** Ephemeral only: it may read a protected base (needs admin on it). */
  trusted?: boolean;
  labels?: Record<string, string>;
  expires_at?: number;
  ttl_minutes?: number;
  secrets?: Op[];
};
/** `history_days`: how long old values are kept (null: for good). */
export type Overview = { project: string; environments: Environment[]; previews: number; history_days: number | null };
export type Folder = { path: string; name: string; secrets: number; folders: number };
/** `project`: a folder another project shares with this one (it exported it). */
export type Import = { project?: string; environment: string; path: string };
/** A project this folder is shared with. */
export type Export = { project: string; created_at: number; created_by: string };
export type Link = { kind: string; ref: string; field: string; mode: LinkMode; options?: Record<string, unknown> };
/** A canvas resource linked nowhere yet, its usual keys, and the environments they'd go to (`GET …/suggestions`). */
export type Suggestion = { kind: string; ref: string; name: string | null; links: { key: string; field: string; mode: LinkMode }[]; environments: string[] };
/** `project` on an import: another project that shares the folder. */
export type Source = { kind: "own" } | { kind: "import"; environment: string; path: string; project?: string } | { kind: "base"; environment: string };
export type Secret = {
  id: string;
  key: string;
  path: string;
  type: SecretType;
  version: number;
  comment: string;
  tags: string[];
  link: Link | null;
  fp: string;
  references: string[];
  updated_at: number;
  updated_by: string;
  source: Source;
  /** A personal value of the caller replaces it. */
  overridden: boolean;
  /** When the value stops working outside roll (ms), if someone said. */
  expires_at: number | null;
  /** Why a credential's value is easy to guess; only for a caller who may reveal it. */
  weak?: string;
};
/** `expires_at`: null clears it; absent keeps it. */
export type SetSecret = { value?: string; comment?: string; tags?: string[]; type?: SecretType; link?: Link | null; message?: string; expires_at?: number | null };
/** `path`: another folder for this one operation (default: the batch's). */
export type Op =
  | { op: "set"; key: string; value?: string; comment?: string; tags?: string[]; type?: SecretType; link?: Link | null; path?: string; expires_at?: number | null }
  | { op: "delete"; key: string; type?: SecretType; path?: string };
export type Batch = { path?: string; message?: string; ops: Op[]; if_seq?: number };
export type ChangeKind = "write" | "rollback" | "promote" | "rotate" | "request" | "rename";
export type Change = { seq: number; environment: string; path: string; message: string; by: string; at: number; keys: number; kind: ChangeKind };
/** `what`: added | changed | deleted | comment | tags | link | rotated */
export type Item = { key: string; path: string; type: SecretType; what: string[]; from_fp: string | null; to_fp: string | null };
export type Written = { change: Change | null; secret: Secret | null; warnings: string[] };
/** `change` is null when nothing changed. */
/** `request`: the change request opened instead (a promotion into a protected environment). */
export type Applied = { change: Change | null; items: Item[]; warnings: string[]; request?: ChangeRequest };
export type StateRow = { key: string; path: string; version: number; fp: string; comment: string; tags: string[]; link: Link | null; at: number; expires_at: number | null };
export type ChangeDetail = Change & { items: Item[]; state?: StateRow[] };
export type Version = { version: number; seq: number; op: "set" | "delete"; what: string[]; purged: boolean; fp: string | null; comment: string; tags: string[]; link: Link | null; at: number; by: string; expires_at: number | null };
export type Problem = { key: string; problem: string };
/** `leases`: ids of dynamic credentials issued. */
/** `dynamic`: the keys holding a dynamic link's short-lived credential. */
/** `dynamic`: keys holding a dynamic link's short-lived credential; `linked`: keys whose value comes from any link. */
export type Values = { values: Record<string, string>; problems: Problem[]; leases: string[]; dynamic: string[]; linked: string[] };
export type CellSource = "own" | "import" | "base";
/** `misfit`: the typed-key rule the stored value breaks (shown only to whoever may reveal it). */
export type Cell = { fp: string; version: number; updated_at: number; link: Link | null; reference: boolean; source: CellSource; comment: boolean; personal: boolean; misfit?: string; expires_at?: number };
export type RowStatus = "same" | "different" | "missing";
export type GridRow = { key: string; cells: Record<string, Cell | null>; status: RowStatus };
export type Grid = { path: string; environments: string[]; rows: GridRow[]; folders: string[]; previews: number };
export type CompareStatus = "same" | "different" | "only_from" | "only_to";
export type CompareRow = { key: string; status: CompareStatus; from: number | null; to: number | null };
export type Compare = { from: string; to: string; path: string; rows: CompareRow[] };
export type RequestState = "open" | "approved" | "rejected" | "applied" | "closed" | "stale";
export type Review = { by: string; verdict: "approve" | "reject"; comment: string; at: number };
/** One item of a change request with its values: `now` is null for a new key or a live/dynamic link. */
export type RequestValue = { key: string; path: string; type: "shared" | "personal"; delete: boolean; now: string | null; now_link: Link | null; proposed: string | null; proposed_link: Link | null };

export type KeyInsight = { key: string; version: number; updated_at: number; updated_by: string; last_read_at: number | null; last_read_by: string | null; reads_30d: number; stale: boolean; expires_at: number | null; expiring: boolean;
  /** In a protected environment: unprotected environments holding the very same value. */
  exposed_in: string[];
  /** Why a credential's value is easy to guess; only for a caller who may reveal it. */
  weak?: string;
};
export type Insights = { path: string; max_age_days: number | null; reads_since: number; keys: KeyInsight[] };

export type ChangeRequest = {
  id: string;
  environment: string;
  path: string;
  message: string;
  by: string;
  at: number;
  state: RequestState;
  items: Item[];
  reviews: Review[];
  approvals: number;
  approvals_needed: number;
  applied_seq: number | null;
};
export type LinkField = { field: string; label: string; modes: LinkMode[]; secret: boolean; suggest: string; credential?: string };
export type LinkTarget = { kind: string; ref: string; name: string; fields: LinkField[] };
export type Lease = {
  id: string;
  environment: string;
  /** The folder of the secret it was issued for (`/` for a retired credential's). */
  path: string;
  key: string;
  kind: "dynamic" | "retired";
  link: Link | null;
  consumer: string;
  issued_at: number;
  expires_at: number;
  revoked_at: number | null;
};
export type Token = {
  id: string;
  name: string;
  environments: string[] | null;
  paths: string[] | null;
  actions: Action[];
  expires_at: number | null;
  created_at: number;
  created_by: string;
  last_used_at: number | null;
  prefix: string;
  /** Client addresses (CIDRs) it works from; null: any. */
  allowed_ips: string[] | null;
};
/** A one-time share (never its ciphertext): `open` until its last open, expiry or revocation. */
export type Share = { id: string; note: string; views_left: number; created_by: string; created_at: number; expires_at: number; opened_at: number | null; open: boolean };
/** `ciphertext`: base64url(nonce ‖ AES-256-GCM ciphertext) with `roll-share-v1` as associated data; `views` 1–10, `ttl_minutes` 5–10080. */
export type NewShare = { ciphertext: string; note?: string; views?: number; ttl_minutes?: number };
export type KeyType = "url" | "int" | "port" | "bool" | "json" | "email";
/** A key every standard environment must define in a folder; typed keys also say what its value must be. */
export type Required = { key: string; path: string; comment: string; type?: KeyType; pattern?: string };
/** A workload identity: CI jobs and clusters whose OIDC tokens match log in for a short-lived token. */
export type Identity = {
  id: string;
  name: string;
  /** The token's `iss`. */
  issuer: string;
  audience: string;
  /** The token's `sub`, or a pattern of it (`*` for any run of characters). */
  subject: string;
  /** Other claims that must match. */
  claims: Record<string, string>;
  environments: string[] | null;
  paths: string[] | null;
  actions: Action[];
  ttl_minutes: number;
  created_at: number;
  created_by: string;
  last_used_at: number | null;
  last_subject: string | null;
  /** Client addresses (CIDRs) a login works from; null: any. */
  allowed_ips: string[] | null;
};
export type NewIdentity = { name: string; issuer: string; audience: string; subject: string; claims?: Record<string, string>; environments?: string[]; paths?: string[]; actions: Action[]; ttl_minutes?: number; allowed_ips?: string[] };
export type NewToken = { name: string; environments?: string[]; paths?: string[]; actions: Action[]; expires_at?: number; ttl_days?: number; allowed_ips?: string[] };
export type AuditEntry = { id: number; at: number; actor: string; action: string; environment: string | null; path: string | null; keys: string[]; summary: string; ip: string | null };
/** `since`: rows at or after this time (ms); `before`: rows older than this id. */
export type AuditFilter = { env?: string; key?: string; actor?: string; action?: string; before?: number; since?: number; limit?: number };
export type AuditFormat = "csv" | "jsonl";
/** An audit stream to a SIEM: signed batches of new rows, in order. */
export type Sink = { id: string; url: string; status: "new" | "ok" | "error"; error: string | null; cursor: number; delivered_at: number | null; created_at: number; created_by: string };
export type Imported = Applied & { skipped: string[] };
export type ImportedAsRequest = { request: ChangeRequest; skipped: string[] };
/** `docker`: Docker's `--env-file`; `kubernetes`: a Secret manifest (`name`, `namespace`). */
export type ExportFormat = "dotenv" | "json" | "shell" | "yaml" | "docker" | "kubernetes";
/** A one-way push of an environment's values to a platform outside roll (docs/secrets.md, "Syncs"). Never holds a value. */
export type Sync = {
  id: string;
  environment: string;
  path: string;
  kind: string;
  target: Record<string, unknown>;
  keys: string[] | null;
  prefix: string;
  status: "pending" | "ok" | "error";
  error: string | null;
  last_digest: string | null;
  last_synced_at: number | null;
  /** Names at the target. */
  synced: number;
  signed: boolean;
  created_at: number;
  created_by: string;
};
export type NewSync = { environment: string; path?: string; kind: string; target: Record<string, unknown>; keys?: string[]; prefix?: string };
export type SyncField = { name: string; label: string; required: boolean; placeholder: string };
/** A kind the server registered: its target's fields, whether pushes carry values, whether it has a signing secret. */
export type SyncKind = { kind: string; label: string; values: boolean; signed: boolean; fields: SyncField[] };
/** `resealed`: "all", or how many projects still need the previous key (null without a key); `links`, `syncs`: the kinds registered. */
export type Health = { ok: boolean; key: "env" | "kms" | "file" | "missing"; resealed: "all" | number | null; links: string[]; syncs: string[] };
/** Where a key is set (`GET /v1/secrets/search?key=`): names and versions, never a value. */
export type Place = { project: string; project_name: string; environment: string; path: string; key: string; version: number; updated_at: number; linked: boolean };
/** One thing to fix (`GET …/health`), worst first: where, never a value. */
export type Finding = { issue: "expired" | "missing" | "exposed" | "weak" | "expiring" | "stale" | "unread" | "unreferenced"; environment: string; path: string; key: string; detail: string };
/** One project's findings (`GET /v1/secrets/findings`). */
export type ProjectFindings = { project: string; project_name: string; findings: Finding[] };
/** A URL roll calls to issue and revoke a value that lives outside roll (`rotator` links). */
export type Rotator = { id: string; name: string; host: string; created_at: number; created_by: string };
export type WhoAmI = { principal: string; project?: string | null; scopes?: unknown };

/** A `roll-seal-v1` box (base64url fields): X25519 to the recipient, HKDF-SHA256, AES-256-GCM. */
export type Sealed = { v: number; epk: string; nonce: string; ct: string };
/** A resolve sealed to the caller's key: `sealed` opens to the JSON of `Values.values`, with `context` as the associated data. */
export type SealedValues = { sealed: Sealed; context: string; problems: Problem[]; leases: string[]; dynamic: string[]; linked: string[] };
