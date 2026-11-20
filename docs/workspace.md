# Workspace projects

Workspace projects group existing product resources so the dashboard can show them together, and
remember where each one sits on a canvas. A project only points at resources: deleting a project,
or taking a resource out of one, never deletes the resource.

- Code: `crates/roll-workspace` (the store and the router), mounted by `crates/rolld/src/workspace.rs`.
- Client: `apps/dashboard/src/lib/workspace-api.ts`. Change it and this contract together.
- Tests: `crates/roll-workspace/tests/api.rs` (the router in process, on SQLite).

People share a project as its members, each with a role ([members.md](members.md): v1, workspace
and secrets). The routes are below ([Members](#members)).

## Where it runs

Workspace projects are always on: they are not a product in `ROLL_PRODUCTS`. The routes take the
operator token or any signed-in user's session (docs/accounts.md).

## Owners and members

Every project has a namespace (the `owner` column): the signed-in user who made it (their `usr_…`
id), or empty for one made with the operator token. Its maker is also its first member, as `owner`
(docs/members.md).

- A user who isn't an admin lists and reads only the projects they're a **member** of. Any other
  project is a 404. What they may change there is their role's: rolld's gate decides every route
  from their session's grant (docs/members.md, "Evaluation").
- The operator and admins see every project (D9).
- Names are unique per namespace, and a resource's place is per namespace: two people can each put
  the same resource on their own project's canvas. A shared project's canvas is one canvas.
- A project lists the caller's `role` in it when they're a member.

### What a canvas stands for

Anyone may put any resource on their own canvas, so being on a canvas proves nothing about who owns
the resource. Before anything acts on a resource through a project, it asks
`Workspace::trusts(project, kind, ref)`. Examples:
- a secret linked to a database's credentials;
- an audit sink writing to a bucket;
- a repo's checks and previews getting the project's secrets.

The answer:
- **The operator's projects** stand for everything on their canvas.
- **A person's projects** stand only for:
  - what was made through the project, or set to it: the resource's **home** (`homes`, D6). A
    create the evaluator places with `X-Roll-Project` sets it; `PUT …/homes/{kind}/{ref}` moves it.
    Only kinds whose ids are never given out again have one (`HOME_KINDS`: postgres, redis,
    stream, nosql, search, service); a product deleting the resource drops it
    (`Workspace::forget_resource`). A person homes or places only what `Owners` says is theirs;
  - what `Owners` (set by rolld) says is one of the project's **owners'** (role `owner`):
    - everything, for an admin;
    - a repo under the owner's roll code owner (`ada/…`);
    - a search cluster the owner owns;
    - nothing else, until a product says who owns its resources.

Adding a member never widens this; adding an owner can, which is why only owners make owners.

A failed lookup is a no. Never decide from canvas membership alone: roll secrets' links and sinks,
roll code's secret lookups and roll previews all ask `trusts`.

- On a single node, the data is SQLite at `<home>/workspace.db`.
- With the system database (`ROLL_SYSTEM_DATABASE`), the data is Postgres, in the schema `workspace`.
  Nodes share it without leases, because every read-then-write is one `code_sql` write transaction.

## Tables

```sql
projects(id TEXT PRIMARY KEY, owner TEXT NOT NULL DEFAULT '', name TEXT NOT NULL,
         description TEXT NOT NULL DEFAULT '', icon TEXT,  -- icon: checked JSON, see Shapes
         created_at INTEGER NOT NULL, UNIQUE (owner, name))
resources(owner TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL, ref TEXT NOT NULL,
          project_id TEXT NOT NULL, x REAL NOT NULL, y REAL NOT NULL, added_at INTEGER NOT NULL,
          PRIMARY KEY (owner, kind, ref))
members(project_id, user_id, role, environments,  -- JSON array, or NULL for the default
        added_by, added_at, PRIMARY KEY (project_id, user_id))
project_invites(id, project_id, token_hash UNIQUE, email, role, environments,
        state,                                      -- pending | used | revoked
        created_by, created_at, expires_at, used_by, used_at)
homes(kind, ref, project_id, set_by, set_at, PRIMARY KEY (kind, ref))
member_events(id, at, action, project_id, actor, subject, detail, ip, claimed_at, done)
```

Projects from before owners become the operator's: a one-time migration moves the old tables
aside, creates these and copies the rows (with their description and icon, when they had them).
Projects from before members get their maker as their owner (`workspace-members`, once). Deleting
a project deletes its members, invites and homes in the same write.

`member_events` is the outbox: every membership change records its event in its own transaction,
and rolld acts on it after the write (roll's audit log, `product=workspace`; a removal revokes the
person's tokens confined to the project, D7), claiming each for a minute first and retrying until
it's done.

- A project id is `prj_` followed by 12 random characters of lowercase base32.
- Times are unix milliseconds.
- A resource belongs to at most one project of each owner.

## Values

- **Name**: 1–48 characters of `[a-z0-9-]`, not starting or ending with `-`. Names are unique per
  owner.
- **Kind**: `postgres`, `redis`, `stream`, `bucket`, `nosql`, `search`, `queue`, `tasks`, `repo` or
  `service` (a roll apps service, docs/apps/README.md; roll apps places a new service on its
  project's canvas itself).
- **Ref**: the product's own id, 1–200 characters. It is a Postgres project id, a Redis database
  id, a stream id, a bucket name, a NoSQL database id, a search cluster id, a queues vhost name, a
  tasks tenant id, `owner/name` for a repo, or a service id (`svc_…`). In a path it is
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
| `GET /v1/workspace/projects/{id}/members` | | `Member[]` (emails only for its admins) |
| `PATCH /v1/workspace/projects/{id}/members/{user}` | `{role?, environments?}` | `Member` |
| `DELETE /v1/workspace/projects/{id}/members/{user}` | | 204 |
| `DELETE /v1/workspace/projects/{id}/members/me` | | 204: leave |
| `GET /v1/workspace/projects/{id}/invites` | | `Invite[]` (pending; never a token) |
| `POST /v1/workspace/projects/{id}/invites` | `{email, role, environments?}` | 201 `Invite & {url}` |
| `DELETE /v1/workspace/projects/{id}/invites/{invite}` | | 204 |
| `PUT /v1/workspace/projects/{id}/homes/{kind}/{ref}` | | `{kind, ref, project}` |
| `GET /v1/workspace/roles` | | `{roles: [{role, rules}], products, developer_default}` |
| `GET /v1/workspace/invites` | | the signed-in person's pending `Invite[]` |
| `POST /v1/workspace/invites/{token}/accept` | | `Member` |
| `POST /v1/workspace/invites/{token}/decline` | | 204 |

- `PUT` adds the resource, moves it on the canvas, or moves it from another project of the same
  owner, in one write.
  `added_at` changes only when the resource joins a project.
- `DELETE` of a project removes the project and its membership rows.
- `PATCH` changes only the fields it has; `"icon": null` goes back to the default.
- 400: a bad name, description, icon, kind, ref or body (`x` and `y` must be finite numbers).
- 404: no such project (or another owner's), or (`DELETE` of a resource) the resource isn't in
  that project.
- 409: `{"error": "a project named X exists"}`.

### Members

docs/members.md is the design; these are its v1 routes.

- `Member`: `{project, user, handle, email?, role, environments, added_by, added_at, inactive?}`.
  `role` is `viewer`, `developer`, `admin` or `owner`; `environments` narrows a developer (null:
  every environment that isn't protected, D3); `inactive` is a person who can't sign in now.
- `Invite`: `{id, project, project_name, email, role, environments, state, created_by,
  created_at, expires_at}`. `POST …/invites` answers it with `url` once
  (`<web>/dashboard/invites?token=rlv_…`, checksummed so leak scanning knows it), mails it to the address when it has an account, and
  stores only the token's hash. The link works for 7 days, once, for that address only; a new
  invite to the same address replaces the old one. The answer is the same whether or not the
  address has an account (D5: one without waits until roll's operator or an admin makes it).
- Managing members (invites, roles, removal, homes) needs `admin` on the project, and a role is
  given, changed or taken away only when the caller's own rules cover it: an admin manages up to
  admin, only an owner makes, demotes or removes owners. 403 `{code: "forbidden"}` otherwise.
- 409 `{code: "last_owner"}`: the change would leave a person's project with no owner who can sign
  in. The operator's projects never run out (D8: they take members too).
- `members/me`, `GET /v1/workspace/invites` and accepting or declining are the signed-in person's
  own. Accepting takes their session (never a token) and their account's address must be the
  invite's; every failure is the same 404.
- `PUT …/homes/{kind}/{ref}` needs `admin` outright on the resource where it is now, as placing
  one does, and from a person (their session or their token) that `Owners` says it's theirs;
  moving a home away from another project takes being that project's admin too. A kind outside
  `HOME_KINDS` is a 400.
- A new invite to an address replaces a pending one only when the caller may give the role it
  names (409 otherwise); a member removed or demoted loses the pending invites their role no longer
  gives, in the same write.

## Shapes

```ts
type Project = { id: string; owner: string; name: string; description: string; icon: Icon | null; created_at: number; resources: Resource[] };
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
