# Project members and roles (design)

Status: **v1 built** (workspace and secrets; [Phasing](#phasing), and [What v1 leaves out](#what-v1-leaves-out)).
This is roll secrets' long-standing gap R1 (docs/secrets.md, "Next"), and it covers every product
whose resources sit on a workspace project. Before it, a person reached only the projects they
made, and a team shared only through the operator or through tokens. This document is the plan
for people sharing a project, each with a role. v1 takes option (a) of every open decision
([Open decisions](#open-decisions)); each is one constant in `roll_auth::roles::decisions`.

Where v1 differs from the plan below: the roles' templates live in `crates/roll-auth/src/roles.rs`
(not roll-access), so the workspace checks "nobody grants more than they hold" without depending on
roll-access; roll-access builds the session grant from them (`Access::session_grant`). An invite's
link is `<web>/dashboard/invites?token=…` (no new top-level route).

The one rule behind the whole design: **a member's session becomes a roll-access grant**. The
grant is built from the member's role, on every request, and it goes through the one evaluator
(docs/permissions.md). roll-workspace stores who is a member. roll-access turns membership into
rules and decides. Products never check a role: they see a `roll_auth::Grant`, as they already do
for tokens. There is no second permission system.

## What others do, and what roll takes

| product | model | what roll takes |
| --- | --- | --- |
| Infisical | org roles (admin, member, no access) and project roles (admin, member, viewer, no access); custom roles on Enterprise, with conditions on environment and secret path; additional privileges per member, permanent or temporary; groups get project roles; SCIM | four built-in project roles; per-environment narrowing in v1; custom roles, groups and SCIM groups later; temporary access through roll-access's `ask` and remembered approvals, which already exist |
| Doppler | workplace roles (owner, admin, collaborator); project roles (viewer, collaborator, admin, none), each assigned **per environment**; workplace admins see every project; groups get project roles; custom roles; roles add up (most permissive union) | a role applies to the environments the assignment names; roles add up; admins of the whole server (roll's admin accounts) keep seeing everything |
| Vault | namespaces as tenants, each with its own policies, identity groups and delegated namespace admins; policies are path rules | a project is the tenant; a role is a set of rules, not code; a project admin manages the project but nothing server-wide (the deployment key, sinks, policy stay the operator's) |
| 1Password Business | vault permissions (view, create, edit, archive, delete items, view and copy passwords, item history, import, **manage vault**); groups; managing a vault doesn't show its items; admins aren't in a vault's contents unless added | managing members and seeing values are separate actions (`admin` vs `reveal`), and whether a project admin reveals production by default is an explicit decision ([D4](#open-decisions)) |

What none of them combine, and roll does: a role is just rules in the same model a scoped token
uses. So a person's own token, an MCP client they connect, and their session all reach exactly
what the role allows, decided in one place. A member also can never give a token more than their
role.

## The model

### Membership

A membership is **person × project × role**, with an optional list of environments the role is
narrowed to:

```ts
type Member = {
  project: string;            // prj_…
  user: string;               // usr_…
  handle: string;             // shown to every member
  email?: string;             // shown to the project's admins and owners only
  role: "viewer" | "developer" | "admin" | "owner";
  environments: string[] | null; // developer only: where the role applies in full; null = the default (D3)
  added_by: string;           // usr_… or "operator"
  added_at: number;
};
```

- A person has at most one membership per project. A second role is a change of role, not a
  second row. Doppler's several roles per environment come down to "developer in these
  environments, viewer elsewhere", which is one row with `environments`.
- `environments` uses roll-access's environment entries (`staging`, `pv-*`). It narrows only the
  actions a developer has beyond viewer's. Outside the listed environments, a developer is a
  viewer.
- A project holds at most 500 members, and a person is a member of at most 1,000 projects. Each
  membership is a few rules, so a session's grant stays small (see [Evaluation](#evaluation)).

### Built-in roles

Each role is a fixed template of `roll_auth::scope` rules over one project, written in
`crates/roll-auth/src/roles.rs`. `P` stands for the project, and `{project: P}` is the selector
on every rule. "Member products" are the products whose routes are open to members
([Phasing](#phasing)).

| role | workspace (the project itself) | each member product | managing members |
| --- | --- | --- | --- |
| `viewer` | read | read: names, metadata, history, never a value | sees the member list (handles and roles) |
| `developer` | read | viewer's, plus `write` and `reveal`, in its environments (D3); never `delete`, `deploy` or `admin` | as viewer |
| `admin` | read, write | every action in every environment: `read`, `write`, `delete`, `reveal`, `deploy`, `admin` | invites, changes roles up to `admin`, removes non-owners |
| `owner` | read, write, delete | as admin | as admin, plus owners: makes, demotes and removes owners, deletes the project |

What each action means in each product is the catalogue's (docs/permissions.md, "Actions").
Concretely, for roll secrets:

| | viewer | developer | admin and owner |
| --- | --- | --- | --- |
| names, versions, history, audit log | yes | yes | yes |
| values (`reveal`) | no (D2) | in its environments (D3) | everywhere (D4) |
| set, change, rollback, promote into | no | in its environments; a protected environment opens a change request | yes |
| delete secrets, environments | no | no | yes |
| protection, approvals, syncs, links, tokens, identities, sinks, purge | no | no | yes |
| review a change request | no | no | yes, or when named as an approver |

A membership has one role, so nothing adds up inside a project. The operator and roll's admin
accounts (docs/accounts.md, `admin`) stay the operator everywhere, whatever their membership says
([D9](#open-decisions)).

**Granting never reaches past the granter.** An admin can't make an owner, because the owner
role's rules aren't covered by an admin's (`Rule::covers`). This is the same rule that keeps a
token from making a wider token. Nobody writes a special case for "only owners make owners": it
falls out of the comparison.

### Per-environment narrowing

v1 narrows only the developer role, through `environments`. The default, when `environments` is
null, is decision D3. The recommended default is every environment that isn't protected. For that,
a rule needs one new field:

```ts
type Rule = { …; unprotected?: true }; // only resources whose environment the product says isn't protected
```

The product's resolver sets `Resource.protected` beside `Resource.environment`:

- secrets: an environment with a protection policy;
- apps: `production`;
- previews: never protected.

A resource whose protection isn't known never matches an `unprotected` rule. The field only
narrows, so a token can carry it too (a CI token for every environment except protected ones).

### Custom roles and groups

Neither is in v1. Groups are built in v1.1 ([members-v1-1.md](members-v1-1.md), phase 1, with SCIM
groups), and custom roles come later:

- **Custom roles** (Infisical Enterprise, Doppler): a role becomes a row of rules in roll-access,
  named per project or per server. The evaluation doesn't change: a membership names a role, and
  the role names rules. v1 keeps the role as a name looked up in `roles.rs`, so moving it to a
  table changes no caller.
- **Groups**: a group is a set of people with a role on a project, and a person's grant is the
  union of their own role and their groups' roles (Doppler and Infisical both union them). Groups
  come with SCIM groups. Today roll-accounts' SCIM is users only (docs/accounts.md, "SCIM"), and a
  group pushed by a company's provider is the reason to have them.

## Where it lives and who decides

| piece | where | what it does |
| --- | --- | --- |
| memberships, invites, homes | `crates/roll-workspace` | stores them; every read-then-write is one `code_sql` write |
| roles' rules | `crates/roll-auth/src/roles.rs` | turns a membership into `scope::Rule`s |
| a session's grant | `crates/roll-access` (`Access::session_grant`) | on every request, the person's memberships as a `Grant` of kind `session` |
| the decision | `roll_access::evaluate` | the same evaluator, catalogue and rules as for tokens |
| who is signed in | `roll_auth::SignedIn` | unchanged: who it is, admin or not, the session |

### Evaluation

On a request with a non-admin person's full session, on a member product's route, rolld's gate
(`roll_auth::check`) does what it does for a token today:

1. It resolves the session (unchanged) and puts `SignedIn` on the request.
2. It asks the authorizer for the session's grant: `Authorizer::handle_session(req, signed_in,
   next)`. roll-access reads the person's memberships from roll-workspace (one indexed read by
   `user_id`, never cached) and builds the grant from them:

   ```rust
   Grant { id: <session id ses_…>, name: "session", kind: GrantKind::Session,
           scope: <roles' rules for every membership>, user: Some(usr_…), ask_otherwise: false }
   ```
3. The evaluator decides the route exactly as for a token: the catalogue row, the resource, the
   rules, allow or deny. Lists are filtered and creates are placed in `X-Roll-Project`'s project.
   On allow, the request goes on as the operator's, narrowed: `Operator` plus `Grant` plus
   `SignedIn`, as a token's does today.
4. Routes of products that aren't member products yet answer 403 `operator_only`, as now.

A person's own token (`rlt_`, `rlo_`) is evaluated as now, and then also against the session grant
built at that moment. So it reaches what both allow (`Within`, generalised). Remove someone, and
every token of theirs stops reaching the project on its next request, without a revocation.

**A canvas proves nothing here either.** For a person's grant (session or token), a `{project: P}`
selector matches a resource only when the resource is on P's canvas **and**
`Workspace::trusts(P, kind, ref)`. Otherwise an admin of P could drop a stranger's database on P's
canvas and gain `admin` on it. The operator's tokens keep today's meaning. See
[The canvas rule](#the-canvas-rule).

### `SignedIn` and products

- `SignedIn` gains nothing. Who it is stays there, and what they may do is the `Grant` beside it.
  A product that needs a second check inside one request asks `Grant::allows`, as for tokens.
- `GrantKind::Session` tells products and the audit log that the principal is the person
  (`usr_…`), not a token. It is never listed in `/v1/access/tokens`, and it can't be revoked
  there: ending a session or removing a membership ends it.
- An approval (`ask`) never applies to a session grant (`ask_otherwise: false`, and roles only
  `allow`). A person asking for more is an elevation (v1.1, [members-v1-1.md](members-v1-1.md)).

## What changes for each product

Each product does only what's listed. None of them reads a role.

| product | v1 change |
| --- | --- |
| **workspace** | `Scope::Owner(user)` becomes `Scope::Member(user)`: lists and reads cover every project the person is a member of. The project routes go through the evaluator for sessions, so its catalogue rows decide (read, write, delete as in the role table). Placing a resource keeps permissions.md's rule: `write` on the project **and** `admin`, allowed outright, on the resource where it is now. For a person that means admin through another project that trusts it, or `Owners` saying it's theirs (their repo, their cluster). New: members, invites and homes ([Tables](#tables)). |
| **secrets** | `OwnProjects` is deleted. A person's caller is `GrantGrants(session grant)`. `GrantGrants::sees(project)` answers whether any rule names the project, so a non-member's project stays a 404. `Within(token, own_projects)` becomes `Within(token, session grant)`. `maker_reaches` asks whether the maker can sign in and their session grant **now** allows what the made thing does on the project (an identity's or sync's action), instead of "owns it". Protected environments: `approvers` may name `role:admin` and `role:owner` beside principals, and an empty list stays "anyone with `admin` there". Approvals are counted **as they are when applied**, so an approver who was removed or demoted no longer counts. Reach (`…/reach`) is to list the members whose role reveals the folder (not built yet: the thirty-fifth review's item 3 in docs/secrets.md's `## Next`). |
| **code** | None in v1. Membership doesn't give access to a repo: roll code keeps its owners, orgs and tokens (`Product` routes, its own checks). Only trust changes: a project stands for a repo when `Owners` says the repo is one of the project's **owners'** (their roll code owner, or an org they manage), instead of the project creator's. |
| **previews** | Member routes (v1.1, built): `/v1/previews` is a person's route, roll-access knows a preview's project (`Resolvers::project_fn`) and that its environment, `preview`, is never protected (`protected_fn`, asked for a row's fixed environment too), so the catalogue's actions decide by role (viewer reads, developer reveals and writes, admin opens shells and manages rules). Previews drops a session's grant before its handlers, which see the person. Previews get project secrets through `counts_for_repo` → `trusts`, which changes only as code's does. |
| **apps** | Apps' routes become member routes (v1.1). Apps already puts a new service on its project's canvas (`Tgt::Create` with `place: service`); that placement now also sets the service's home. Its resolver says `production` is protected. Production deploys are `deploy` (admin and owner only). Apps still adds no token or scope checks of its own. |
| **search** | `/v1/search` stops scoping by `SignedIn` owner and goes through the evaluator (v1.1). A cluster's existing `owner` stays a source of trust (`Owners`). A cluster made in a project gets that project as its home. |
| **storage** | None in v1. Storage's people are IAM users with console sessions (its own evaluator). A bucket on a project's canvas counts for the project's secrets only through its home. Mapping project roles onto IAM is later. |
| **Postgres, Redis, streams, NoSQL, queues, tasks** | Later, one product at a time. Each needs its creates placed in a project (a `Create` row with `place`) and its resolver to find the project, and then it joins the member products. This replaces accounts' "Phase 2: per-user separation": a resource belongs to the project it was made in, not to a person. |

## The canvas rule

"A canvas proves nothing about who owns what's on it" stays true. Membership adds people to a
project. It doesn't make anything on the canvas the project's. `Workspace::trusts(project, kind,
ref)` answers yes when:

1. the project is the operator's (`owner` empty), as now; or
2. the resource's **home** is this project: it was made through this project (a create the
   evaluator placed with `X-Roll-Project`), or someone set it there who may: the operator, or a
   person whose own it is (`Owners`). Only kinds whose ids are never given out again have homes
   (`HOME_KINDS`); or
3. `Owners` says the resource is one of the project's **owners'** (role `owner`): their repos and
   their search clusters (`Owners::owns`). The person who made the project also counts as before
   members (`Owners::trusts`: everything, when they're an admin account); an admin account added
   as an owner later adds only what's its own.

A resource has at most one home (`homes`, keyed by kind and ref). Moving it to another project's
canvas doesn't move its home. Moving a home away from a project takes being that project's admin
too, in one write.

What this means for members:

- Members get rights on a resource only where the project trusts it (see
  [Evaluation](#evaluation)). A resource dropped on the canvas without a home shows as **not
  trusted**, with the reason, and no role reaches it through this project.
- A project's secrets reach a repo's checks and previews (`counts_for_repo`) only for repos the
  project trusts. Adding a member never widens that. Adding an owner can, because their repos now
  count. That's why only owners make owners.
- Removing the last owner whose repo made a repo trusted stops that repo counting at once. Health
  reports it as a finding, not as silence.

## Lifecycle

### Invite and accept

1. A project admin or owner invites an email with a role (and `environments`):
   `POST …/projects/{id}/invites`. This needs `admin` on the project's workspace resource, and the
   role must be covered by the inviter's own grant. roll mails a link
   `<web>/dashboard/invites?token=rlv_…` (checksummed, so leak scanning knows it; the page takes it
   out of the address bar). The token is shown once, stored hashed, and expires after 7 days.
2. An address without a roll account also needs an account. The default (D5) is that the invite
   waits for an operator or admin, because roll accounts are invite-only (docs/accounts.md). Once
   approved, one link creates the account and accepts the project invite.
3. The invited person signs in and **accepts** (`POST /v1/workspace/invites/{token}/accept`). This
   takes a session only, never a token. Accepting checks that the signed-in address is the
   invite's. Until it's accepted, nobody else sees the person in the member list, so an invite
   can't be used to look up who has an account.
4. The answer to an invite never says whether the address has an account (as accounts' email
   start doesn't).

### Change a role

`PATCH …/members/{user} {role?, environments?}` needs `admin`, and both the old and the new role
must be covered by the caller's grant. So an admin can't change an owner. It takes effect on the
person's next request, because nothing is cached.

### Remove, and leave

`DELETE …/members/{user}` (admin; an owner needs an owner) or `DELETE …/members/me` (any member
leaving) does three things in one workspace write:

- deletes the membership;
- refuses with 409 `last_owner` when it would leave no owner who can sign in;
- records the audit row and promises the removal event in the same transaction (the outbox).

After the write, it acts on what the person made:

| what | what happens | how |
| --- | --- | --- |
| their session and tokens | stop reaching the project on the next request | the session grant no longer has the project; `Within` intersects every token with it |
| tokens confined to the project | revoked, why "removed from project X", one audit row each | `Access::revoke_many` with the ids `confined_to(project)` finds (D7); tokens reaching other projects only narrow |
| workload identities they made | logins refused, or narrowed after a demotion | `maker_reaches` checks the identity's whole rule against their session now; the token a login gives runs `Within` it |
| syncs they made | stop pushing what they no longer reveal | each push runs as them, as they are then (`person_caller`) |
| references bound to them (#407's `bind_ref`, `resolve_as`) | stop resolving | the maker no longer reaches; health reports `maker_gone` on each, so an admin rebinds |
| dynamic leases they hold, one-time shares they made | revoked and ended | `Secrets::member_changed(project, user)` on the event, outside the workspace write |
| folders they shared with other projects | stop being shared once they may no longer manage the project | the same hook |
| backups, gateways, ACME bindings, sinks, notify URLs, rotators, outside databases and directories they set up | stop | `maker_reaches(by, [admin on the project])` on every use |
| keys they bound to hosts, placeholders made for their consumers | stop swapping (a read still gets a placeholder, never the value) | each swap asks again: the binding's `set_by` may still `admin` its folder, a placeholder's maker may still `reveal` there |
| their pending invites | revoked, or those their new role no longer gives | in the same write |
| their personal overrides | stay (only they read them) | not v1 |
| approvals they gave on open change requests | no longer count | gates are checked as they are when applied |

**Adopt.** An admin can adopt an identity, sync or bound reference that a former member made:
`POST …/identities/{id}/adopt` and its siblings. The admin becomes its maker, within their own
grant, and the audit log records the adoption. Nothing restarts on its own.

### The last owner

- The last owner who can sign in can't leave, be removed or be demoted: 409 `last_owner`.
- If every owner is disabled or deleted, the project keeps working for its other members. The
  operator or an admin then makes an owner, and the dashboard flags the project as "no owner"
  until then.
- Deleting an account removes its memberships through the same path as a removal. A project left
  without an owner is flagged the same way.

### Audit

Every membership change is in roll's audit log (`GET /v1/access/audit?product=workspace`, the SIEM
sinks), as the account events are. Each row has:

- `action`: one of `member.invite`, `member.invite_revoke`, `member.accept`, `member.role`,
  `member.remove`, `member.leave`, `home.set`, `adopt`;
- the actor and the person it's about (both as `usr_…` with handles);
- the project;
- for a role change, the old and new role and environments;
- the client's address.

Tokens revoked because of a removal carry the removal's id in `why`. roll secrets' own
tamper-evident log records the removal hook's effects (leases revoked, shares ended), not the
membership change, which isn't a secrets event.

## Migration

Today every project has an `owner` column: a person's id, or empty for the operator's.

- `projects.owner` stays, renamed in the docs to the project's **namespace**: names are unique per
  namespace, and resources' places are keyed by it. A shared project's canvas is one canvas, keyed
  by its namespace, whoever moves things on it.
- A one-time migration (`workspace-members`) inserts `(project, owner, "owner")` into `members`
  for every project with a non-empty owner, in one write. Every person keeps exactly what they had.
- The operator's projects get no members. The operator or an admin can add people to them, which
  is the main way a company shares (D8).
- Homes start empty. Trust for existing resources keeps coming from `Owners` as before, so nothing
  that worked stops working, and nothing gains trust it didn't have.
- When two projects with the same name are both visible to one person, the dashboard shows them as
  `<namespace handle>/<name>`.

## Tables

In the workspace database (SQLite `workspace.db`, or the system database's `workspace` schema):

```sql
members(project_id TEXT NOT NULL, user_id TEXT NOT NULL,
        role TEXT NOT NULL,                 -- viewer | developer | admin | owner
        environments TEXT,                  -- JSON array, or NULL for the default
        added_by TEXT NOT NULL, added_at INTEGER NOT NULL,
        PRIMARY KEY (project_id, user_id)) WITHOUT ROWID;
CREATE INDEX members_user ON members (user_id);
project_invites(id TEXT PRIMARY KEY,        -- pin_…
        project_id TEXT NOT NULL, token_hash TEXT NOT NULL UNIQUE,
        email TEXT NOT NULL, role TEXT NOT NULL, environments TEXT,
        state TEXT NOT NULL,                -- pending | waiting_admin | used | revoked
        created_by TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
        used_by TEXT, used_at INTEGER);
homes(kind TEXT NOT NULL, ref TEXT NOT NULL, project_id TEXT NOT NULL,
      set_by TEXT NOT NULL, set_at INTEGER NOT NULL, PRIMARY KEY (kind, ref)) WITHOUT ROWID;
```

Deleting a project ends its invites and keeps its members and homes apart (`deleted_members`,
`deleted_homes`) in the same write, with the project itself (docs/workspace.md): nobody's grant
names it while it's deleted, and restoring it (roll secrets' recovery window, docs/secrets.md)
brings them back as they were.

## API and UI

### Routes

All are under `/v1/workspace`, and each has a catalogue row (`crates/roll-access/src/catalog.rs`):

| route | catalogue row | answer |
| --- | --- | --- |
| `GET /projects/{id}/members` | workspace `read`, `{id}` project | `Member[]` (email only for admin) |
| `PATCH /projects/{id}/members/{user}` | workspace `admin`, `{id}` | `Member`; 403 when either role isn't covered by the caller; 409 `last_owner` |
| `DELETE /projects/{id}/members/{user}` | workspace `admin`, `{id}` | 204; 409 `last_owner` |
| `DELETE /projects/{id}/members/me` | workspace `read`, `{id}` | 204: leave; 409 `last_owner` |
| `GET /projects/{id}/invites` | workspace `admin`, `{id}` | `Invite[]` (never a token) |
| `POST /projects/{id}/invites` | workspace `admin`, `{id}` | 201 `Invite & {url}`; the link is shown once |
| `DELETE /projects/{id}/invites/{invite}` | workspace `admin`, `{id}` | 204 |
| `PUT /projects/{id}/homes/{kind}/{ref}` | workspace `admin`, `{id}`, plus `admin` outright on the resource | `{kind, ref, project}` |
| `GET /roles` | any grant (`AnyGrant`) | the built-in roles and their rules, for the builder and the members page |
| `GET /invites` | a person's session only | the signed-in person's pending invites |
| `POST /invites/{token}/accept` | a person's session only | 200 `Member` |
| `POST /invites/{token}/decline` | a person's session only | 204 |

- The last three are a person's alone, like `/v1/accounts`. They go in permissions.md's
  "Classification" table, as roll's other person-only routes do.
- `crates/rolld/tests/auth.rs` proves the rest are classified. The `members/me` route needs its own
  row, so that a viewer can leave.
- `docs/workspace.md`'s contract and `apps/dashboard/src/lib/workspace-api.ts` change together, as
  the repo's rule says.

### Dashboard

- **Members** is a tab on `/dashboard/projects/$id`, with a table of handle, role, environments,
  added by and when:
  - Invite opens a sheet with an email, a role, and environments (for a developer).
  - A row's menu offers change role, remove, and, for yourself, leave.
  - Pending invites are listed under the table, with revoke.
  - Controls a member can't use are hidden, read from `GET /roles` and the caller's role, never
    guessed. The server decides anyway.
- **Invites** (`/dashboard/invites`) is where a signed-in person accepts or declines. Home shows a
  badge while one waits.
- The project sidebar shows each shared project with its namespace handle. On the canvas, a
  resource without trust shows "not trusted here".
- The secrets Access tab lists members beside tokens. "Who can read it…" (reach) names them.

### CLI

```
roll projects members list [--project P]
roll projects members invite EMAIL --role developer [--env development,staging]
roll projects members set-role USER --role admin [--env …]
roll projects members remove USER
roll projects leave [--project P]
roll projects invites list|revoke ID
roll projects roles
roll invites list|accept TOKEN|decline TOKEN
```

The CLI signs in with `roll login` (a person's token). Accepting an invite takes a session, so
`roll invites accept` opens the browser.

### MCP

These are tools in `crates/roll-mcp`, one route each, listed in docs/mcp.md when they're built:

- `workspace_list_members`
- `workspace_invite_member`
- `workspace_set_member_role` (hinted destructive when it lowers a role)
- `workspace_remove_member` (destructive)

Accepting an invite isn't a tool, because it's a person's.

## Phasing

**v1: teams can share secrets.**

- members, invites, roles, removal, the last owner, migration;
- the session grant through the evaluator for **workspace and secrets**;
- `unprotected` rules and the developer's `environments`;
- homes for resources created through a project, and `trusts` over owners;
- removal's effects on tokens, identities, syncs, bound references, leases and shares, plus adopt;
- audit rows;
- the dashboard's Members tab and Invites page, the CLI, the MCP tools.

**v1.1: the products that already know their project.** Apps, previews and search become member
products. Previews is built (#524).

**v1.1: groups, SCIM groups and time-boxed elevation** (design: [members-v1-1.md](members-v1-1.md)).
Groups hold roles on projects, and roles add up. A company's provider keeps a group's members over
SCIM. A member asks for a role on one project or environment for a few hours, and someone else
approves it through roll-access's approvals.

**Later:**

- the other products, one at a time, as each places its creates and resolves its project;
- custom roles;
- time-boxed memberships (`expires_at` on a member, Infisical's temporary access);
- elevation to one folder or key, and approval chains in sequence;
- project admins approving `ask` requests of their project's tokens;
- storage roles mapped onto IAM.

### Tests

There is one test per flow, in the files the repo already uses:

| test | file | what it proves |
| --- | --- | --- |
| members' lifecycle | `crates/roll-workspace/tests/api.rs` | invite, accept by the right address only, change role, remove, leave, `last_owner`, the migration making every creator an owner |
| roles as rules | `crates/roll-access/tests/access.rs` | each role's grant allows and denies what the role table says; an admin can't make an owner; `unprotected` matches only unprotected environments |
| a canvas proves nothing | `crates/roll-access/tests/access.rs` | an admin of P who drops a foreign resource on P's canvas gains nothing on it; a home makes it count |
| every route classified | `crates/rolld/tests/auth.rs` | the new routes have rows; a non-member's project is a 404 for a session and for that person's token |
| secrets by role | `crates/roll-secrets/tests/api.rs` (also on Postgres) | a viewer can't reveal; a developer reveals in development, not production, and their write to a protected environment opens a request; an admin reviews; a removed approver's approval stops counting |
| removal cuts off what they made | `crates/roll-secrets/tests/api.rs` | after removal, the person's confined token is revoked, their identity's login is refused, their sync pauses, and their bound reference stops resolving; adopt brings them back under the adopter |
| two nodes | `crates/roll-secrets/tests/nodes.rs` | a removal on one node stops the person on the other at once |
| the members page | `apps/dashboard/src/lib/workspace-*.test.ts` | which controls each role is offered |
| the CLI | `crates/roll-cli/tests/` | `roll projects members …` against a live API |

### What v1 leaves out

Built: memberships, invites (mailed when the address has an account), roles, change of role,
removal and leaving, the last owner, the migration, the session grant through the evaluator for
workspace and secrets, `unprotected` rules and a developer's `environments`, homes for creates and
`trusts` over owners and homes, D7's revocation and the audit rows (through the `member_events`
outbox), approvals by role and counted as applied, what a member made running at their rights now
(`maker_reaches` takes the action and scope each made thing uses: a backup schedule needs `admin`
and `reveal` on the whole project, a gateway or an ACME binding `admin`, an identity's login every
action of its rule; so a removal or a demotion stops or narrows them at once), the Members tab and
the Invites page, the CLI, the four MCP tools.

Homes are kept only for kinds whose ids a product never gives out again (Postgres, Redis, streams,
NoSQL, search clusters, apps services: `roll_workspace::members::HOME_KINDS`). A bucket, a queues
vhost, a tasks tenant or a repo goes by a name someone else can make again after it's deleted,
and those products don't tell the workspace when they delete one, so a home kept by name would
hand the new one the old one's trust: they get no home (their trust comes from `Owners` alone).
A product that deletes a resource drops its home and canvas places (`Workspace::forget_resource`;
roll apps does on purge). A person's own grant never proves a resource is theirs: placing or
homing one takes `Owners`, or a home already in the project.

Not yet:
- **roll previews** stays owner-only (its routes reach shells, files and branches' credentials):
  only a project's owners (and admin accounts) reach its previews, and team-access previews open
  for its owners and admins, until previews decide by role (v1.1);
- deleting a removed member's personal overrides, and **adopt**;
- reach listing members beside tokens, and health's `maker_gone` findings;
- one link that makes an account and accepts (D5's waiting invite: an address without an account
  waits until roll's operator or an admin makes it, then the same link works);
- the namespace handle beside a shared project's name in the sidebar, the "not trusted here" mark
  on the canvas, Home's invites badge;
- the two-node test (`crates/roll-secrets/tests/nodes.rs`): every check reads the shared databases
  on each request, so nothing is held per node.

## Open decisions

These are the user's to make. The design above uses the first option of each.

- **D1. Role names.** (a) `viewer`, `developer`, `admin`, `owner`; (b) Infisical's `viewer`,
  `member`, `admin` plus `owner`; (c) Doppler's `viewer`, `collaborator`, `admin` plus `owner`.
- **D2. Does a viewer reveal values?** (a) No: viewer is roll's `read`, names and history only;
  (b) yes, in unprotected environments (Doppler's viewer reads secrets).
- **D3. A developer's default environments.** (a) Every environment that isn't protected, with
  production only by naming it (needs the `unprotected` rule field); (b) every environment;
  (c) none until named.
- **D4. Does a project admin see production values by default?** (a) Yes, an admin is everything
  in the project (Infisical's and Doppler's admins); (b) no, managing isn't seeing (1Password):
  admins get `admin` everywhere but `reveal` only in the environments their assignment names.
- **D5. Inviting someone without a roll account.** (a) The project admin's invite waits for the
  operator or an admin; (b) project admins invite freely; (c) only people who already have an
  account.
- **D6. Homes.** (a) A resource made through a project belongs to it, and that replaces accounts'
  per-user "Phase 2" owners; (b) keep per-user owners per product, and trust only through owners.
- **D7. Removing a member and their tokens.** (a) Revoke their tokens confined to the project at
  once, audited; (b) only narrow them (they stop reaching the project but stay listed as live).
- **D8. Members on the operator's projects.** (a) Yes, the operator or an admin adds people to
  them; (b) no, people share only projects a person made.
- **D9. roll's admin accounts.** (a) An admin stays the operator in every project; (b) an admin is
  a member like anyone else in projects, and the operator only on server-wide routes.
