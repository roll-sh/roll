# Members v1.1: groups, SCIM groups and time-boxed elevation (design)

Status: **phase 1 built** (groups, group roles and SCIM `Groups`, with D10–D12 and D18 as option
(a)); time-boxed elevation (D13–D17) is still a design. This is item 6 of roll secrets' thirty-fourth product review
(docs/secrets.md, "Next"), and it builds on members v1 ([members.md](members.md), on main since
#501). members.md's v1.1 also covers apps, previews and search becoming member products (previews
is built, #524). This document is the other half: who holds a role, besides a person.

It adds three things:

1. **Groups as role holders.** A group gets a role on a project, and its people get that role.
2. **SCIM `Groups`.** A company's identity provider keeps a group's members.
3. **Time-boxed elevation.** A member asks for more on one project or one environment, for a few
   hours, with a reason. Someone else approves it, and it ends on its own.

The rule behind v1 still holds: **a person's session is a roll-access grant**, built on every
request and decided by the one evaluator. Groups and elevations add rules to that grant. Neither
one is a second permission system, and no product reads a group or an elevation.

Each open decision below (D10–D18, continuing members.md's D1–D9) has a default, option (a). Each
default is one constant in `roll_auth::roles::decisions`.

## What others do, and what roll takes

| product | what it does | what roll takes |
| --- | --- | --- |
| Infisical ([SCIM](https://infisical.com/docs/documentation/platform/scim/overview), [access requests](https://infisical.com/docs/documentation/platform/access-controls/access-requests)) | SCIM pushes users and groups. Groups get project roles, and a member's roles add up. A person asks for temporary access to an environment or a secret path, with a reason and a duration. An approval policy names its approvers (users or groups) and how many must approve. | groups with project roles, as a union; a request for one environment, for a duration, with a reason; approvers as roles or a group, counted as change requests are |
| Doppler ([SCIM](https://docs.doppler.com/docs/scim)) | SCIM groups become Doppler groups. A group holds workplace and project roles, and its members get them. Removing someone from the group in the IdP removes the access. | an IdP group becomes one roll group, and roll decides its project roles |
| 1Password Business | SCIM bridge provisions people and groups. Groups get vault permissions, and removing a person from the group in the IdP takes them out. | the IdP owns a group's members; roll owns what the group may do |
| Vault ([identity groups](https://developer.hashicorp.com/vault/docs/concepts/identity), [control groups](https://developer.hashicorp.com/vault/docs/enterprise/control-groups)) | External groups map an IdP group onto Vault policies. Control groups hold a request until N members of named groups approve it. | approvers can be a group, with a count; a request is held until enough approve |
| AWS IAM Identity Center (temporary elevated access) | A person asks for a permission set on one account, for a duration, with a justification. An approver says yes, and the access ends at the end of the window. | a role on one project (or one environment) for up to N hours; it ends by itself; ending it early is possible |

What roll adds: an elevation is a rule with an end time in the person's session grant. The same
evaluator decides it, so their own tokens and MCP clients get it too, inside their rules
(`Within`). It lapses on the next request after its end, on every node, with nothing to revoke.

## Decisions at a glance

- Groups live in `roll-workspace`, next to memberships. A person's direct and group roles are read
  in **one indexed query** on every request.
- Roles add up (a union of rules). **Groups never hold `owner`**, so groups never widen what a
  project trusts (`Workspace::trusts` doesn't change).
- SCIM `Groups` is roll-accounts' wire. It writes through a `Directory` trait that rolld wires to
  the workspace. It is **off for each connection until an admin turns it on**.
- An elevation is a roll-access **approval** (method `ELEVATE`). When it's approved, an
  `elevations` row is written in the same transaction. The session grant adds the live rows.
  Gates, two-person counting, deciding, `Approved {id, by, until}` and the audit are the ones
  approvals already have.
- **An elevation lets a person do the work. It never lets them decide for others.** It never
  counts for managing members, making tokens, being a `role:` approver, reviewing a change
  request, accepting a preview's review, approving a fork's run, or seeing decoys. Those checks
  read the grant's **standing** rules (direct and group roles).

## Groups

### The model

```ts
type Group = {
  id: string;                // grp_ followed by 12 random characters of lowercase base32
  name: string;              // 1–255 characters, no control characters
  description: string;       // up to 280 characters
  source: "roll" | `scim:${string}`;  // scim:<SSO connection id>: only SCIM changes its members
  external_id?: string;      // the IdP's id (SCIM externalId); kept, never matched across sources
  members: number;           // how many people are in it
  created_by: string;        // usr_…, "operator", or scim:<connection>
  created_at: number;
};
type GroupRole = {           // a group's role on one project
  project: string;
  group: string;
  name: string;
  source: Group["source"];
  role: "viewer" | "developer" | "admin";   // never owner (D10)
  environments: string[] | null;          // developer only, as a member's
  added_by: string;
  added_at: number;
};
```

```sql
groups(id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
       source TEXT NOT NULL, external_id TEXT, created_by TEXT NOT NULL, created_at INTEGER NOT NULL,
       UNIQUE (source, name)) WITHOUT ROWID;
CREATE UNIQUE INDEX groups_external ON groups (source, external_id) WHERE external_id IS NOT NULL;
group_members(group_id TEXT NOT NULL, user_id TEXT NOT NULL, added_by TEXT NOT NULL,
       added_at INTEGER NOT NULL, PRIMARY KEY (group_id, user_id)) WITHOUT ROWID;
CREATE INDEX group_members_user ON group_members (user_id);
group_roles(project_id TEXT NOT NULL, group_id TEXT NOT NULL, role TEXT NOT NULL,
       environments TEXT, added_by TEXT NOT NULL, added_at INTEGER NOT NULL,
       PRIMARY KEY (group_id, project_id)) WITHOUT ROWID;
CREATE INDEX group_roles_project ON group_roles (project_id);
deleted_group_roles(…the same columns…, PRIMARY KEY (project_id, group_id)) WITHOUT ROWID;
```

Limits. Each one is checked in the write that would pass it, and a write past a limit is a 409
`{code: "limit"}` that names the limit:

| what | at most |
| --- | --- |
| groups on the server | 10,000 |
| people in one group | 10,000 |
| groups a person is in | 100 |
| projects one group has a role on | 200 |
| groups with a role on one project | 50 |
| projects in one person's grant (direct and through groups) | 1,000 (v1's `PROJECTS_MAX`) |

### Roles add up

A person's rules on project P are the union of:

- their own membership's rules;
- each group role on P of every group they're in.

These are `roll_auth::roles::rules` for each one, concatenated. Roles are rule sets, so the union
needs no special case. A developer in `staging` directly and a developer in `development` through a
group has both. Lists show the highest one as the person's `role`, and `via` says where each comes
from.

- **Groups never hold `owner` (D10).** An owner's own repos and search clusters make the project
  trust them (`Owners::owns`, members.md "The canvas rule"). If a group could own, an IdP adding
  someone to the group would quietly widen what the project's secrets reach. So owners stay
  people. A group role of `owner` is a 400.
- The last owner (409 `last_owner`) only counts people, as in v1. A project whose only admins come
  through a group still works. If it has no owner who can sign in, the dashboard flags it "no
  owner", as v1 does.
- A group is not a member. It can't accept invites, own anything, or be an approver except where a
  policy names it (`group:grp_…`, see [Deciding](#deciding)).

### Where groups live

Groups live in **`roll-workspace`**, not `roll-accounts`:

- The session grant needs a person's direct roles and group roles together, on every request,
  without a cache. In one database that is one indexed read:

  ```sql
  SELECT project_id, role, environments, '' AS via FROM members WHERE user_id = ?1
  UNION ALL
  SELECT gr.project_id, gr.role, gr.environments, gr.group_id
    FROM group_members gm JOIN group_roles gr ON gr.group_id = gm.group_id
   WHERE gm.user_id = ?1
  ORDER BY via = '' DESC, project_id
  LIMIT 1001;  -- distinct projects are capped at 1,000 below
  ```

  If accounts held the groups, every request would read two databases, and they could disagree
  for the length of one request.
- Memberships, invites, homes and the outbox are in the workspace already, and a group role
  changes in the same write as the checks it needs (`may_grant`, the limits).
- roll-accounts keeps what it owns: people, SSO connections and the SCIM wire. SCIM's `Groups`
  handlers call a trait that rolld sets (`Accounts::set_directory`), as the workspace's `People` and
  `Owners` are set today:

  ```rust
  /// What SCIM changes in roll's groups (rolld: the workspace). Every call is one write.
  pub trait Directory: Send + Sync {
      fn scim_groups(&self, conn: &str, filter: GroupFilter, page: Page) -> Result<(i64, Vec<ScimGroup>)>;
      fn scim_group(&self, conn: &str, id: &str) -> Result<ScimGroup>;
      fn scim_create(&self, conn: &str, g: NewGroup) -> Result<ScimGroup>;           // 409 uniqueness
      fn scim_replace(&self, conn: &str, id: &str, g: NewGroup) -> Result<ScimGroup>;
      fn scim_patch(&self, conn: &str, id: &str, ops: Vec<GroupOp>) -> Result<ScimGroup>;
      fn scim_delete(&self, conn: &str, id: &str) -> Result<()>;
  }
  ```

### Reading group roles without caching

| who reads | what changes in v1.1 |
| --- | --- |
| `Access::session_grant` | `Workspace::memberships_of(user)` runs the query above. `Membership` gains `via: Option<String>` (the group), which the audit log uses. The grant also gets the person's live elevations ([below](#what-an-elevation-gives)). Nothing is cached, as in v1. |
| `roll_auth::Grant` | gains `standing: Option<Arc<Scope>>`: a session's rules without its elevations (`None`: the same as `scope`). `Grant::allows_standing` reads it. Tokens never set it. |
| `Workspace::role_in` | the person's highest role through their membership and their groups (what lists show). The new `Workspace::direct_role` is their own membership only (the last-owner check, the member list). |
| `Workspace::trusts` | **unchanged.** Rule 3 reads the project's owners, and groups are never owners. Homes don't depend on who holds a role. |
| `maker_reaches(by, needs)` (roll secrets) | unchanged in code. It reads the maker's session grant now, and that grant now includes their group roles and their live elevations. A sync, identity, backup or bound reference that a person made while their access came through a group stops as soon as the group loses the role, or the person leaves the group. |
| roll secrets' `Standing.roles` (who a `role:admin` approver is) | reads `role_in`, so a group admin is a `role:admin` approver. It never reads an elevation. `role:owner` only ever matches people. |
| `Workspace::may_grant` / `Actor::may_grant` | checks the caller's **standing** rules. An elevated admin can't invite, change roles or give a group a role. |

### When a group loses a role

When a group's role on P is removed or lowered, when a person leaves a group, or when a group is
deleted, the write records one `member_events` row (`group.role`, `group.role_remove`,
`group.member_remove`, `group.delete`) in the same transaction, as v1's member changes do. The
next request of everyone affected is decided without the role, because the grant is read live.
After the write, the outbox worker does what v1's removal does, for each affected person:

1. It works out who lost something: the group's members, or the one person who left. It pages
   through `group_members` 500 at a time and keeps a cursor in the event's `detail`, so a
   10,000-person group resumes where it stopped after a crash.
2. For each person who no longer reaches P at all (no membership and no other group role), it runs
   `Access::member_event`'s removal: their tokens confined to P are revoked (D7), one audit row
   each, with the event named in `why`.
3. For each person whose rights on P went down, it calls `Secrets::member_changed(P, user)`. That
   ends the folder shares they made once they can't manage P, and their one-time shares and leases
   once they're not a member.
4. Anything else they made stops through `maker_reaches` on its next use (members.md, "Remove, and
   leave"). This needs nothing from the worker.

**What a group-made thing does when the group loses the role:** nothing is made by a group. A
token, an identity or a sync is always made by a person, and it runs at that person's rights now.
When the group loses the role, every member whose rights came through that group loses those
rights at once. What they made stops or narrows as it would after a v1 removal or demotion. Adopt
works the same way.

### Routes

The catalogue rows are in `crates/roll-access/src/catalog.rs`. "Operator's" means no row: a roll
admin's session or the operator token, listed in permissions.md's "Classification" table and in
`rolld/tests/auth.rs`.

| route | catalogue action | answer and failures |
| --- | --- | --- |
| `GET /v1/workspace/groups` | person's own (classification) | the groups the caller may see: all of them for the operator; for a person, the groups they're in and the groups with a role on a project they're an admin of |
| `POST /v1/workspace/groups` `{name, description?}` | operator's (D12) | 201 `Group` (`source: "roll"`); 409 when the name exists; 409 `limit` |
| `GET /v1/workspace/groups/{group}` | person's own | `Group`; 404 unless the caller sees it (as in the list) |
| `PATCH /v1/workspace/groups/{group}` `{name?, description?}` | operator's | `Group`; 403 `managed` for a SCIM group's name |
| `DELETE /v1/workspace/groups/{group}` | operator's | 204; 403 `managed` for a SCIM group (its IdP deletes it); its roles go, through the outbox |
| `GET /v1/workspace/groups/{group}/members` | person's own | `{user, handle, email?, added_at}[]`, paged (`?after=&limit=`, up to 500); emails only for the operator |
| `PUT /v1/workspace/groups/{group}/members/{user}` | operator's | 204; 403 `managed` for a SCIM group; 404 for a person who doesn't exist; 409 `limit` |
| `DELETE /v1/workspace/groups/{group}/members/{user}` | operator's | 204; 403 `managed` for a SCIM group |
| `GET /v1/workspace/projects/{id}/groups` | workspace `read`, `{id}` | `GroupRole[]` with each group's member count |
| `PUT /v1/workspace/projects/{id}/groups/{group}` `{role, environments?}` | workspace `admin`, `{id}`, **standing** | `GroupRole`; 400 for `owner`; 403 `forbidden` when the role (or the one it replaces) isn't covered by the caller's standing rules, or the caller may not assign the group (D11); 409 `limit`, including any member who would pass 1,000 projects |
| `DELETE /v1/workspace/projects/{id}/groups/{group}` | workspace `admin`, `{id}`, **standing** | 204; 403 when the old role isn't covered by the caller |
| `GET /v1/workspace/projects/{id}/members` | unchanged row | `Member` gains `via: {group, name, role}[]` for people who are also in through groups; a new `?groups=1` adds people who are in only through groups (paged, `?after=&limit=`) |

- **Who may assign a group (D11):** the operator, or a project admin who is in the group
  themselves. Otherwise a project admin could see the member list of any group by assigning it, and
  could put a whole company in a project its people never chose.
- **Standing** is a new catalogue flag on a row (`.standing()`): the evaluator decides it against
  `Grant::standing`, so an elevation's rules never count there. The workspace's member-management
  rows (members, invites, homes, group roles, the elevation policy) all get it in v1.1.
- `docs/workspace.md`'s contract and `apps/dashboard/src/lib/workspace-api.ts` change together.

### Failure cases

- A group deleted while one of its people is mid-request: that request was decided on the grant it
  read. The next one is decided without the group.
- A person who is in a group but can't sign in (disabled, deleted): they have no session and no
  grant. Their tokens pause with the account (v1), and `maker_reaches` refuses for them.
- A group role on a deleted project: it is set aside with the project ([Interactions](#interactions)).
- A grant that would pass 1,000 projects. This can only happen when SCIM adds a person to groups
  whose projects add up to more, because roll's own writes refuse it. The grant keeps the person's
  direct memberships first, then group projects by project id, up to 1,000. The person's Home and
  the server's health say `too_many_projects`, and name the groups. It is never silent.
- A row with a role the code doesn't know (a later version wrote it): it is read as `viewer`, as v1
  reads members.

## SCIM `Groups`

roll-accounts' SCIM (docs/accounts.md, "SCIM") only handles users today. Its
`ServiceProviderConfig` says so, and `/Groups` is a SCIM 404. v1.1 adds RFC 7643's Group resource
and RFC 7644's operations on it.

### Turning it on

- An SSO connection gets `groups: bool`. It is false by default (D18), and set with
  `PATCH /v1/accounts/sso/{id} {groups: true}` (an admin's call, audited as `sso.update`).
- While it's off, `/Groups` stays a SCIM 404 and the config says `groups: unsupported`. A provider
  that was set up with "push groups" switched off keeps working as it does today. Turning it on
  never imports anything: the provider pushes its groups on its next sync.
- Turning it off stops new group changes. The connection's groups stay as they are, with their
  members and roles. Deleting them is the operator's call (`DELETE /v1/workspace/groups/{group}`
  works on a SCIM group once its connection's `groups` is off).
- Removing the SSO connection deletes its groups (one workspace write, through the outbox).

### Mapping an IdP group to a roll group

- Each SCIM group is **one roll group** with `source: "scim:<connection>"`, made by the provider's
  `POST /Groups`. `displayName` becomes `name`, and `externalId` is kept. Its SCIM `id` is the
  group's `grp_…`.
- roll never links an IdP group to an existing roll group by name. Same-named groups from two
  connections, or from roll and a connection, are different groups. A name match must never make
  an IdP the keeper of a group that roll admins built.
- The IdP owns the **members**. roll owns the group's **roles**: which projects, which role, which
  environments. The operator or a project admin gives roles in roll (the routes above). The IdP
  never sends a role, as with Doppler and Infisical.
- A member is a roll person that the connection reaches: their address is in the connection's
  verified domains, and they're not an admin (the same rule as `/Users`). The provider names them
  by the `usr_…` id roll gave it from `/Users`.

### Operations

All of these are under `/v1/accounts/scim/v2/`, with the connection's SCIM token (the product's own
credential, so the gate doesn't classify them). Each change is one workspace write and one audit
row in roll-accounts' audit (`scim.group.create`, `scim.group.update`, `scim.group.delete`, as
`scim:<connection>`, with the counts of added, removed and skipped members).

| operation | what roll does |
| --- | --- |
| `GET /Groups?filter=displayName eq "…"` (also `externalId eq`, `id eq`), `startIndex`, `count` (up to 200), `excludedAttributes=members` | the connection's groups, paged in the database. Okta and Entra look a group up by name before they create it. |
| `GET /Groups/{id}` | one group, with `members` (each `{value: usr_…, display}`), unless `excludedAttributes=members` |
| `POST /Groups` `{displayName, externalId?, members?}` | 201 with `Location`; 409 `uniqueness` when the connection has that name |
| `PUT /Groups/{id}` | replaces the name, `externalId` and members, in one write |
| `PATCH /Groups/{id}` | `add`, `remove` and `replace` on `members`, `displayName` and `externalId`, including Entra's `members[value eq "usr_…"]` remove path and Okta's whole-list `replace`. The operations apply in order, in one write. |
| `DELETE /Groups/{id}` | 204: the group, its members and its project roles go. The outbox cuts off what they reached. |

- **Members the connection can't reach** (an admin, someone outside its domains, an unknown id)
  are skipped, never an error. They're listed in the audit row's `skipped` count and on the
  connection's page. One admin in an IdP group must not stop the whole group from syncing, and a
  404 per person would let the provider probe who has an account.
- `Bulk` stays unsupported. `ResourceTypes` and `Schemas` describe Group only while `groups` is on.

### Deprovisioning

- **A person removed from the group** (`PATCH` remove, `PUT` without them, `DELETE` of the group)
  loses its roles on their next request, on every node, because the grant is read live. In the
  same write, the outbox gets `group.member_remove`. After the write, it revokes their tokens
  confined to the projects they no longer reach, and calls `Secrets::member_changed`. What they
  made stops by `maker_reaches` on its next use.
- **A person deactivated** (`/Users` `active: false` or `DELETE`) is roll's own disable, as today:
  sessions end, and `maker_reaches` refuses everything for them. Their group memberships stay, so
  reactivating them in the IdP gives back exactly what they had. Their grant is gone while they're
  disabled anyway.
- **A pending or live elevation** of someone who loses their last role on P ends: an elevation
  needs a standing membership ([below](#what-an-elevation-gives)).

### Bounds

Never size an allocation from a count a peer sent:

- A `/Groups` body is read up to **1 MiB** (`axum::body::to_bytes` with that limit). A bigger one is
  a SCIM 413 (`tooLarge`), and nothing is parsed.
- `members` in one `POST`, `PUT` or `PATCH` value: at most **1,000** entries, each value at most 64
  bytes. A `PATCH` holds at most **100** operations, and each `path` is at most 256 bytes. Past any
  of these: a SCIM 400 (`tooMany`, `invalidValue`). Okta and Entra send members in batches far
  below this.
- A group holds at most **10,000** members. An add past it is a SCIM 400 (`tooMany`), and nothing
  in that request applies.
- Strings are cleaned as `/Users` cleans them (no control characters, at most 255 characters).
- `totalResults`, `startIndex` and `count` from a provider are clamped (`count` up to 200) and
  never size a buffer. Lists are paged in SQL.
- `GET /Groups/{id}` with 10,000 members answers about 1 MB, which is within the cap the provider
  reads. Providers can skip it with `excludedAttributes=members`.

### Failure cases

- A provider sends a group with `groups` off: SCIM 404, as today. Nothing is stored.
- The workspace write fails (a database error): SCIM 500. Nothing applied, and the provider
  retries. Every operation is idempotent (add an existing member, remove an absent one).
- Two providers race on one group (a retry and a new sync): each operation is one `code_sql` write,
  so the later one wins as a whole. Members are never half-applied.
- A leaked SCIM token is revoked as today (`Accounts::revoke_leaked_scim`). The provider can't
  change groups until an admin makes a new token. Groups keep their state.

## Time-boxed elevation

A member asks for a role on one project, or on one environment of it, for up to N hours, with a
reason. Someone other than the requester approves it. Then the person has that role until the end
of the window, and it ends on its own.

### What a person asks for

```ts
type ElevationAsk = {
  project: string;              // prj_…: one they're already a member of (directly or through a group)
  role: "developer" | "admin";  // never owner
  environment?: string;         // one environment (production); none: every environment
  for: "15m" | "1h" | "4h" | "8h" | "1d";  // within the project's policy (max_for)
  reason: string;               // 1–500 characters; the approvers read it
};
type Elevation = ElevationAsk & {
  id: string;                   // the approval's apr_…
  user: string; handle: string;
  state: "pending" | "active" | "ended" | "denied" | "expired";
  gates: { approvals: number; approvers?: string[]; approved: string[] }[];
  approved_by: string[];
  created_at: number; granted_at?: number; until?: number;
  ended_at?: number; ended_by?: string; ended_why?: string;
};
```

- **Only members ask.** A stranger to P gets a 404, as P is to them. An invite is how someone joins.
- **A request about one environment** gives the role's product rules with `environments:
  [environment]`, including a protected one. Getting into production for an hour is what this is
  for. Workspace rules are never part of an elevation.
- At most **5** pending or active elevations per person. Asking for the same thing again returns
  the one waiting, as a token's access request does.
- Asking takes the person's **session** (the dashboard). A token never asks for its person. An
  agent that holds a person's token asks for itself with the existing token access request
  (permissions.md, "Access requests"), which its person decides.

### The project's policy

```ts
type ElevationPolicy = {
  enabled: boolean;               // false by default (D13)
  approvals: number;              // 1–10; default 1
  protected_approvals: number;    // 1–10; default 2: when it reaches a protected environment (D14)
  approvers: string[];            // "role:admin", "role:owner", "group:grp_…", "usr_…"; default ["role:admin", "role:owner"]
  max_for: "15m" | "1h" | "4h" | "8h" | "1d";  // default 8h (D17)
  roles: ("developer" | "admin")[];            // what may be asked for; default both
};
```

It is stored with the project (`projects.elevation`, checked JSON), so it moves to
`deleted_projects` with the project and comes back with it.
`GET`/`PUT /v1/workspace/projects/{id}/elevation-policy` (workspace `read` / workspace `admin`,
standing). A policy whose approvers can't reach the count (no such group, fewer people than
`approvals`) is still saved, but the policy page says so. A request under it waits until the
operator decides it.

### Deciding

An elevation is a roll-access **approval**, so there is no second approval system:

- `POST /v1/access/elevations` writes an `Approval` with `grant_id: "session:<usr_…>"` (the person,
  not one session, so signing out doesn't drop it), `method: "ELEVATE"`, `route:
  "/v1/access/elevations"`, `product: "workspace"`, `resource: <project>`, `environment`, and `args:
  {role, for, reason}`. `summary` says the whole reach: "ada asks for admin on acme (production) for
  1h: rotate the leaked Stripe key".
- **Gates** are, in order:
  1. the project's policy: `approvals` (or `protected_approvals` when the environment is protected,
     or when none is named and the project has a protected one), from its `approvers`;
  2. every one of roll's `approval_rules` (permissions.md, "Two-person approvals") that covers any
     action the elevation would give, in its environment. For example, the operator's
     `secrets:reveal@production=2` applies to an admin elevation on production.

  The approval stays `pending` until each gate has its approvals.
- **Who counts**, as change requests count (docs/secrets.md, "Protected environments and
  approvals"):
  - The requester **never** counts, under any name they go by: their `usr_…`, a chat id linked to
    them, or a token of theirs.
  - An approver counts once.
  - `role:admin` and `role:owner` mean the project's admins and owners by their **standing** role
    (direct or group, never an elevation).
  - `group:grp_…` means the group's members.
  - roll's admin accounts count for the project's gate too (D15), as the operator everywhere (D9).
  - The operator token never counts, because anyone may hold it.
  - Who counts is read **when each approval is added and again when the elevation starts**. An
    approver removed or demoted in between no longer counts, as change requests' approvals are
    counted as applied.
- **Deciding takes a session.** These are the existing `POST /v1/access/approvals/{id}/approve` and
  `/deny` (operator's in the classification table today). They now also take the session of a
  person the approval's gates name, for `ELEVATE` approvals only. A token never decides, so an
  agent can't approve its own person's request, and neither can a token of an approver. Chat
  approvals (permissions.md, "Approvals in chat") count only for a chat approver the operator
  linked to an account, as now.
- A pending elevation **expires after 24 hours** (`ELEVATE` approvals' own lifetime; other
  approvals keep 10 minutes). It can't be remembered: `remember` is a 400 on it.
- Approved, in the **same write** that flips the approval: an `elevations` row in roll-access's
  store (an access request's temporary rule is written the same way):

  ```sql
  elevations(id TEXT PRIMARY KEY,          -- the approval's apr_…
             user_id TEXT NOT NULL, project_id TEXT NOT NULL, role TEXT NOT NULL,
             environment TEXT, reason TEXT NOT NULL, approved_by TEXT NOT NULL,  -- JSON array
             granted_at INTEGER NOT NULL, until INTEGER NOT NULL,
             ended_at INTEGER, ended_by TEXT, ended_why TEXT) WITHOUT ROWID;
  CREATE INDEX elevations_live ON elevations (user_id, until);
  ```

  `until` is `granted_at + for`, counted from the approval, not from the request.

### What an elevation gives

`Access::session_grant(user)` reads `elevations WHERE user_id = ? AND until > now AND ended_at IS
NULL`: one indexed read, never cached. It keeps only rows on projects the person is still a
standing member of. For each row, it adds `roles::elevation_rules(project, role, environment)`: the
role's product rules (no workspace rule), narrowed to the environment. Then:

- `Grant.scope` holds the standing rules plus the elevations' rules. `Grant.standing` holds the
  standing rules alone.
- When the evaluator allows a request **only** because of an elevation's rule (`standing` alone
  would refuse it), it sets `roll_auth::Approved {id: <apr_…>, by: approved_by, until: Some(until)}`.
  That is what a rule remembered for a while sets today. roll secrets' audit, timeline and reach
  then say "revealed by ada, approved by ben until 14:00" with no new code.
- The person's tokens and MCP clients act `Within` the session, so they reach the elevation too,
  inside their own rules. A token whose rules don't name production still doesn't reach production.

**What it never does: decide for others (D16).** These read `Grant::standing` (or a `.standing()`
catalogue row):

- managing members, invites, homes, group roles and the elevation policy (all workspace rules,
  which an elevation doesn't have);
- making or widening tokens: `may_grant(maker, scope)` covers by standing rules, as a token's
  temporary rules never let it make a wider token (permissions.md, "Remembered approvals");
- being a `role:` approver of a change request, a folder policy or another elevation;
- reviewing or bypassing a roll secrets change request (`…/requests/{id}/review`, `…/apply` with
  `bypass`);
- previews' `visual/accept`, `regressions/accept` and `approve` (accepting a review, letting a
  fork's pull request run with the project's data);
- seeing and planting decoys (`Caller::sees_decoys` asks `allows_standing(Admin, whole project)`).

A person who needs those for good needs a role, which a project admin gives.

### Ending

- **On its own.** At `until`, the next request's grant doesn't have it, on every node. There is no
  session grant to revoke, because none is stored. The minute sweeper (roll-access's maintenance
  loop) notices ended rows and writes `elevation.end` to the outbox for what follows. Being late
  never extends the access: the sweeper only runs the side effects.
- **Early.** `DELETE /v1/access/elevations/{id}` `{why?}` from the requester (a session or a token
  of theirs: ending only narrows), any person who could have decided it, a standing admin of the
  project, or the operator. It sets `ended_at`, `ended_by` and `ended_why` in one write, and the
  next request is decided without it.
- **Because something else changed.** The person leaves P or loses their last role on it, P is
  deleted, or the person's account is disabled. The grant drops the row at once (the standing check
  above), and the sweeper ends it with the reason.
- **What follows an end** (from the outbox): an access audit row; roll secrets' chain row (below);
  `Secrets::member_changed(P, user)`; and `notifications/tools/list_changed` to the person's open
  MCP listens.
- **Anything issued under it ends by `until`.** A dynamic lease or a one-time share made by a
  request carrying `Approved.until` gets a lifetime no longer than `until`. roll secrets clamps
  this when it issues them (new in v1.1; a lease's renewal under a lapsed elevation is refused as
  any reveal would be). Things the person made under it (an identity, a sync) run at their rights
  now (`maker_reaches`), so they stop at the end. The dashboard says so when one is made under an
  elevation: "works until 14:00 unless you hold this role then".

### Audit

- **roll's audit log** (`GET /v1/access/audit`, the SIEM sinks): `elevation.ask`, `.approve`
  (each approver), `.deny`, `.start`, `.end` (with `ended_why`) and `.expire`, with the
  requester, the project, the role, the environment, `for`, `until` and the reason. Requests run
  under it name the approval, as any approved request does.
- **roll secrets' tamper-evident chain** (docs/secrets.md, "Tamper evidence"): every elevation
  covers secrets (it's a member product), so the outbox calls `Secrets::elevation_changed(project,
  …)`. That writes `elevation.start` and `elevation.end` rows in the project's chain, with who
  asked, who approved, the role, the environment, `until` and the reason (never a value). Every
  read and write made under it already carries the approval in its row (`Caller::approval`).
- A refused ask or decision leaves an access audit row. A session's refused request leaves none,
  as in v1.

### Routes

| route | catalogue action | answer and failures |
| --- | --- | --- |
| `POST /v1/access/elevations` `ElevationAsk` | person's own: a session only (classification) | 202 `Elevation` (pending); 404 for a project they aren't a member of; 403 `elevation_off` when the policy isn't enabled; 400 for a role, `for` or reason the policy doesn't allow (`owner`, past `max_for`); 409 `limit` past 5 |
| `GET /v1/access/elevations?project=&state=&mine=` | person's own: a session or their token | the caller's own, plus those they may decide; the operator sees all |
| `GET /v1/access/elevations/{id}` | person's own | `Elevation`; 404 for anyone who isn't the requester, a possible approver, a standing admin of P, or the operator |
| `DELETE /v1/access/elevations/{id}` `{why?}` | person's own, or the operator | 204; 409 `ended` when it already has |
| `POST /v1/access/approvals/{id}/approve` and `/deny` | operator's, plus a named approver's session for `ELEVATE` | as today; 403 `own_request` for the requester; 403 `not_approver`; a `remember` on an elevation is a 400 |
| `GET /v1/workspace/projects/{id}/elevation-policy` | workspace `read`, `{id}` | `ElevationPolicy` |
| `PUT /v1/workspace/projects/{id}/elevation-policy` | workspace `admin`, `{id}`, standing | `ElevationPolicy`; 400 for an approver that isn't a role, `grp_…` or `usr_…` |

The elevation routes are a person's, like `/v1/workspace/invites`. They go in permissions.md's
"Classification" table, and `rolld/tests/auth.rs` checks them.

### Dashboard and CLI

- **Asking.** The project's Members tab, and every 403 that names what the role lacks
  (members.md: "a 403 that names what the role lacks"), offer **Request access**. That opens a sheet
  with the role, the environment, the duration (only what the policy allows) and the reason, plus
  what approving would let them do ("reveal and change production's secrets for 1 hour").
- **Approving.** It's a notification in the header, as approvals are now. The approvals list
  (`/dashboard/access/approvals`) shows elevations to people who may decide them: who, what, for
  how long, why, and how many approvals are still needed. Approvers also get a mail (best effort;
  at most 20 people per request), with a link and never a button that decides from the mail.
- **While active**, the person's header shows "admin on acme (production) until 14:00", with
  **End now**. The project's Members tab lists live elevations to its admins, with **End**.
- **Policy.** A section of the project's settings: on/off, approvers, counts and the longest
  duration.
- **CLI:**

  ```
  roll access request --project P --role admin [--env production] --for 1h --reason "…" [--wait]
  roll access elevations [--project P] [--mine]
  roll access end ID [--why "…"]
  roll projects groups list|set GROUP --role developer [--env …]|remove GROUP [--project P]
  roll projects elevation-policy [--project P] [--enable --approvals 1 --approver role:admin --max 8h]
  roll groups list|show|members GROUP
  ```

  `roll access request` opens the dashboard's request sheet, filled in, because asking takes a
  session (as `roll invites accept` does). With `--wait`, it then waits with the person's own login
  token on `GET /v1/access/elevations/{id}`, and prints when it starts or is denied.
  `roll access approve` is not added: deciding takes a session, and the CLI's login is a token.

### Failure cases

- An approver approves after the request expired: 409 `expired`, as approvals answer now.
- The last approval lands after the requester left the project: the elevation never starts. It
  becomes `ended` with `ended_why: "not a member"` in the approving write.
- Two approvers decide at the same moment: approvals are one write each, so the second sees the
  first's state. Two approvals on a gate of one start it once.
- The approver's role is taken away between approving and the last approval: their approval stops
  counting, and the elevation waits for another one.
- roll-access's store is down: the session's grant can't be built, so the request fails as any
  gate failure does. It never falls back to the standing rules alone, and never to the elevated
  ones.
- A clock step on one node: `until` is compared with each node's clock, as token expiries and
  temporary rules are. A node that runs fast ends it early, never late beyond its own skew.

## Interactions

### #585: a project's delete window

- `group_roles` of a deleted project move to `deleted_group_roles` in the delete's write, beside
  `deleted_members` and `deleted_homes`. Nobody's grant names the project while it's deleted.
- `Workspace::restore` brings them back as they were, except roles of groups deleted since. Those
  are dropped, each a `group.role_remove` in roll's audit log (built: the restore's answer doesn't
  list them).
- The elevation policy travels with the project row.
- Pending elevations on P are denied (`project deleted`), and live ones end in the delete's
  follow-up. They're **not** restored: an approval was for that time, and a recovery days later
  isn't that time.
- Groups themselves are server-wide and don't move. `Workspace::forget_deleted` drops the kept
  group roles when roll secrets purges.

### roll previews' accept routes

Members v1 first kept previews to a project's owners (#501's review). Since #524, each previews
route is decided by role through the catalogue. `visual/accept`, `regressions/accept` and
`approve` are `admin` on the preview's project, and previews still checks that the person can
write to the change's repo in roll code and isn't its author. v1.1 changes two things:

- An admin through a group accepts, as a direct admin does.
- The three rows become `.standing()`: an elevated admin can't accept a review or let a fork's run
  use the project's data. Previews' own handlers don't change. They keep seeing the person
  (`SignedIn`), and they drop a session's grant before their handlers, as now.

The previews thread owns those rows. This design asks it for the `.standing()` flag and one test
(below).

### roll MCP's tool listing by role (#644)

- `tools/list` is decided within the person's session (#644). Group roles are in that grant, so a
  group's developer sees developer tools with no MCP change.
- An elevation's tools appear while it's live and go when it ends. Its start and end send
  `notifications/tools/list_changed` on the person's open listens, as an approved access request
  does now (docs/mcp.md).
- New tools, one route each, listed in docs/mcp.md:
  - `workspace_list_groups` (`GET /v1/workspace/projects/{id}/groups`);
  - `workspace_set_group_role` (`PUT …/groups/{group}`, hinted destructive when it lowers a role);
  - `workspace_remove_group_role` (`DELETE …/groups/{group}`, destructive);
  - `access_list_elevations` (`GET /v1/access/elevations`);
  - `access_end_elevation` (`DELETE /v1/access/elevations/{id}`: an agent may end its person's
    elevation early, because ending only narrows).
- `crates/roll-mcp/src/no-tool.txt` gets lines for these, as what they are:
  - `POST /v1/access/elevations`: a person's call; asking takes a session.
  - Deciding: a person's call.
  - `PUT …/elevation-policy`: a person's call.
  - The operator's `/v1/workspace/groups…` routes: the operator's.

### Decoys

v1's rule is that only members who manage the project see decoys (`Caller::sees_decoys` and admin
on the whole project).

- An admin through a group manages the project, so they see decoys.
- An elevated admin does **not**: `sees_decoys` asks the standing rules. A decoy is there to catch
  someone who got in where they shouldn't be, and an elevation obtained by a stolen or
  social-engineered session is exactly that case. The decoy still alerts on use.
- Health's `decoy_used` goes to standing admins only.

## Rollout and migration

- **No feature flag in the environment.** Each part is inert until someone turns it on:
  - groups do nothing until the operator makes one or SCIM pushes one;
  - SCIM `Groups` is per connection, off by default (D18);
  - elevation is per project, off until a project admin enables a policy (D13).

  The defaults are constants in `roll_auth::roles::decisions` (`GROUPS_HOLD_OWNER = false`,
  `ELEVATION_SEES_DECOYS = false`, `ELEVATION_DEFAULT_MAX = 8h`, and so on).
- **Schema:** new tables with `CREATE TABLE IF NOT EXISTS` in the workspace (`groups`,
  `group_members`, `group_roles`, `deleted_group_roles`, plus `projects.elevation` and
  `deleted_projects.elevation`) and in roll-access (`elevations`). There's no backfill, and every
  v1 grant is unchanged. With the system database, they go in the workspace's schema and
  roll-access's, beside the tables they join.
- **Order**, one PR each, each green on its own:
  1. groups and group roles (workspace, the session grant's union, `standing`, the outbox
     fan-out, routes, the Members tab's Groups section, the CLI);
  2. SCIM `Groups` (accounts, `Directory`, the connection toggle);
  3. elevation (the access store and routes, the policy, `.standing()` rows including previews',
     roll secrets' chain rows and lease clamp, the dashboard and the CLI);
  4. MCP tools and docs/mcp.md (`pnpm test:mcp-sweep`).
- **Gates:** `pnpm test:secrets` (`--docker` for the SQL), `pnpm test:code` (roll-auth,
  roll-access, roll-mcp; `--docker` on Postgres), `cargo test -p rolld --test auth`, and
  `pnpm test:mcp-sweep` for part 4.

### Tests

One per flow, in the files the repo already uses for each:

| test | file | what it proves |
| --- | --- | --- |
| groups hold roles, and roles add up | `crates/roll-workspace/tests/api.rs` | a group's role reaches its people; direct developer plus group admin is admin; `owner` is a 400; the limits; a SCIM group's members are 403 `managed` to roll's routes; only a member of the group assigns it |
| a deleted project's group roles come back | `crates/roll-workspace/tests/api.rs` | set aside at delete, back on restore, a deleted group's role dropped and listed |
| group roles and elevations as grants | `crates/roll-access/tests/access.rs` | the session grant has group rules and live elevations; at `until` it hasn't (a test clock); `standing` excludes elevations; `.standing()` rows refuse an elevated rule; `may_grant` ignores elevations; `Approved.until` is set only when the elevation decided |
| an elevation is decided as change requests are | `crates/roll-access/tests/access.rs` | the requester never counts; two gates need two people; a `group:` approver counts and a non-member doesn't; a demoted approver stops counting; a token can't decide; early end; 24 h expiry |
| SCIM groups | `crates/roll-accounts/tests/flows.rs` | off by default (404); create, filter by name, PATCH add and remove (Entra's path form), PUT, DELETE; unreachable members skipped and counted; 1 MiB, 1,000-member and 100-operation bounds |
| losing a group role stops what they made | `crates/roll-secrets/tests/api.rs` (also on Postgres) | after the group loses the role, the person's confined token is revoked, their identity's login is refused and their sync pauses |
| an elevation in roll secrets | `crates/roll-secrets/tests/api.rs` (also on Postgres) | a developer reveals production only while elevated; the reveal's audit names the approval; the chain has `elevation.start` and `.end`; a lease issued under it ends by `until`; an elevated admin sees no decoys and can't review a change request |
| through rolld | `crates/rolld/tests/auth.rs` | every new route classified; a SCIM PATCH that removes a person from a group makes their next session request to the project a 404 |
| previews' accepts need a standing admin | `crates/roll-previews/tests/engine.rs` | a group admin accepts; an elevated admin gets a 403 |
| the dashboard | `apps/dashboard/src/lib/workspace-*.test.ts` | the Groups section's and the request sheet's controls by role and policy |
| the CLI | `crates/roll-cli/tests/projects.rs` | `roll projects groups …`, `roll access elevations` and `end` against a live API |

## Open decisions

These are the user's to make. The design uses option (a) of each.

- **D10. Groups holding `owner`.** (a) Never: owners stay people, so an IdP can't widen what a
  project trusts; (b) yes, and trust then counts every group owner's repos.
- **D11. Who gives a group a role on a project.** (a) The operator, or a project admin who is in
  the group; (b) any project admin, for any group (Infisical); (c) the operator only.
- **D12. Who makes roll (non-SCIM) groups.** (a) The operator and roll's admins; (b) any person,
  for the projects they own.
- **D13. Elevation by default.** (a) Off until a project admin enables a policy; (b) on, with the
  default policy (admins and owners approve, one approval).
- **D14. How many approvals.** (a) 1, and 2 when it reaches a protected environment; (b) always 1;
  (c) always 2.
- **D15. roll's admin accounts approving elevations.** (a) Yes, as the operator everywhere (D9);
  (b) only where a policy names them.
- **D16. What an elevation counts for.** (a) Doing the work only: never deciding for others
  (reviews, accepts, approve-run, approvers, members, tokens, decoys); (b) everything its rules
  say, like any rule.
- **D17. The longest elevation.** (a) 8 hours by default, up to a day; (b) up to 7 days by policy.
- **D18. SCIM groups per connection.** (a) Opt-in per connection; (b) on for every connection that
  has SCIM.

Later, not v1.1: elevation to one folder or key (Infisical's secret paths); approval chains in
sequence (step 1 a team lead, step 2 security); elevations posted to chat; custom roles; a project
admin approving their project's tokens' `ask` requests.
