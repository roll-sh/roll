/**
 * Client for rolld's workspace API (`crates/roll-workspace`, docs/workspace.md): projects that
 * group the products' resources, and where each one sits on its project's canvas. Change both
 * together.
 */

import type { IdentityColor } from "./agents-api";
import { call } from "./roll-api";

export const RESOURCE_KINDS = ["postgres", "redis", "stream", "bucket", "nosql", "search", "repo"] as const;
export type ResourceKind = (typeof RESOURCE_KINDS)[number];

export interface PlacedResource {
  kind: ResourceKind;
  /** The product's own id: a Postgres project id, a Redis or NoSQL db id, a stream id, a bucket name, `owner/name`. */
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
  name: string;
  description: string;
  icon: WorkspaceIcon | null;
  created_at: number;
  resources: PlacedResource[];
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
};

/** The workspace rule for project names (crates/roll-workspace). */
export const PROJECT_NAME = /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/;

export const DESCRIPTION_MAX = 280;
export const ICON_IMAGE_MAX = 200 * 1024;
