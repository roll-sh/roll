/**
 * roll storage IAM through the control plane: `POST /v1/storage/iam {"Action": …, ...params}`.
 * Parameters are AWS IAM's (arrays and objects are expanded to `Name.member.N` / `Name.Field` by the
 * server). Answers are the action's Result element as JSON: lists of `<member>` are arrays, an empty
 * element is `null` (so an empty list is `null`: read lists through `members`), and leaves are strings
 * (`"IsTruncated": "false"`, `"MaxSessionDuration": "3600"`). Policy documents are JSON text.
 *
 * In operator mode the calls act as the selected account's root user; with a console session, as
 * the signed-in IAM user (and IAM's AccessDenied when their policies don't allow it).
 */

import { storageCall, StorageError } from "./storage-session";

// --- Calls -------------------------------------------------------------------------------------

export type IamValue = string | number | boolean | null | undefined | IamValue[] | { [k: string]: IamValue };
export type IamParams = Record<string, IamValue>;

/** The params as sent: `undefined`, `null`, `""` and `[]` left out (IAM treats them as absent). */
export function iamBody(action: string, params: IamParams = {}): Record<string, IamValue> {
  const out: Record<string, IamValue> = { Action: action };
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null || v === "") continue;
    if (Array.isArray(v) && v.length === 0) continue;
    out[k] = v;
  }
  return out;
}

/** One IAM action; rejects with a `StorageError` carrying IAM's code (`NoSuchEntity`, `MalformedPolicyDocument`, …). */
export function iam<T = Record<string, unknown>>(action: string, params: IamParams = {}): Promise<T> {
  return storageCall<T>("POST", "/v1/storage/iam", iamBody(action, params));
}

/**
 * A lookup of something that may not exist (a login profile, the password policy): `null` instead
 * of IAM's `NoSuchEntity`, asked for with `?missing=null` so the browser doesn't log a 404.
 */
export function iamMaybe<T = Record<string, unknown>>(action: string, params: IamParams = {}): Promise<T | null> {
  return storageCall<T | null>("POST", "/v1/storage/iam?missing=null", iamBody(action, params));
}

/** Every page of a list action (`Marker` / `IsTruncated`), the `key` lists joined. */
export async function iamAll<T>(action: string, key: string, params: IamParams = {}): Promise<T[]> {
  const out: T[] = [];
  let marker: string | undefined;
  for (let i = 0; i < 100; i++) {
    const r = await iam<Record<string, unknown>>(action, { ...params, MaxItems: 1000, Marker: marker });
    out.push(...members<T>(r?.[key]));
    if (!bool(r?.IsTruncated) || typeof r?.Marker !== "string" || !r.Marker) break;
    marker = r.Marker;
  }
  return out;
}

/** A list from IAM's JSON: `null` (empty), one object, or an array. */
export function members<T>(x: unknown): T[] {
  if (x == null || x === "") return [];
  return Array.isArray(x) ? (x as T[]) : [x as T];
}

/** IAM's booleans are strings. */
export const bool = (x: unknown) => x === true || x === "true";

/** IAM's numbers are strings; `null` when absent or not a number. */
export function num(x: unknown): number | null {
  if (x == null || x === "") return null;
  const n = Number(x);
  return Number.isFinite(n) ? n : null;
}

/** An IAM date (ISO 8601) as Unix ms, or `null`. */
export function iamDate(x: unknown): number | null {
  if (typeof x !== "string" || !x) return null;
  const t = Date.parse(x);
  return Number.isFinite(t) ? t : null;
}

export const isNoSuchEntity = (e: unknown) => e instanceof StorageError && e.code === "NoSuchEntity";

// --- Types (IAM's field names; leaves are strings) ---------------------------------------------

type S = string;
type Opt = string | null | undefined;

export interface IamTag {
  Key: S;
  Value: S;
}

export interface IamUser {
  UserName: S;
  UserId: S;
  Arn: S;
  Path: S;
  CreateDate: S;
  PasswordLastUsed?: Opt;
}

export interface IamGroup {
  GroupName: S;
  GroupId: S;
  Arn: S;
  Path: S;
  CreateDate: S;
}

export interface IamRole {
  RoleName: S;
  RoleId: S;
  Arn: S;
  Path: S;
  CreateDate: S;
  Description?: Opt;
  MaxSessionDuration?: Opt;
  /** JSON text. */
  AssumeRolePolicyDocument?: Opt;
}

export interface ManagedPolicy {
  PolicyName: S;
  PolicyId: S;
  Arn: S;
  Path: S;
  DefaultVersionId: S;
  AttachmentCount?: Opt;
  IsAttachable?: Opt;
  CreateDate: S;
  UpdateDate?: Opt;
  Description?: Opt;
}

export interface AttachedPolicy {
  PolicyName: S;
  PolicyArn: S;
}

export interface PolicyVersion {
  VersionId: S;
  IsDefaultVersion: S;
  CreateDate: S;
  /** JSON text (GetPolicyVersion). */
  Document?: Opt;
}

export interface AccessKeyMeta {
  UserName: S;
  AccessKeyId: S;
  Status: "Active" | "Inactive";
  CreateDate: S;
}

export interface NewAccessKey extends AccessKeyMeta {
  SecretAccessKey: S;
}

export interface AccessKeyLastUsed {
  LastUsedDate?: Opt;
  ServiceName?: Opt;
  Region?: Opt;
}

export interface MfaDevice {
  UserName?: Opt;
  SerialNumber: S;
  EnableDate?: Opt;
}

export interface VirtualMfaDevice {
  SerialNumber: S;
  /** Base64 of the base32 seed text. */
  Base32StringSeed?: Opt;
  /** Base64 PNG. */
  QRCodePNG?: Opt;
  User?: IamUser | null;
  EnableDate?: Opt;
}

export interface LoginProfile {
  UserName: S;
  CreateDate: S;
  PasswordResetRequired?: Opt;
}

export interface PasswordPolicy {
  MinimumPasswordLength?: Opt;
  RequireSymbols?: Opt;
  RequireNumbers?: Opt;
  RequireUppercaseCharacters?: Opt;
  RequireLowercaseCharacters?: Opt;
  AllowUsersToChangePassword?: Opt;
  ExpirePasswords?: Opt;
  MaxPasswordAge?: Opt;
  PasswordReusePrevention?: Opt;
  HardExpiry?: Opt;
}

export type Decision = "allowed" | "explicitDeny" | "implicitDeny";

export interface StatementPosition {
  Line: S;
  Column: S;
}

export interface MatchedStatement {
  SourcePolicyId?: Opt;
  SourcePolicyType?: Opt;
  StartPosition?: StatementPosition | null;
  EndPosition?: StatementPosition | null;
}

export interface EvaluationResult {
  EvalActionName: S;
  EvalResourceName?: Opt;
  EvalDecision: Decision | S;
  MatchedStatements?: MatchedStatement[] | MatchedStatement | null;
  MissingContextValues?: S[] | S | null;
  EvalDecisionDetails?: Record<string, S> | null;
  PermissionsBoundaryDecisionDetail?: { AllowedByPermissionsBoundary?: Opt } | null;
  ResourceSpecificResults?: unknown;
}

// --- The actions the dashboard uses ------------------------------------------------------------

const tagList = (tags: IamTag[]) => tags.map((t) => ({ Key: t.Key, Value: t.Value }));

export type PolicyTarget = { kind: "user"; name: string } | { kind: "group"; name: string } | { kind: "role"; name: string };
const T = { user: "User", group: "Group", role: "Role" } as const;
const nameParam = (t: PolicyTarget): IamParams => ({ [`${T[t.kind]}Name`]: t.name });

export const iamApi = {
  // Users
  listUsers: () => iamAll<IamUser>("ListUsers", "Users"),
  getUser: (UserName: string) => iam<{ User: IamUser & { Tags?: IamTag[] | null } }>("GetUser", { UserName }).then((r) => r.User),
  createUser: (UserName: string, opts: { Path?: string; Tags?: IamTag[] } = {}) =>
    iam<{ User: IamUser }>("CreateUser", { UserName, Path: opts.Path, Tags: opts.Tags ? tagList(opts.Tags) : undefined }),
  deleteUser: (UserName: string) => iam("DeleteUser", { UserName }),

  // Tags (users and roles)
  listTags: (t: Exclude<PolicyTarget, { kind: "group" }>) =>
    iam<{ Tags: unknown }>(`List${T[t.kind]}Tags`, nameParam(t)).then((r) => members<IamTag>(r.Tags)),
  tag: (t: Exclude<PolicyTarget, { kind: "group" }>, tags: IamTag[]) => iam(`Tag${T[t.kind]}`, { ...nameParam(t), Tags: tagList(tags) }),
  untag: (t: Exclude<PolicyTarget, { kind: "group" }>, keys: string[]) => iam(`Untag${T[t.kind]}`, { ...nameParam(t), TagKeys: keys }),

  // Access keys
  listAccessKeys: (UserName: string) => iamAll<AccessKeyMeta>("ListAccessKeys", "AccessKeyMetadata", { UserName }),
  createAccessKey: (UserName: string) => iam<{ AccessKey: NewAccessKey }>("CreateAccessKey", { UserName }).then((r) => r.AccessKey),
  updateAccessKey: (UserName: string, AccessKeyId: string, Status: "Active" | "Inactive") =>
    iam("UpdateAccessKey", { UserName, AccessKeyId, Status }),
  deleteAccessKey: (UserName: string, AccessKeyId: string) => iam("DeleteAccessKey", { UserName, AccessKeyId }),
  accessKeyLastUsed: (AccessKeyId: string) =>
    iam<{ UserName?: string; AccessKeyLastUsed?: AccessKeyLastUsed | null }>("GetAccessKeyLastUsed", { AccessKeyId }).then(
      (r) => r.AccessKeyLastUsed ?? null,
    ),

  // Groups
  listGroups: () => iamAll<IamGroup>("ListGroups", "Groups"),
  getGroup: (GroupName: string) =>
    iam<{ Group: IamGroup; Users: unknown }>("GetGroup", { GroupName, MaxItems: 1000 }).then((r) => ({
      group: r.Group,
      users: members<IamUser>(r.Users),
    })),
  createGroup: (GroupName: string, Path?: string) => iam<{ Group: IamGroup }>("CreateGroup", { GroupName, Path }),
  deleteGroup: (GroupName: string) => iam("DeleteGroup", { GroupName }),
  addUserToGroup: (GroupName: string, UserName: string) => iam("AddUserToGroup", { GroupName, UserName }),
  removeUserFromGroup: (GroupName: string, UserName: string) => iam("RemoveUserFromGroup", { GroupName, UserName }),
  groupsForUser: (UserName: string) => iamAll<IamGroup>("ListGroupsForUser", "Groups", { UserName }),

  // Inline policies (users, groups, roles)
  listInline: (t: PolicyTarget) => iamAll<string>(`List${T[t.kind]}Policies`, "PolicyNames", nameParam(t)),
  getInline: (t: PolicyTarget, PolicyName: string) =>
    iam<{ PolicyDocument: string }>(`Get${T[t.kind]}Policy`, { ...nameParam(t), PolicyName }).then((r) => r.PolicyDocument ?? ""),
  putInline: (t: PolicyTarget, PolicyName: string, PolicyDocument: string) =>
    iam(`Put${T[t.kind]}Policy`, { ...nameParam(t), PolicyName, PolicyDocument }),
  deleteInline: (t: PolicyTarget, PolicyName: string) => iam(`Delete${T[t.kind]}Policy`, { ...nameParam(t), PolicyName }),

  // Managed policies and attachments
  listPolicies: (Scope: "All" | "AWS" | "Local" = "All", OnlyAttached = false) =>
    iamAll<ManagedPolicy>("ListPolicies", "Policies", { Scope, OnlyAttached: OnlyAttached || undefined }),
  getPolicy: (PolicyArn: string) => iam<{ Policy: ManagedPolicy }>("GetPolicy", { PolicyArn }).then((r) => r.Policy),
  createPolicy: (p: { PolicyName: string; PolicyDocument: string; Description?: string; Path?: string }) =>
    iam<{ Policy: ManagedPolicy }>("CreatePolicy", p).then((r) => r.Policy),
  deletePolicy: (PolicyArn: string) => iam("DeletePolicy", { PolicyArn }),
  createPolicyVersion: (PolicyArn: string, PolicyDocument: string, SetAsDefault: boolean) =>
    iam<{ PolicyVersion: PolicyVersion }>("CreatePolicyVersion", { PolicyArn, PolicyDocument, SetAsDefault }).then((r) => r.PolicyVersion),
  getPolicyVersion: (PolicyArn: string, VersionId: string) =>
    iam<{ PolicyVersion: PolicyVersion }>("GetPolicyVersion", { PolicyArn, VersionId }).then((r) => r.PolicyVersion),
  listPolicyVersions: (PolicyArn: string) => iamAll<PolicyVersion>("ListPolicyVersions", "Versions", { PolicyArn }),
  setDefaultPolicyVersion: (PolicyArn: string, VersionId: string) => iam("SetDefaultPolicyVersion", { PolicyArn, VersionId }),
  deletePolicyVersion: (PolicyArn: string, VersionId: string) => iam("DeletePolicyVersion", { PolicyArn, VersionId }),
  entitiesForPolicy: (PolicyArn: string) =>
    iam<{ PolicyGroups: unknown; PolicyUsers: unknown; PolicyRoles: unknown }>("ListEntitiesForPolicy", { PolicyArn, MaxItems: 1000 }).then((r) => ({
      groups: members<{ GroupName: string }>(r.PolicyGroups).map((g) => g.GroupName),
      users: members<{ UserName: string }>(r.PolicyUsers).map((u) => u.UserName),
      roles: members<{ RoleName: string }>(r.PolicyRoles).map((x) => x.RoleName),
    })),
  listAttached: (t: PolicyTarget) => iamAll<AttachedPolicy>(`ListAttached${T[t.kind]}Policies`, "AttachedPolicies", nameParam(t)),
  attach: (t: PolicyTarget, PolicyArn: string) => iam(`Attach${T[t.kind]}Policy`, { ...nameParam(t), PolicyArn }),
  detach: (t: PolicyTarget, PolicyArn: string) => iam(`Detach${T[t.kind]}Policy`, { ...nameParam(t), PolicyArn }),

  // Roles
  listRoles: () => iamAll<IamRole>("ListRoles", "Roles"),
  getRole: (RoleName: string) => iam<{ Role: IamRole }>("GetRole", { RoleName }).then((r) => r.Role),
  createRole: (p: { RoleName: string; AssumeRolePolicyDocument: string; Description?: string; MaxSessionDuration?: number; Path?: string }) =>
    iam<{ Role: IamRole }>("CreateRole", p).then((r) => r.Role),
  updateRole: (RoleName: string, p: { Description?: string; MaxSessionDuration?: number }) => iam("UpdateRole", { RoleName, ...p }),
  updateAssumeRolePolicy: (RoleName: string, PolicyDocument: string) => iam("UpdateAssumeRolePolicy", { RoleName, PolicyDocument }),
  deleteRole: (RoleName: string) => iam("DeleteRole", { RoleName }),

  // MFA
  listMfaDevices: (UserName: string) => iamAll<MfaDevice>("ListMFADevices", "MFADevices", { UserName }),
  listVirtualMfaDevices: (AssignmentStatus: "Assigned" | "Unassigned" | "Any" = "Any") =>
    iamAll<VirtualMfaDevice>("ListVirtualMFADevices", "VirtualMFADevices", { AssignmentStatus }),
  createVirtualMfaDevice: (VirtualMFADeviceName: string) =>
    iam<{ VirtualMFADevice: VirtualMfaDevice }>("CreateVirtualMFADevice", { VirtualMFADeviceName }).then((r) => r.VirtualMFADevice),
  enableMfaDevice: (UserName: string, SerialNumber: string, AuthenticationCode1: string, AuthenticationCode2: string) =>
    iam("EnableMFADevice", { UserName, SerialNumber, AuthenticationCode1, AuthenticationCode2 }),
  resyncMfaDevice: (UserName: string, SerialNumber: string, AuthenticationCode1: string, AuthenticationCode2: string) =>
    iam("ResyncMFADevice", { UserName, SerialNumber, AuthenticationCode1, AuthenticationCode2 }),
  deactivateMfaDevice: (UserName: string, SerialNumber: string) => iam("DeactivateMFADevice", { UserName, SerialNumber }),
  deleteVirtualMfaDevice: (SerialNumber: string) => iam("DeleteVirtualMFADevice", { SerialNumber }),

  // Console passwords
  /** `null` when the user has no password. */
  getLoginProfile: (UserName: string) =>
    iamMaybe<{ LoginProfile: LoginProfile }>("GetLoginProfile", { UserName }).then(
      (r) => r?.LoginProfile ?? null,
      (e) => {
        if (isNoSuchEntity(e)) return null;
        throw e;
      },
    ),
  createLoginProfile: (UserName: string, Password: string, PasswordResetRequired: boolean) =>
    iam("CreateLoginProfile", { UserName, Password, PasswordResetRequired }),
  updateLoginProfile: (UserName: string, p: { Password?: string; PasswordResetRequired?: boolean }) =>
    iam("UpdateLoginProfile", { UserName, ...p }),
  deleteLoginProfile: (UserName: string) => iam("DeleteLoginProfile", { UserName }),
  changePassword: (OldPassword: string, NewPassword: string) => iam("ChangePassword", { OldPassword, NewPassword }),

  // Account
  /** `null` when the account has none (IAM's default rules apply). */
  getPasswordPolicy: () =>
    iamMaybe<{ PasswordPolicy: PasswordPolicy }>("GetAccountPasswordPolicy").then(
      (r) => r?.PasswordPolicy ?? null,
      (e) => {
        if (isNoSuchEntity(e)) return null;
        throw e;
      },
    ),
  updatePasswordPolicy: (p: IamParams) => iam("UpdateAccountPasswordPolicy", p),
  deletePasswordPolicy: () => iam("DeleteAccountPasswordPolicy"),
  listAliases: () => iam<{ AccountAliases: unknown }>("ListAccountAliases").then((r) => members<string>(r.AccountAliases)),
  createAlias: (AccountAlias: string) => iam("CreateAccountAlias", { AccountAlias }),
  deleteAlias: (AccountAlias: string) => iam("DeleteAccountAlias", { AccountAlias }),
  summary: () => iam<{ SummaryMap: Record<string, string> | null }>("GetAccountSummary").then((r) => r.SummaryMap ?? {}),
  generateCredentialReport: () => iam<{ State: string; Description?: string }>("GenerateCredentialReport"),
  getCredentialReport: () => iam<{ Content: string; ReportFormat?: string; GeneratedTime?: string }>("GetCredentialReport"),

  // Simulation
  simulate: (req: { action: SimulationAction; params: IamParams }) =>
    iam<{ EvaluationResults: unknown; IsTruncated?: string }>(req.action, { ...req.params, MaxItems: 1000 }).then((r) =>
      members<EvaluationResult>(r.EvaluationResults),
    ),
};

// --- Names, tags, paths ------------------------------------------------------------------------

const NAME_MAX = { user: 64, group: 128, role: 64, policy: 128, "mfa device": 226 } as const;

/** IAM's name rules: letters, digits and `+=,.@_-`, within the kind's length. `null` when fine. */
export function iamNameError(kind: keyof typeof NAME_MAX, name: string): string | null {
  const max = NAME_MAX[kind];
  if (!name) return `Name the ${kind}.`;
  if (name.length > max) return `At most ${max} characters.`;
  if (!/^[\w+=,.@-]+$/.test(name)) return "Use letters, digits and + = , . @ _ - only.";
  return null;
}

/** IAM paths: `/`, or `/` + segments + `/`, printable ASCII. `null` when fine (empty: the default `/`). */
export function iamPathError(path: string): string | null {
  if (!path) return null;
  if (path.length > 512) return "At most 512 characters.";
  if (!/^\/([\x21-\x7E]*\/)?$/.test(path)) return "A path starts and ends with /, e.g. /engineering/.";
  return null;
}

/** IAM's tag rules: at most 50, keys 1–128 and unique (ignoring case), values up to 256, no `aws:`. */
export function iamTagsError(tags: IamTag[]): string | null {
  if (tags.length > 50) return "At most 50 tags.";
  const chars = /^[\p{L}\p{Z}\p{N}_.:/=+\-@]*$/u;
  const seen = new Set<string>();
  for (const { Key: k, Value: v } of tags) {
    if (!k) return "Every tag needs a key.";
    if ([...k].length > 128) return "Tag keys are at most 128 characters.";
    if ([...v].length > 256) return `Tag values are at most 256 characters (${k}).`;
    if (!chars.test(k) || !chars.test(v)) return "Tags use letters, digits, spaces and _ . : / = + - @ only.";
    if (k.toLowerCase().startsWith("aws:")) return "Keys starting with aws: are reserved.";
    if (seen.has(k.toLowerCase())) return `Duplicate key ${k}.`;
    seen.add(k.toLowerCase());
  }
  return null;
}

/** Whether a policy ARN is one of AWS's managed policies (read-only). */
export const isAwsManaged = (arn: string) => /^arn:[^:]+:iam::aws:policy\//.test(arn);

/** The last part of an ARN's resource (`arn:aws:iam::1:user/a/b` → `b`). */
export function arnName(arn: string) {
  const resource = arn.split(":").slice(5).join(":");
  return resource.slice(resource.lastIndexOf("/") + 1);
}

/** A user/group/role ARN in the account. */
export function principalArn(accountId: string, kind: "user" | "group" | "role", name: string, path = "/") {
  return `arn:aws:iam::${accountId}:${kind}${path || "/"}${name}`;
}

// --- Policy documents --------------------------------------------------------------------------

/**
 * An IAM policy as typed: `null` when it's a JSON object with a Statement (IAM checks the rest and
 * answers MalformedPolicyDocument), else what's wrong. Empty text is an error: a document is required.
 */
export function policyDocumentError(text: string): string | null {
  if (!text.trim()) return "Write the policy document.";
  if (text.length > 131072) return "A policy document is at most 128 KB.";
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch (e) {
    return `Not valid JSON: ${e instanceof Error ? e.message : String(e)}`;
  }
  if (typeof v !== "object" || v === null || Array.isArray(v)) return "A policy is a JSON object.";
  if (!("Statement" in v)) return "A policy needs a Statement.";
  const st = (v as { Statement: unknown }).Statement;
  if (typeof st !== "object" || st === null) return "Statement is an object or a list of them.";
  return null;
}

/** A policy document (JSON text or an object) indented for the editor; text that isn't JSON is kept. */
export function prettyPolicy(doc: string | object | null | undefined): string {
  if (doc == null) return "";
  if (typeof doc !== "string") return JSON.stringify(doc, null, 2);
  try {
    return JSON.stringify(JSON.parse(doc), null, 2);
  } catch {
    return doc;
  }
}

/** A starting identity policy: read one bucket. */
export function starterPolicy(bucket = "my-bucket"): string {
  return prettyPolicy({
    Version: "2012-10-17",
    Statement: [
      { Effect: "Allow", Action: ["s3:ListBucket"], Resource: `arn:aws:s3:::${bucket}` },
      { Effect: "Allow", Action: ["s3:GetObject"], Resource: `arn:aws:s3:::${bucket}/*` },
    ],
  });
}

/** A starting trust policy: principals of this account may assume the role. */
export function starterTrustPolicy(accountId: string): string {
  return prettyPolicy({
    Version: "2012-10-17",
    Statement: [{ Effect: "Allow", Principal: { AWS: `arn:aws:iam::${accountId}:root` }, Action: "sts:AssumeRole" }],
  });
}

// --- Passwords ---------------------------------------------------------------------------------

/** The symbols IAM password policies count. */
export const PASSWORD_SYMBOLS = "!@#$%^&*()_+-=[]{}|'";

/**
 * What a password misses, under the account's policy (or IAM's default rules without one: 8–128
 * characters with three of uppercase, lowercase, digits and symbols). The server checks again.
 */
export function passwordProblems(pw: string, policy: PasswordPolicy | null): string[] {
  const out: string[] = [];
  const has = {
    upper: /[A-Z]/.test(pw),
    lower: /[a-z]/.test(pw),
    digit: /[0-9]/.test(pw),
    symbol: [...pw].some((c) => PASSWORD_SYMBOLS.includes(c)),
  };
  if (pw.length > 128) out.push("at most 128 characters");
  if (!policy) {
    if (pw.length < 8) out.push("at least 8 characters");
    if (Object.values(has).filter(Boolean).length < 3) out.push("three of: uppercase, lowercase, digits, symbols");
    return out;
  }
  const min = num(policy.MinimumPasswordLength) ?? 6;
  if (pw.length < min) out.push(`at least ${min} characters`);
  if (bool(policy.RequireUppercaseCharacters) && !has.upper) out.push("an uppercase letter");
  if (bool(policy.RequireLowercaseCharacters) && !has.lower) out.push("a lowercase letter");
  if (bool(policy.RequireNumbers) && !has.digit) out.push("a digit");
  if (bool(policy.RequireSymbols) && !has.symbol) out.push(`a symbol (${PASSWORD_SYMBOLS})`);
  return out;
}

/**
 * A random password that meets the policy (and IAM's default rules): every character class, at
 * least 20 characters (or the policy's minimum). `random` is for tests.
 */
export function generatePassword(policy: PasswordPolicy | null, random: (n: number) => number = randomBelow): string {
  const sets = ["ABCDEFGHJKLMNPQRSTUVWXYZ", "abcdefghijkmnopqrstuvwxyz", "23456789", PASSWORD_SYMBOLS];
  const length = Math.min(128, Math.max(20, num(policy?.MinimumPasswordLength) ?? 0));
  const all = sets.join("");
  const chars = sets.map((s) => s[random(s.length)]);
  while (chars.length < length) chars.push(all[random(all.length)]);
  // Shuffle so the guaranteed characters aren't always first.
  for (let i = chars.length - 1; i > 0; i--) {
    const j = random(i + 1);
    [chars[i], chars[j]] = [chars[j], chars[i]];
  }
  return chars.join("");
}

/** A uniform integer in [0, n) from the platform's CSPRNG. */
function randomBelow(n: number): number {
  const limit = Math.floor(0x1_0000_0000 / n) * n;
  const buf = new Uint32Array(1);
  for (;;) {
    crypto.getRandomValues(buf);
    if (buf[0] < limit) return buf[0] % n;
  }
}

/** The password policy form. */
export interface PasswordPolicyDraft {
  min: number;
  symbols: boolean;
  numbers: boolean;
  upper: boolean;
  lower: boolean;
  allowChange: boolean;
  /** Days; 0: passwords don't expire. */
  maxAge: number;
  /** Remembered passwords; 0: off. */
  reuse: number;
  hardExpiry: boolean;
}

/** The form for a policy (`null`: IAM's defaults when one is first set). */
export function passwordPolicyDraft(p: PasswordPolicy | null): PasswordPolicyDraft {
  return {
    min: num(p?.MinimumPasswordLength) ?? 8,
    symbols: bool(p?.RequireSymbols),
    numbers: bool(p?.RequireNumbers),
    upper: bool(p?.RequireUppercaseCharacters),
    lower: bool(p?.RequireLowercaseCharacters),
    allowChange: p ? bool(p.AllowUsersToChangePassword) : true,
    maxAge: num(p?.MaxPasswordAge) ?? 0,
    reuse: num(p?.PasswordReusePrevention) ?? 0,
    hardExpiry: bool(p?.HardExpiry),
  };
}

/** IAM's limits on the policy (1–1095 days, 1–24 remembered, 6–128 characters). */
export function passwordPolicyError(d: PasswordPolicyDraft): string | null {
  if (!Number.isInteger(d.min) || d.min < 6 || d.min > 128) return "Minimum length is 6–128.";
  if (!Number.isInteger(d.maxAge) || d.maxAge < 0 || d.maxAge > 1095) return "Expiry is 1–1095 days (0: never).";
  if (!Number.isInteger(d.reuse) || d.reuse < 0 || d.reuse > 24) return "Remember 1–24 passwords (0: off).";
  return null;
}

/** UpdateAccountPasswordPolicy's params for a draft (left-out fields are IAM's defaults: off). */
export function passwordPolicyParams(d: PasswordPolicyDraft): IamParams {
  return {
    MinimumPasswordLength: d.min,
    RequireSymbols: d.symbols,
    RequireNumbers: d.numbers,
    RequireUppercaseCharacters: d.upper,
    RequireLowercaseCharacters: d.lower,
    AllowUsersToChangePassword: d.allowChange,
    MaxPasswordAge: d.maxAge > 0 ? d.maxAge : undefined,
    PasswordReusePrevention: d.reuse > 0 ? d.reuse : undefined,
    HardExpiry: d.maxAge > 0 ? d.hardExpiry : undefined,
  };
}

// --- MFA ---------------------------------------------------------------------------------------

/** Base64 to text (UTF-8). */
export function decodeBase64Text(b64: string): string {
  const bin = atob(b64.replace(/\s+/g, ""));
  const bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0));
  return new TextDecoder().decode(bytes);
}

/** A virtual device's seed as an authenticator app takes it: the base32 text, in groups of four. */
export function mfaSeed(base32StringSeed: string): string {
  return decodeBase64Text(base32StringSeed)
    .trim()
    .replace(/(.{4})(?=.)/g, "$1 ");
}

/** The QR code (base64 PNG) as an `<img src>`. */
export const qrDataUrl = (pngBase64: string) => `data:image/png;base64,${pngBase64.replace(/\s+/g, "")}`;

/** Two consecutive codes for EnableMFADevice / ResyncMFADevice: six digits each, not the same. */
export function mfaCodesError(a: string, b: string): string | null {
  if (!/^\d{6}$/.test(a) || !/^\d{6}$/.test(b)) return "Enter two 6-digit codes.";
  if (a === b) return "Wait for the next code: the two must be consecutive.";
  return null;
}

/** A 6-digit sign-in code, as typed (spaces dropped). */
export const cleanCode = (v: string) => v.replace(/\D/g, "").slice(0, 6);

// --- Credential report -------------------------------------------------------------------------

/** RFC 4180 CSV: quoted fields, doubled quotes, CRLF or LF. */
export function parseCsv(text: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = "";
  let quoted = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quoted) {
      if (c === '"') {
        if (text[i + 1] === '"') {
          field += '"';
          i++;
        } else quoted = false;
      } else field += c;
    } else if (c === '"') quoted = true;
    else if (c === ",") {
      row.push(field);
      field = "";
    } else if (c === "\n" || c === "\r") {
      if (c === "\r" && text[i + 1] === "\n") i++;
      row.push(field);
      rows.push(row);
      row = [];
      field = "";
    } else field += c;
  }
  if (field !== "" || row.length) {
    row.push(field);
    rows.push(row);
  }
  return rows;
}

export interface CredentialReport {
  columns: string[];
  rows: Record<string, string>[];
}

/** GetCredentialReport's `Content` (base64 CSV) as rows keyed by column. */
export function credentialReport(contentBase64: string): CredentialReport {
  const [columns = [], ...lines] = parseCsv(decodeBase64Text(contentBase64)).filter((r) => r.some((f) => f !== ""));
  return { columns, rows: lines.map((l) => Object.fromEntries(columns.map((c, i) => [c, l[i] ?? ""]))) };
}

/** A credential report cell as it reads: `N/A`, `not_supported` and `no_information` are a dash. */
export function reportCell(v: string | undefined): string {
  if (!v || v === "N/A" || v === "not_supported" || v === "no_information") return "—";
  return v;
}

// --- Policy simulator --------------------------------------------------------------------------

export type SimulationAction = "SimulatePrincipalPolicy" | "SimulateCustomPolicy";

/** IAM's context key types. */
export const CONTEXT_TYPES = [
  "string",
  "stringList",
  "numeric",
  "numericList",
  "boolean",
  "booleanList",
  "ip",
  "ipList",
  "binary",
  "binaryList",
  "date",
  "dateList",
] as const;
export type ContextType = (typeof CONTEXT_TYPES)[number];

export interface ContextDraft {
  key: string;
  type: ContextType;
  /** Comma-separated for list types. */
  values: string;
}

export interface SimulationDraft {
  mode: "principal" | "custom";
  /** A user, group or role ARN (principal mode). */
  sourceArn: string;
  /** Policy documents (custom mode: required; principal mode: added to the principal's). */
  policies: string[];
  /** Permissions boundary documents. */
  boundaries: string[];
  /** Action names, separated by commas, spaces or lines. */
  actions: string;
  resources: string;
  context: ContextDraft[];
  resourcePolicy: string;
  callerArn: string;
}

/** Words separated by commas, whitespace or lines. */
export function splitList(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** What's missing before a simulation can run (`null`: ready). */
export function simulationError(d: SimulationDraft): string | null {
  if (d.mode === "principal" && !d.sourceArn.trim()) return "Choose a user, group or role.";
  const policies = d.policies.filter((p) => p.trim());
  if (d.mode === "custom" && policies.length === 0) return "Write a policy to simulate.";
  for (const p of [...policies, ...d.boundaries.filter((b) => b.trim())]) {
    const e = policyDocumentError(p);
    if (e) return e;
  }
  if (d.resourcePolicy.trim()) {
    const e = policyDocumentError(d.resourcePolicy);
    if (e) return `Resource policy: ${e}`;
  }
  if (splitList(d.actions).length === 0) return "Name at least one action, e.g. s3:GetObject.";
  for (const c of d.context) {
    if (!c.key.trim() && !c.values.trim()) continue;
    if (!c.key.trim()) return "Every context entry needs a key, e.g. aws:SourceIp.";
    if (!c.values.trim()) return `Give ${c.key.trim()} a value.`;
  }
  return null;
}

/** The context entries as IAM's `ContextEntries` (empty rows skipped; list types split on commas). */
export function contextEntries(context: ContextDraft[]) {
  return context
    .filter((c) => c.key.trim() && c.values.trim())
    .map((c) => ({
      ContextKeyName: c.key.trim(),
      ContextKeyType: c.type,
      ContextKeyValues: c.type.endsWith("List")
        ? c.values
            .split(",")
            .map((v) => v.trim())
            .filter(Boolean)
        : [c.values.trim()],
    }));
}

/** The action and params for a simulation (documents sent as typed, so positions match the editor). */
export function simulationRequest(d: SimulationDraft): { action: SimulationAction; params: IamParams } {
  const policies = d.policies.filter((p) => p.trim());
  const boundaries = d.boundaries.filter((p) => p.trim());
  const common: IamParams = {
    ActionNames: splitList(d.actions),
    ResourceArns: splitList(d.resources),
    ContextEntries: contextEntries(d.context),
    ResourcePolicy: d.resourcePolicy.trim() ? d.resourcePolicy : undefined,
    CallerArn: d.callerArn.trim() || undefined,
    PermissionsBoundaryPolicyInputList: boundaries,
  };
  if (d.mode === "principal") {
    return { action: "SimulatePrincipalPolicy", params: { PolicySourceArn: d.sourceArn.trim(), PolicyInputList: policies, ...common } };
  }
  return { action: "SimulateCustomPolicy", params: { PolicyInputList: policies, ...common } };
}

/** How a decision reads, and its tone. */
export function decisionLabel(d: string): { label: string; tone: "ok" | "danger" | "muted" } {
  switch (d) {
    case "allowed":
      return { label: "allowed", tone: "ok" };
    case "explicitDeny":
      return { label: "denied by a statement", tone: "danger" };
    case "implicitDeny":
      return { label: "denied: nothing allows it", tone: "muted" };
    default:
      return { label: d, tone: "muted" };
  }
}

/** A matched statement's place in its policy, e.g. `3:5–9:6` (line:column). */
export function statementRange(m: MatchedStatement): string | null {
  const p = (x: StatementPosition | null | undefined) => (x && x.Line ? `${x.Line}:${x.Column}` : null);
  const a = p(m.StartPosition);
  const b = p(m.EndPosition);
  if (!a) return null;
  return b ? `${a}–${b}` : a;
}

/** The lines of a document a matched statement spans (1-based, inclusive), for highlighting. */
export function statementLines(m: MatchedStatement): [number, number] | null {
  const a = num(m.StartPosition?.Line);
  if (a == null) return null;
  return [a, num(m.EndPosition?.Line) ?? a];
}

/**
 * The document a matched statement is in, when the dashboard sent it: `PolicyInputList.N` or
 * `PermissionsBoundaryPolicyInputList.N` ids (1-based, as IAM names them), or the resource policy.
 */
export function sourceDocument(m: MatchedStatement, d: SimulationDraft): string | null {
  const id = m.SourcePolicyId ?? "";
  const n = (prefix: string) => {
    const match = new RegExp(`^${prefix}\\.(\\d+)$`).exec(id);
    return match ? Number(match[1]) - 1 : null;
  };
  const i = n("PolicyInputList");
  if (i != null) return d.policies.filter((p) => p.trim())[i] ?? null;
  const j = n("PermissionsBoundaryPolicyInputList");
  if (j != null) return d.boundaries.filter((p) => p.trim())[j] ?? null;
  if (m.SourcePolicyType === "resource" || id === "ResourcePolicy") return d.resourcePolicy || null;
  return null;
}

/** GetAccountSummary's counters worth showing, with their quotas. */
export function summaryStats(map: Record<string, string>) {
  const n = (k: string) => num(map[k]) ?? 0;
  return {
    users: n("Users"),
    usersQuota: num(map.UsersQuota),
    groups: n("Groups"),
    roles: n("Roles"),
    policies: n("Policies"),
    mfaDevices: n("MFADevicesInUse") || n("MFADevices"),
    rootMfa: n("AccountMFAEnabled") > 0,
    accessKeysPerUserQuota: num(map.AccessKeysPerUserQuota),
  };
}
