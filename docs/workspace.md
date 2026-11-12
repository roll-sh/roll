# Workspace projects

Workspace projects group existing product resources so the dashboard can show them together, and
remember where each one sits on a canvas. A project only points at resources: deleting a project,
or taking a resource out of one, never deletes the resource.

- Code: `crates/roll-workspace` (the store and the router), mounted by `crates/rolld/src/workspace.rs`.
- Client: `apps/dashboard/src/lib/workspace-api.ts`. Change it and this contract together.
- Tests: `crates/roll-workspace/tests/api.rs` (the router in process, on SQLite).

## Where it runs

Workspace projects are always on: they are not a product in `ROLL_PRODUCTS`. The routes sit behind
the operator token, like `/v1/projects`.

- On a single node, the data is SQLite at `<home>/workspace.db`.
- With the system database (`ROLL_SYSTEM_DATABASE`), the data is Postgres, in the schema `workspace`.
  Nodes share it without leases, because every read-then-write is one `code_sql` write transaction.

## Tables

```sql
projects(id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL)
resources(kind TEXT NOT NULL, ref TEXT NOT NULL, project_id TEXT NOT NULL,
          x REAL NOT NULL, y REAL NOT NULL, added_at INTEGER NOT NULL, PRIMARY KEY (kind, ref))
```

- A project id is `prj_` followed by 12 random characters of lowercase base32.
- Times are unix milliseconds.
- A resource belongs to at most one project.

## Values

- **Name**: 1–48 characters of `[a-z0-9-]`, not starting or ending with `-`. Names are unique.
- **Kind**: `postgres`, `redis`, `stream`, `bucket`, `nosql`, `repo` or `service` (a roll apps
  service, docs/apps/README.md; roll apps places a new service on its project's canvas itself).
- **Ref**: the product's own id, 1–200 characters. It is a Postgres project id, a Redis database
  id, a stream id, a bucket name, a NoSQL database id, `owner/name` for a repo, or a service id
  (`svc_…`). In a path it is
  one percent-encoded segment (`acme%2Fsite`).

## Routes

Bodies are JSON. Every error is `{"error": string}`.

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/workspace/projects` | | `Project[]`, oldest first |
| `POST /v1/workspace/projects` | `{name, description?, icon?}` | 201 `Project` |
| `GET /v1/workspace/projects/{id}` | | `Project` |
| `PATCH /v1/workspace/projects/{id}` | `{name?, description?, icon?}` | `Project` |
| `DELETE /v1/workspace/projects/{id}` | | 204 |
| `PUT /v1/workspace/projects/{id}/resources/{kind}/{ref}` | `{x, y}` | `Resource` |
| `DELETE /v1/workspace/projects/{id}/resources/{kind}/{ref}` | | 204 |

- `PUT` adds the resource, moves it on the canvas, or moves it from another project, in one write.
  `added_at` changes only when the resource joins a project.
- `DELETE` of a project removes the project and its membership rows.
- `PATCH` changes only the fields it has; `"icon": null` goes back to the default.
- 400: a bad name, description, icon, kind, ref or body (`x` and `y` must be finite numbers).
- 404: no such project, or (`DELETE` of a resource) the resource isn't in that project.
- 409: `{"error": "a project named X exists"}`.

## Shapes

```ts
type Project = { id: string; name: string; description: string; icon: Icon | null; created_at: number; resources: Resource[] };
type Icon = { color?: Color } & (
  | { kind: "monogram"; text?: string } // 1–3 characters; none: from the name
  | { kind: "emoji"; emoji: string }
  | { kind: "image"; url: string } // data:image/png|jpeg|webp;base64,…, at most 200 KiB; never SVG
);
type Color = "slate" | "violet" | "purple" | "pink" | "rose" | "teal" | "cyan" | "green";
type Resource = { kind: Kind; ref: string; x: number; y: number; added_at: number };
```

A project's resources are ordered by `added_at`.

A description is at most 280 characters. Icon colours are the dashboard's identity colours (`--id-<color>`).
