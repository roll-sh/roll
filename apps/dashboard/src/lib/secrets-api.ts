/**
 * Client for roll secrets (`crates/roll-secrets`, docs/secrets.md "HTTP API"): a project's
 * environments, folders, secrets, history, links, change requests, tokens and audit. The shapes are
 * the contract's; change both together.
 *
 * Values only come back from the reveal routes (`value`, `versionValue`, `values`). Callers keep a
 * revealed value for as long as it's shown and no longer.
 */

import type { Action, SecretType, Environment, NewEnvironment, Overview, Folder, Import, Link, Secret, SetSecret, Batch, Change, Written, Applied, ChangeDetail, Version, Problem, Values, Grid, Compare, RequestState, RequestValue, Insights, ChangeRequest, LinkTarget, Lease, Token, Export, Ensure, Ensured, Suggestion, Identity, NewIdentity, Required, KeyType, Totp, Share, NewShare, NewToken, AuditEntry, AuditFilter, AuditVerification, AuditFormat, Sink, Imported, ImportedAsRequest, ExportFormat, Sync, NewSync, SyncKind, Health, WhoAmI, Place, Rotator, SaasRotator, NewSaasRotator, AwsRole, Finding, ProjectFindings, SyncPreview, FileDiff, OutsideDatabase, Directory, NewDirectory, SnowflakeAccount, NewSnowflakeAccount, AtlasProject, NewAtlasProject, AwsUser, CloudAccount, NewCloudAccount, KubernetesCluster, NewKubernetesCluster, SignedCertificate, CaCertificate, Ca, AcmeSettings, NewAcmeKey, SshCa, HostCertificate, UserCertificate, TransitKey, TransitPublicKey, Rotation, FolderPolicy, Reach, GitHubApp, SinkFormat, Gateway, MadeGateway, NewDecoy, CutOff, CutOffRequest, ValidityProbe, CheckedCredential } from "@roll/secrets/types";
import type { Grant } from "./access-api";
import { withSession } from "./code-token";
import { grantFor, tokenOf } from "./secrets-tokens";
import { apiHeaders, onApiStatus, ROLL_API } from "./roll-api";

/** The API's shapes live in the `@roll/secrets` package, shared with services. */
export type * from "@roll/secrets/types";

export const ACTIONS: readonly Action[] = ["read", "reveal", "write", "delete", "admin"];
export const KEY_TYPES: readonly KeyType[] = ["url", "int", "port", "bool", "json", "email", "totp"];

/** An answer that isn't 2xx: the API's `error`, its `code` (`approval_required`, `stale`, `conflict`…) and the status. */
export class SecretsError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code: string | null,
  ) {
    super(message);
    this.name = "SecretsError";
  }
}

/** A write to a protected environment: it needs a change request with the same operations. */
export const needsApproval = (e: unknown): e is SecretsError => e instanceof SecretsError && e.code === "approval_required";

/** Like `call` in roll-api.ts, but keeps the error's `code`, which the approval flow needs. */
async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(
    `${ROLL_API}${path}`,
    withSession({
      method,
      headers: apiHeaders(body),
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
    }),
  );
  onApiStatus(res.status);
  const text = await res.text();
  let data: unknown = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    if (res.ok) throw new Error(`${method} ${path}: the response isn't JSON`);
  }
  if (!res.ok) {
    const err = (data ?? {}) as { error?: string; code?: string };
    throw new SecretsError(err.error ?? (`${res.status} ${res.statusText}`.trim() || "request failed"), res.status, err.code ?? null);
  }
  return data as T;
}

const enc = encodeURIComponent;

/** A query string from the set entries (`?a=1&b=2`, or nothing). */
export function qs(q: Record<string, string | number | boolean | undefined | null>): string {
  const parts = Object.entries(q)
    .filter(([, v]) => v !== undefined && v !== null && v !== "")
    .map(([k, v]) => `${enc(k)}=${enc(String(v))}`);
  return parts.length ? `?${parts.join("&")}` : "";
}

/** A route that answers a file (an export): its text, or the API's error. */
async function text(url: string): Promise<string> {
  const res = await fetch(`${ROLL_API}${url}`, withSession({ headers: apiHeaders(), cache: "no-store" }));
  onApiStatus(res.status);
  const body = await res.text();
  if (!res.ok) {
    let err: { error?: string; code?: string } = {};
    try {
      err = JSON.parse(body);
    } catch {
      // not JSON: the status says it
    }
    throw new SecretsError(err.error ?? `${res.status} ${res.statusText}`.trim(), res.status, err.code ?? null);
  }
  return body;
}

/** The client for one project. */
export function secretsApi(project: string) {
  const P = `/v1/secrets/projects/${enc(project)}`;
  const E = (env: string) => `${P}/environments/${enc(env)}`;
  const K = (env: string, key: string) => `${E(env)}/secrets/${enc(key)}`;
  return {
    overview: () => req<Overview>("GET", P),
    grid: (path = "/") => req<Grid>("GET", `${P}/grid${qs({ path })}`),
    compare: (from: string, to: string, path = "/") => req<Compare>("GET", `${P}/compare${qs({ from, to, path })}`),

    environments: () => req<Environment[]>("GET", `${P}/environments`),
    createEnvironment: (e: NewEnvironment) => req<Environment>("POST", `${P}/environments`, e),
    ensureEnvironment: (slug: string, e: Ensure) => req<Ensured>("PUT", `${P}/environments/${enc(slug)}`, e),
    deleteByLabel: (label: string) => req<{ deleted: string[] }>("DELETE", `${P}/environments${qs({ label })}`),
    order: (order: string[]) => req<Environment[]>("PUT", `${P}/environments/order`, { order }),
    environment: (env: string) => req<Environment>("GET", E(env)),
    updateEnvironment: (env: string, patch: { slug?: string; name?: string; expires_at?: number | null; max_age_days?: number | null }) => req<Environment>("PATCH", E(env), patch),
    deleteEnvironment: (env: string, confirm?: string) => req<null>("DELETE", `${E(env)}${qs({ confirm })}`),
    /** `notify_url`: where opened change requests are announced (null stops it, absent keeps it). */
    policy: (env: string, p: { protected: boolean; approvals?: number; approvers?: string[]; notify_url?: string | null }) => req<Environment>("PUT", `${E(env)}/policy`, p),
    folderPolicies: (env: string) => req<FolderPolicy[]>("GET", `${E(env)}/folder-policies`),
    setFolderPolicy: (env: string, path: string, p: { approvals?: number; approvers?: string[] } = {}) => req<FolderPolicy>("PUT", `${E(env)}/folder-policies${qs({ path })}`, p),
    deleteFolderPolicy: (env: string, path: string) => req<null>("DELETE", `${E(env)}/folder-policies${qs({ path })}`),
    /** Destroys older versions' values of a key (all but the current one by default). */
    purge: (env: string, key: string, path = "/", before_version?: number) => req<{ purged: number }>("POST", `${K(env, key)}/purge${qs({ path })}`, before_version ? { before_version } : {}),
    /** How long old values are kept (null: for good). */
    setHistoryDays: (history_days: number | null) => req<{ history_days: number | null }>("PATCH", P, { history_days }),
    /** Check credentials found in code with their providers (admin: on or off; it contacts them). */
    setValidityChecks: (validity_checks: "on" | "off") => req<{ validity_checks: "on" | "off" }>("PATCH", P, { validity_checks }),
    /** Whether found credentials work, asked of their providers; none asked while the project's checks are off. */
    checkValidity: (credentials: ValidityProbe[]) => req<{ validity_checks: "on" | "off"; results: CheckedCredential[] }>("POST", `${P}/validity`, { credentials }),
    /** A credential's last check, by its fingerprint. */
    unstored: (fingerprint: string) => req<CheckedCredential>("GET", `${P}/unstored/${enc(fingerprint)}`),
    /** The customer-managed KMS key wrapping the project's data key (admin), if one does. */
    kms: () => req<{ key: string | null }>("GET", `${P}/kms`),
    /** Wrap the data key with a KMS key's ARN: roll encrypts and decrypts with it first. */
    setKms: (key: string) => req<{ key: string }>("PUT", `${P}/kms`, { key }),
    /** Back to roll's deployment key alone (the KMS key must still answer). */
    removeKms: () => req<{ key: null }>("DELETE", `${P}/kms`),

    folders: (env: string, path = "/") => req<{ path: string; folders: Folder[] }>("GET", `${E(env)}/folders${qs({ path })}`),
    createFolder: (env: string, path: string) => req<Folder>("POST", `${E(env)}/folders`, { path }),
    deleteFolder: (env: string, path: string) => req<{ change: Change | null }>("DELETE", `${E(env)}/folders${qs({ path })}`),
    imports: (env: string, path = "/") => req<Import[]>("GET", `${E(env)}/imports${qs({ path })}`),
    setImports: (env: string, path: string, imports: Import[]) => req<Import[]>("PUT", `${E(env)}/imports${qs({ path })}`, imports),
    shared: () => req<Import[]>("GET", `${P}/shared`),
    suggestions: () => req<Suggestion[]>("GET", `${P}/suggestions`),
    /** The whole project sealed to `recipient` (an X25519 public key): only its holder opens it. */
    backup: (recipient: string) => req<{ sealed: unknown; context: string }>("POST", `${P}/backup`, { recipient }),
    exports: (env: string, path = "/") => req<Export[]>("GET", `${E(env)}/exports${qs({ path })}`),
    setExports: (env: string, path: string, projects: string[]) => req<Export[]>("PUT", `${E(env)}/exports${qs({ path })}`, projects.map((project) => ({ project }))),

    secrets: (env: string, path = "/") => req<Secret[]>("GET", `${E(env)}/secrets${qs({ path })}`),
    secret: (env: string, key: string, path = "/", type?: SecretType) => req<Secret>("GET", `${K(env, key)}${qs({ path, type })}`),
    set: (env: string, key: string, body: SetSecret, path = "/") => req<Written>("PUT", `${K(env, key)}${qs({ path })}`, body),
    remove: (env: string, key: string, path = "/", type?: SecretType) => req<Written>("DELETE", `${K(env, key)}${qs({ path, type })}`),
    /** A decoy key (an admin's): answered as the key, never its value. */
    addDecoy: (env: string, d: NewDecoy) => req<Secret>("POST", `${E(env)}/decoys`, d),
    /** Reveals one value (audited). `resolve` (default true) expands references. */
    value: (env: string, key: string, path = "/", resolve = true) =>
      req<{ value: string; problems: Problem[] }>("GET", `${K(env, key)}/value${qs({ path, resolve: resolve ? undefined : "false" })}`),
    /** A TOTP key's current code (audited as `secrets.totp`, without the code). */
    totp: (env: string, key: string, path = "/") => req<Totp>("GET", `${K(env, key)}/totp${qs({ path })}`),
    versions: (env: string, key: string, path = "/", type?: SecretType) => req<Version[]>("GET", `${K(env, key)}/versions${qs({ path, type })}`),
    versionValue: (env: string, key: string, version: number, path = "/", type?: SecretType) =>
      req<{ value: string }>("GET", `${K(env, key)}/versions/${version}/value${qs({ path, type })}`),
    rotate: (env: string, key: string, path = "/") => req<Applied>("POST", `${K(env, key)}/rotate${qs({ path })}`),

    values: (env: string, path = "/", resolve = true) => req<Values>("GET", `${E(env)}/values${qs({ path, resolve: resolve ? undefined : "false" })}`),
    /** The environment's values as a file (`dotenv`, `json`, `shell`, `yaml`): text, not JSON. */
    /** `seq`: the folder's own values as they were after that change (as written then). */
    exportText: (env: string, format: ExportFormat, path = "/", seq?: number) => text(`${E(env)}/values${qs({ path, format, seq })}`),

    changes: (env: string, before?: number, limit?: number) => req<Change[]>("GET", `${E(env)}/changes${qs({ before, limit })}`),
    batch: (env: string, b: Batch) => req<Applied>("POST", `${E(env)}/changes`, b),
    importDotenv: (env: string, body: { dotenv: string; path?: string; overwrite?: boolean; message?: string }) => req<Imported>("POST", `${E(env)}/import`, body),
    /** A local copy against the folder, by fingerprint (needs reveal; audited). */
    diffFile: (env: string, body: { dotenv: string; path?: string }) => req<FileDiff>("POST", `${E(env)}/diff`, body),
    /** The same import as a change request (a protected environment). */
    importAsRequest: (env: string, body: { dotenv: string; path?: string; overwrite?: boolean; message?: string }) =>
      req<ImportedAsRequest>("POST", `${E(env)}/import`, { ...body, request: true }),
    change: (env: string, seq: number, state = false) => req<ChangeDetail>("GET", `${E(env)}/changes/${seq}${qs({ state: state ? 1 : undefined })}`),
    rollback: (env: string, body: { to?: number; at?: number; message?: string; bypass?: boolean }) => req<Applied>("POST", `${E(env)}/rollback`, body),
    promote: (to: string, from: string, body: { path?: string; keys?: string[]; message?: string }) => req<Applied>("POST", `${E(to)}/promote${qs({ from })}`, body),

    requests: (env: string, state?: RequestState) => req<ChangeRequest[]>("GET", `${E(env)}/requests${qs({ state })}`),
    openRequest: (env: string, b: Batch) => req<ChangeRequest>("POST", `${E(env)}/requests`, b),
    /** When each key of a folder was last read and changed, and which are stale. */
    insights: (env: string, path = "/") => req<Insights>("GET", `${E(env)}/insights${qs({ path })}`),
    reach: (env: string, path = "/", key?: string) => req<Reach>("GET", `${E(env)}/reach${qs({ path, key })}`),
    /** An incident's cut-off of everything reach lists (a project admin's); the answer says what was cut and what failed. */
    cutOff: (env: string, path: string, key: string | undefined, body: CutOffRequest) => req<CutOff>("POST", `${E(env)}/reach/cut-off${qs({ path, key })}`, body),
    /** Lift a folder's freeze. */
    unfreeze: (env: string, path = "/") => req<null>("DELETE", `${E(env)}/reach/cut-off${qs({ path })}`),
    request: (env: string, id: string) => req<ChangeRequest>("GET", `${E(env)}/requests/${enc(id)}`),
    /** A request's values now and proposed, for an approver who may reveal them (audited). */
    requestValues: (env: string, id: string) => req<RequestValue[]>("GET", `${E(env)}/requests/${enc(id)}/values`),
    review: (env: string, id: string, verdict: "approve" | "reject", comment?: string) =>
      req<ChangeRequest>("POST", `${E(env)}/requests/${enc(id)}/review`, { verdict, comment }),
    apply: (env: string, id: string, bypass = false) => req<ChangeRequest>("POST", `${E(env)}/requests/${enc(id)}/apply`, bypass ? { bypass } : {}),
    close: (env: string, id: string) => req<ChangeRequest>("POST", `${E(env)}/requests/${enc(id)}/close`),

    links: () => req<LinkTarget[]>("GET", `${P}/links`),
    leases: () => req<Lease[]>("GET", `${P}/leases`),
    revokeLease: (id: string) => req<null>("DELETE", `${P}/leases/${enc(id)}`),
    /** Pushes a dynamic lease's expiry out by its link's TTL (or `ttl_minutes`). */
    renewLease: (id: string, ttl_minutes?: number) => req<Lease>("POST", `${P}/leases/${enc(id)}/renew`, ttl_minutes ? { ttl_minutes } : {}),

    /** The project's tokens: roll-access's, with a rule on this project's secrets (live ones). */
    tokens: () => req<Grant[]>("GET", `/v1/access/tokens${qs({ product: "secrets", resource: project })}`).then((gs) => gs.filter((g) => !g.revoked_at).map((g) => tokenOf(g, project))),
    /** The token is in the answer this once. */
    createToken: (t: NewToken) => req<{ grant: Grant; token: string }>("POST", "/v1/access/tokens", grantFor(project, t)).then((r) => ({ ...tokenOf(r.grant, project), token: r.token })),
    revokeToken: (id: string) => req<unknown>("DELETE", `/v1/access/tokens/${enc(id)}`),
    required: () => req<Required[]>("GET", `${P}/required`),
    setRequired: (list: Required[]) => req<Required[]>("PUT", `${P}/required`, list),
    identities: () => req<Identity[]>("GET", `${P}/identities`),
    /** Replace an identity's pinned keys (its id stays). */
    setIdentityKeys: (id: string, jwks: string) => req<Identity>("PUT", `${P}/identities/${enc(id)}/jwks`, { jwks }),
    createIdentity: (i: NewIdentity) => req<Identity>("POST", `${P}/identities`, i),
    deleteIdentity: (id: string) => req<null>("DELETE", `${P}/identities/${enc(id)}`),
    /** `disabled: false` enables an identity a cut-off disabled. */
    setIdentityDisabled: (id: string, disabled: boolean) => req<Identity>("PATCH", `${P}/identities/${enc(id)}`, { disabled }),

    shares: () => req<Share[]>("GET", `${P}/shares`),
    createShare: (s: NewShare) => req<Share>("POST", `${P}/shares`, s),
    revokeShare: (id: string) => req<null>("DELETE", `${P}/shares/${enc(id)}`),

    listSyncs: () => req<{ syncs: Sync[]; kinds: SyncKind[] }>("GET", `${P}/syncs`),
    /** `signing_secret`: for a signed kind, shown once. */
    createSync: (s: NewSync) => req<Sync & { signing_secret?: string }>("POST", `${P}/syncs`, s),
    previewSync: (s: NewSync) => req<SyncPreview>("POST", `${P}/syncs/preview`, s),
    deleteSync: (id: string) => req<null>("DELETE", `${P}/syncs/${enc(id)}`),
    /** `paused: false` resumes a sync a cut-off paused. */
    setSyncPaused: (id: string, paused: boolean) => req<Sync>("PATCH", `${P}/syncs/${enc(id)}`, { paused }),
    /** Pushes every key now; 409 `running` while another push holds it. */
    runSync: (id: string) => req<Sync>("POST", `${P}/syncs/${enc(id)}/run`),

    audit: (f: AuditFilter = {}) => req<AuditEntry[]>("GET", `${P}/audit${qs(f)}`),
    /** The audit log with these filters as a file (up to 10,000 rows): text, not JSON. */
    auditText: (f: AuditFilter, format: AuditFormat) => text(`${P}/audit${qs({ ...f, before: undefined, limit: undefined, digests: undefined, format })}`),
    /** Walks the audit log's hash chain and checks roll's signed digests (from the last before `since`). */
    verifyAudit: (since?: number) => req<AuditVerification>("GET", `${P}/audit/verify${qs({ since })}`),
    sinks: () => req<Sink[]>("GET", `${P}/audit/sinks`),
    /** The `whsec_…` signing secret comes back once. */
    /** Postgres outside roll: registering one connects to it and checks both roles. */
    githubApps: () => req<GitHubApp[]>("GET", `${P}/github-apps`),
    addGitHubApp: (a: { name: string; app_id: number; installation_id: number; private_key: string; api_base?: string; repositories: string[]; permissions: Record<string, string> }) => req<GitHubApp>("POST", `${P}/github-apps`, a),
    deleteGitHubApp: (id: string) => req<null>("DELETE", `${P}/github-apps/${enc(id)}`),
    outsideDatabases: () => req<OutsideDatabase[]>("GET", `${P}/outside-databases`),
    addOutsideDatabase: (d: { name: string; url: string; roles: [string, string] | []; group?: string; ca?: string; gateway?: string }) => req<OutsideDatabase>("POST", `${P}/outside-databases`, d),
    deleteOutsideDatabase: (id: string) => req<null>("DELETE", `${P}/outside-databases/${enc(id)}`),
    /** LDAP and Active Directory: registering one binds to it and finds its accounts, groups and base. */
    directories: () => req<Directory[]>("GET", `${P}/directories`),
    addDirectory: (d: NewDirectory) => req<Directory>("POST", `${P}/directories`, d),
    deleteDirectory: (id: string) => req<null>("DELETE", `${P}/directories/${enc(id)}`),
    /** Snowflake: registering one signs in as its admin user and finds the managed user and the roles. */
    snowflakeAccounts: () => req<SnowflakeAccount[]>("GET", `${P}/snowflake-accounts`),
    addSnowflakeAccount: (a: NewSnowflakeAccount) => req<SnowflakeAccount>("POST", `${P}/snowflake-accounts`, a),
    deleteSnowflakeAccount: (id: string) => req<null>("DELETE", `${P}/snowflake-accounts/${enc(id)}`),
    /** MongoDB Atlas: registering one takes a token as its service account and finds the managed users. */
    atlasProjects: () => req<AtlasProject[]>("GET", `${P}/atlas-projects`),
    addAtlasProject: (a: NewAtlasProject) => req<AtlasProject>("POST", `${P}/atlas-projects`, a),
    deleteAtlasProject: (id: string) => req<null>("DELETE", `${P}/atlas-projects/${enc(id)}`),
    /** Gateways into private networks: making one returns its roll token once. */
    gateways: () => req<Gateway[]>("GET", `${P}/gateways`),
    addGateway: (g: { name: string; token?: boolean }) => req<MadeGateway>("POST", `${P}/gateways`, g),
    deleteGateway: (id: string) => req<null>("DELETE", `${P}/gateways/${enc(id)}`),
    updateOutsideDatabase: (id: string, ch: { name?: string; url?: string; ca?: string }) => req<OutsideDatabase>("PATCH", `${P}/outside-databases/${enc(id)}`, ch),
    rotateOutsideAdmin: (id: string) => req<OutsideDatabase>("POST", `${P}/outside-databases/${enc(id)}/rotate-admin`),
    /** `format` other than `roll` needs the SIEM's `token` (sealed, never shown again). */
    createSink: (url: string, fromStart = false, format: SinkFormat = "roll", token?: string) =>
      req<Sink & { secret: string }>("POST", `${P}/audit/sinks`, { url, from_start: fromStart, ...(format !== "roll" && { format, token }) }),
    deleteSink: (id: string) => req<null>("DELETE", `${P}/audit/sinks/${enc(id)}`),
    /** Which candidates (tokens of some text) are this project's values the caller may reveal; never the values. */
    scanLeaks: (candidates: string[]) => req<{ index: number; key: string; environment: string; path: string }[]>("POST", `${P}/leaks`, { candidates }),
    /** `code`: also the keys no repository on the canvas mentions (a search per key: on request). */
    health: (code = false) => req<Finding[]>("GET", `${P}/health${qs({ code: code ? 1 : undefined })}`),
    /** Every managed credential: schedule, last issue, next due, last failure. */
    rotations: () => req<Rotation[]>("GET", `${P}/rotations`),
    /** Encryption keys: apps encrypt and decrypt through roll; the key never leaves it. */
    transitKeys: () => req<TransitKey[]>("GET", `${P}/transit`),
    addTransitKey: (name: string) => req<TransitKey>("POST", `${P}/transit`, { name }),
    rotateTransitKey: (name: string) => req<TransitKey>("POST", `${P}/transit/${enc(name)}/rotate`),
    /** Every version's public signing key: what a service checks `…/sign`'s signatures with. */
    transitPublicKeys: (name: string) => req<TransitPublicKey[]>("GET", `${P}/transit/${enc(name)}/public`),
    /** The project's certificate authorities (pki links). */
    cas: () => req<Ca[]>("GET", `${P}/cas`),
    addCa: (c: { name: string; domains: string[]; allow_ip?: boolean; max_days?: number }) => req<Ca>("POST", `${P}/cas`, c),
    deleteCa: (id: string) => req<null>("DELETE", `${P}/cas/${enc(id)}`),
    /** SSH CAs (ssh links): servers trust their public keys. */
    sshCas: () => req<SshCa[]>("GET", `${P}/ssh-cas`),
    addSshCa: (c: { name: string; principals: string[]; max_hours?: number; host_domains?: string[]; max_host_days?: number }) => req<SshCa>("POST", `${P}/ssh-cas`, c),
    /** Certify a person's own key (its public half) for logins the CA allows (reveal). */
    signUserKey: (id: string, b: { public_key: string; principals: string[]; ttl_minutes?: number }) => req<UserCertificate>("POST", `${P}/ssh-cas/${enc(id)}/sign-user`, b),
    /** Sign a server's host key for names under the CA's host domains (admin). */
    signHostKey: (id: string, b: { public_key: string; hosts: string[]; days?: number }) => req<HostCertificate>("POST", `${P}/ssh-cas/${enc(id)}/sign-host`, b),
    deleteSshCa: (id: string) => req<null>("DELETE", `${P}/ssh-cas/${enc(id)}`),
    /** Sign a CSR within the CA's policy: only its key and subjectAltName names are taken. */
    signCsr: (id: string, csr: string, days?: number) => req<SignedCertificate>("POST", `${P}/cas/${enc(id)}/sign`, { csr, days }),
    caCertificates: (id: string) => req<CaCertificate[]>("GET", `${P}/cas/${enc(id)}/certificates`),
    /** On the CA's CRL until it expires. */
    revokeCertificate: (id: string, serial: string) => req<null>("POST", `${P}/cas/${enc(id)}/revoke`, { serial }),
    /** ACME on the CA: its directory URL, pre-authorized names and External Account Bindings. */
    caAcme: (id: string) => req<AcmeSettings>("GET", `${P}/cas/${enc(id)}/acme`),
    setCaAcme: (id: string, preAuthorize: boolean) => req<AcmeSettings>("PATCH", `${P}/cas/${enc(id)}/acme`, { pre_authorize: preAuthorize }),
    /** A binding's HMAC key is in this answer only. */
    addAcmeBinding: (id: string, b: { label?: string; names?: string[]; days?: number }) => req<NewAcmeKey>("POST", `${P}/cas/${enc(id)}/acme/eab`, b),
    /** Every account made with the binding stops at once. */
    revokeAcmeBinding: (id: string, eab: string) => req<null>("DELETE", `${P}/cas/${enc(id)}/acme/eab/${enc(eab)}`),
    /** GCP and Azure accounts: registering one proves its trust names this project and no other. */
    cloudAccounts: () => req<CloudAccount[]>("GET", `${P}/cloud-accounts`),
    addCloudAccount: (a: NewCloudAccount) => req<CloudAccount>("POST", `${P}/cloud-accounts`, a),
    deleteCloudAccount: (id: string) => req<null>("DELETE", `${P}/cloud-accounts/${enc(id)}`),
    /** Kubernetes clusters: registering one calls the API server with the admin token first. */
    kubernetesClusters: () => req<KubernetesCluster[]>("GET", `${P}/kubernetes`),
    addKubernetesCluster: (k: NewKubernetesCluster) => req<KubernetesCluster>("POST", `${P}/kubernetes`, k),
    deleteKubernetesCluster: (id: string) => req<null>("DELETE", `${P}/kubernetes/${enc(id)}`),
    /** AWS roles for dynamic credentials: registering one assumes it (its trust policy must pin the ExternalId). */
    awsRoles: () => req<AwsRole[]>("GET", `${P}/aws-roles`),
    addAwsRole: (r: { name: string; role_arn: string; trust?: "external_id" | "oidc" }) => req<AwsRole>("POST", `${P}/aws-roles`, r),
    deleteAwsRole: (id: string) => req<null>("DELETE", `${P}/aws-roles/${enc(id)}`),
    /** IAM users whose keys roll rotates as a role: registering one lists its keys as the role. */
    awsUsers: () => req<AwsUser[]>("GET", `${P}/aws-users`),
    addAwsUser: (u: { name: string; role: string; user: string }) => req<AwsUser>("POST", `${P}/aws-users`, u),
    deleteAwsUser: (id: string) => req<null>("DELETE", `${P}/aws-users/${enc(id)}`),
    rotators: () => req<Rotator[]>("GET", `${P}/rotators`),
    /** The answer holds its signing secret, once. */
    addRotator: (r: { name: string; url: string }) => req<Rotator & { secret: string }>("POST", `${P}/rotators`, r),
    deleteRotator: (id: string) => req<null>("DELETE", `${P}/rotators/${enc(id)}`),
    updateRotator: (id: string, ch: { name?: string; url?: string }) => req<Rotator>("PATCH", `${P}/rotators/${enc(id)}`, ch),
    rotateRotatorSecret: (id: string) => req<Rotator & { secret: string }>("POST", `${P}/rotators/${enc(id)}/rotate-secret`),
    saasRotators: () => req<SaasRotator[]>("GET", `${P}/saas`),
    /** The answer never holds the credential. */
    addSaasRotator: (r: NewSaasRotator) => req<SaasRotator>("POST", `${P}/saas`, r),
    deleteSaasRotator: (id: string) => req<null>("DELETE", `${P}/saas/${enc(id)}`),
  };
}

export type SecretsApi = ReturnType<typeof secretsApi>;

export const secretsHealth = () => req<Health>("GET", "/v1/secrets/health");
export const secretsWhoami = () => req<WhoAmI>("GET", "/v1/secrets/whoami");
/** roll's OIDC issuer, which a GCP pool or an Azure federated credential trusts (public). */
export const secretsIssuer = () => req<{ issuer: string; jwks_uri: string }>("GET", "/v1/secrets/oidc/.well-known/openid-configuration");
/** Every project, environment and folder holding `key` (`PREFIX*` for a prefix) that the caller may read. */
/** Every project's findings the caller may read, projects with none left out. */
export const allFindings = () => req<ProjectFindings[]>("GET", "/v1/secrets/findings");
export const findSecret = (key: string) => req<Place[]>("GET", `/v1/secrets/search?key=${encodeURIComponent(key)}`);

/** The rule for keys (docs/secrets.md, "Secrets"). */
export const SECRET_KEY = /^[A-Za-z_][A-Za-z0-9_]{0,255}$/;
/** The rule for environment slugs. */
export const ENV_SLUG = /^[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?$/;
/** The rule for tags. */
export const TAG = /^[a-z0-9-]{1,32}$/;
/** The rule for folder paths: `/`, or 1–8 segments of `[A-Za-z0-9_-]` (docs/secrets.md, "Folders"). */
export const FOLDER_PATH = /^\/$|^(?:\/[A-Za-z0-9_-]{1,64}){1,8}$/;
/** The rule for label keys and values. */
export const LABEL_PART = /^[a-z0-9_.-]{1,64}$/;

/**
 * Link kinds whose live links can rotate the resource's own password: the providers that implement
 * `rotate_source` in crates/rolld/src/secrets.rs. Keep the two together.
 */
const ROTATES_SOURCE: ReadonlySet<string> = new Set(["redis", "nosql"]);

/** Whether rotating a linked secret can work: a managed credential always, a live link only where the resource rotates, a dynamic one never (each resolve issues a fresh one). */
export function canRotate(link: Link): boolean {
  return link.mode === "managed" || (link.mode === "live" && ROTATES_SOURCE.has(link.kind));
}
