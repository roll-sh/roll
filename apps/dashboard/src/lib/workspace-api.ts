/**
 * Client for rolld's workspace API (`crates/roll-workspace`, docs/workspace.md): projects that
 * group the products' resources, and where each one sits on its project's canvas. Change both
 * together.
 */

import type { IdentityColor } from "./agents-api";
import { call } from "./roll-api";

export const RESOURCE_KINDS = ["postgres", "redis", "stream", "bucket", "nosql", "search", "queue", "tasks", "repo", "service"] as const;
export type ResourceKind = (typeof RESOURCE_KINDS)[number];

export interface PlacedResource {
  kind: ResourceKind;
  /** The product's own id: a Postgres project id, a Redis or NoSQL db id, a stream id, a bucket name, `owner/name`, a service id (`svc_…`). */
  ref: string;
  x: number;
  y: number;
  added_at: number;
}

/** How a project shows itself (docs/workspace.md, "Shapes"); colours are the identity colours. */
export type WorkspaceIcon = { color?: IdentityColor } & (
  | { kind: "monogram"; text?: string }
  | { kind: "emoji"; emoji: string }
  /** A PNG, JPEG or WebP `data:` URL (never SVG), at most 200 KiB. */
  | { kind: "image"; url: string }
);

export interface WorkspaceProject {
  id: string;
  /** Its namespace: the person who made it (`usr_…`), or empty for the operator's. */
  owner?: string;
  name: string;
  description: string;
  icon: WorkspaceIcon | null;
  created_at: number;
  /** The caller's role in it, when they're a member (none: the operator, or an admin). */
  role?: Role;
  resources: PlacedResource[];
}

/** The built-in roles (docs/members.md), weakest first. */
export const ROLES = ["viewer", "developer", "admin", "owner"] as const;
export type Role = (typeof ROLES)[number];

export interface Member {
  project: string;
  user: string;
  handle: string;
  /** Only for the project's admins and owners. */
  email?: string;
  role: Role;
  /** A developer's environments, where the role applies in full (null: every unprotected one). */
  environments: string[] | null;
  added_by: string;
  added_at: number;
  /** Their account can't sign in now. */
  inactive?: boolean;
}

export interface Invite {
  id: string;
  project: string;
  project_name: string;
  email: string;
  role: Role;
  environments: string[] | null;
  state: "pending" | "waiting_admin" | "used" | "revoked";
  created_by: string;
  created_at: number;
  expires_at: number;
}

export interface RoleRules {
  roles: { role: Role; rules: unknown[] }[];
  products: string[];
  developer_default: "unprotected" | "all" | "none";
}

const project = (id: string) => `/v1/workspace/projects/${encodeURIComponent(id)}`;
const resource = (id: string, kind: ResourceKind, ref: string) =>
  `${project(id)}/resources/${kind}/${encodeURIComponent(ref)}`;

export const workspaceApi = {
  projects: () => call<WorkspaceProject[]>("GET", "/v1/workspace/projects"),
  create: (name: string, look: { description?: string; icon?: WorkspaceIcon | null } = {}) =>
    call<WorkspaceProject>("POST", "/v1/workspace/projects", { name, ...look }),
  /** Changes the fields given; `icon: null` goes back to the default. */
  update: (id: string, patch: { name?: string; description?: string; icon?: WorkspaceIcon | null }) =>
    call<WorkspaceProject>("PATCH", project(id), patch),
  remove: (id: string) => call<null>("DELETE", project(id)),
  /** Adds the resource, moves it on the canvas, or takes it from another project. */
  place: (id: string, kind: ResourceKind, ref: string, x: number, y: number) =>
    call<PlacedResource>("PUT", resource(id, kind, ref), { x, y }),
  /** Takes the resource out of the project; the resource itself stays. */
  unplace: (id: string, kind: ResourceKind, ref: string) => call<null>("DELETE", resource(id, kind, ref)),
  members: (id: string) => call<Member[]>("GET", `${project(id)}/members`),
  /** The link (with its token) is in the answer once. */
  invite: (id: string, email: string, role: Role, environments: string[] | null) =>
    call<Invite & { url: string }>("POST", `${project(id)}/invites`, { email, role, environments }),
  invites: (id: string) => call<Invite[]>("GET", `${project(id)}/invites`),
  revokeInvite: (id: string, invite: string) => call<null>("DELETE", `${project(id)}/invites/${encodeURIComponent(invite)}`),
  setRole: (id: string, user: string, role: Role, environments: string[] | null) =>
    call<Member>("PATCH", `${project(id)}/members/${encodeURIComponent(user)}`, { role, environments }),
  removeMember: (id: string, user: string) => call<null>("DELETE", `${project(id)}/members/${encodeURIComponent(user)}`),
  leave: (id: string) => call<null>("DELETE", `${project(id)}/members/me`),
  roles: () => call<RoleRules>("GET", "/v1/workspace/roles"),
  /** The signed-in person's own pending invites. */
  myInvites: () => call<Invite[]>("GET", "/v1/workspace/invites"),
  accept: (token: string) => call<Member>("POST", `/v1/workspace/invites/${encodeURIComponent(token)}/accept`),
  decline: (token: string) => call<null>("POST", `/v1/workspace/invites/${encodeURIComponent(token)}/decline`),
};

/** The workspace rule for project names (crates/roll-workspace). */
export const PROJECT_NAME = /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/;

export const DESCRIPTION_MAX = 280;
export const ICON_IMAGE_MAX = 200 * 1024;
