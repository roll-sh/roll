/**
 * Client for rolld's workspace API (`crates/roll-workspace`, docs/workspace.md): projects that
 * group the products' resources, and where each one sits on its project's canvas. Change both
 * together.
 */

import { call } from "./roll-api";

export const RESOURCE_KINDS = ["postgres", "redis", "stream", "bucket", "nosql", "repo"] as const;
export type ResourceKind = (typeof RESOURCE_KINDS)[number];

export interface PlacedResource {
  kind: ResourceKind;
  /** The product's own id: a Postgres project id, a Redis or NoSQL db id, a stream id, a bucket name, `owner/name`. */
  ref: string;
  x: number;
  y: number;
  added_at: number;
}

export interface WorkspaceProject {
  id: string;
  name: string;
  created_at: number;
  resources: PlacedResource[];
}

const project = (id: string) => `/v1/workspace/projects/${encodeURIComponent(id)}`;
const resource = (id: string, kind: ResourceKind, ref: string) =>
  `${project(id)}/resources/${kind}/${encodeURIComponent(ref)}`;

export const workspaceApi = {
  projects: () => call<WorkspaceProject[]>("GET", "/v1/workspace/projects"),
  create: (name: string) => call<WorkspaceProject>("POST", "/v1/workspace/projects", { name }),
  rename: (id: string, name: string) => call<WorkspaceProject>("PATCH", project(id), { name }),
  remove: (id: string) => call<null>("DELETE", project(id)),
  /** Adds the resource, moves it on the canvas, or takes it from another project. */
  place: (id: string, kind: ResourceKind, ref: string, x: number, y: number) =>
    call<PlacedResource>("PUT", resource(id, kind, ref), { x, y }),
  /** Takes the resource out of the project; the resource itself stays. */
  unplace: (id: string, kind: ResourceKind, ref: string) => call<null>("DELETE", resource(id, kind, ref)),
};

/** The workspace rule for project names (crates/roll-workspace). */
export const PROJECT_NAME = /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/;
