# Permissions: scoped tokens, approvals and audit

roll's control plane takes one kind of delegated credential: a **scoped token**. The user builds it
by choosing products, resources, actions, an expiry and optionally the addresses it may come from.
An agent, a CI job or an MCP client then holds exactly that much power. The same token works on the
HTTP API, through the `roll` CLI and through the [MCP server](mcp.md). Scopes are enforced by the
API, once, in front of every route, so no surface can do more than another.

- Code: `crates/roll-auth` (the scope model, `roll_auth::scope`, and the gate),
  `crates/roll-access` (the store, the catalogue, the evaluator, `/v1/access`, OAuth), wired in by
  `crates/rolld/src/access.rs`. The MCP server is `crates/roll-mcp` ([mcp.md](mcp.md)).
- Client: `apps/dashboard/src/lib/access-api.ts`. Change it and the [HTTP API](#http-api) together.
- Dashboard: `/dashboard/access` ("MCP & access"), the consent screen at `/oauth/authorize`, and
  `/oauth/device`, where a code a device shows is entered.
  Each token has a page, `/dashboard/access/tokens/<id>`: what it may do in words (resources by
  name), what approvals allow it for a while and until when, the approvals it asked for, its recent
  requests, and edit, rotate and revoke.
- CLI: `roll token …` and `roll mcp …`.
- Tests: `crates/roll-access/tests/access.rs` (scoped tokens, ask, tokens making tokens, OAuth end
  to end), `crates/roll-mcp/tests/mcp.rs`, and `crates/rolld/tests/auth.rs` (every route a real
  daemon serves is classified, and a full token works where the operator's does).

## What good products do, and what roll takes from them

| product | what's good | what roll takes |
| --- | --- | --- |
| GitHub fine-grained PATs | pick resources (all, or selected repos), then a level per permission (none / read / write); mandatory expiry; an org can require approval | resources first, then a level per product; expiry by default; list results filtered to what the token can see |
| Cloudflare API token builder | policies = permission groups × resources (account, zone, or all); client IP filtering; TTL; templates | rules as permissions × resources; IP allowlist; templates ("presets") |
| Stripe restricted keys | per-resource none / read / write; keys show last used | per-action allow; last used on every token |
| Supabase MCP | `read_only` runs SQL as a read-only Postgres role; `project_ref` pins one project; feature groups | read-only SQL enforced by the database, not by parsing; project pinning; tools listed by what the token can do |
| Neon MCP | branch-based workflows; tools for run SQL, migrations on a branch | branch tools; destructive actions marked as such |
| Vercel / Linear MCP | OAuth 2.1 with dynamic client registration; consent per workspace | the MCP authorization spec end to end, with a consent screen that narrows the grant |
| MCP authorization (2025-06-18, 2025-11-25) | protected resource metadata (RFC 9728), PKCE, resource indicators (RFC 8707), DCR (RFC 7591) | all of it ([mcp.md](mcp.md#authorization)) |

Two things none of them combine and roll does: **ask** (an action that pauses until a person
approves it, the request then running as sent) and one evaluator shared by the API, the CLI and MCP.

## Principals

| credential | prefix | what it is |
| --- | --- | --- |
| operator token | (any) | `ROLL_ADMIN_TOKEN`: everything. The person who runs roll. |
| scoped token | `rlt_` | created by the operator (or a token allowed to manage access) with the builder |
| OAuth access token | `rlo_` | issued to an MCP client after the consent screen; one hour |
| OAuth refresh token | `rlf_` | rotates on every use; 30 days since its last use; a spent one used again revokes the connection |
| product credentials | `rlc_`, `rla_`, keys… | roll code owner tokens, storage IAM, stream and tasks keys: unchanged, checked by their product |

Each grant secret is its prefix, 34 random characters of lowercase base32 and a 6-character
checksum (CRC-32 of the rest), 44 characters in all. Scanners use the checksum, as GitHub's tokens
carry one, to tell a real roll token from look-alike text without asking roll
(`roll_access::checksum_ok`, `find_secrets`). Secrets made before checksums still work; they just
aren't recognised by scanners.

A scoped token and an OAuth connection are both a **grant**: one row with the rules, expiry, IP
allowlist, name and who made it. An OAuth connection is a grant whose secrets are the rotating
access and refresh tokens. Revoking the grant revokes them all at once. A refresh token, once
used, is kept as spent until it would have expired: presented again later than 30 seconds after
its use, someone else has a copy (OAuth 2.1, section 4.3.1), so the connection is revoked, with why,
and the audit log has it. Within those 30 seconds it's only refused (a client retrying after a lost
answer keeps its connection), and the audit log has that too.
Revoking a refresh token (`POST /v1/access/oauth/revoke`, RFC 7009) revokes its connection, why
"its client revoked it", so a client that signs out leaves nothing listed as live; revoking an
access token ends only that one.

### Signing the CLI in

`roll login` signs a person's CLI in through the browser, as `gh auth login` and Doppler's,
Infisical's and 1Password's CLIs do, rather than a token pasted into a shell profile:

- It registers itself as a public client (RFC 7591, named "roll CLI on <host>") with a loopback
  redirect on a port it holds (`http://127.0.0.1:<port>/callback`, RFC 8252), and opens the consent
  screen with PKCE (S256), a fresh `state`, and `--scope` (default `development`) as the preset the
  screen starts from. The person chooses what it may do there, as for any MCP client; a person's
  login waits for an admin the same way (`approve_people`).
- **The server is the API it was pointed at** (RFC 8414 3.3): its metadata's `issuer` must be the
  CLI's API URL, and the token, registration, revocation and device endpoints must be on that
  origin; otherwise it refuses, saying the address the server gives. So a link that sets
  `ROLL_API_URL=https://evil` can't have a real roll token kept under evil's address and sent
  there afterwards. The issuer is rolld's own address: `ROLL_CODE_PUBLIC_URL`, else
  `http://ROLL_PUBLIC_HOST:<API port>`; point the CLI at that one. OAuth's calls never follow a
  redirect (a 307 would re-send a refresh token).
- The answer's `state` and `iss` (RFC 9207) are checked before the code is exchanged. One whose
  `state` isn't this login's (a stray tab, a page that found the port) is ignored and the login
  keeps waiting; a connection to the port has 5 seconds to send its request line.
- **With no browser here** (`--no-browser`, an SSH session, or a browser that didn't open) it signs in
  with a code instead, as `gh auth login` does (RFC 8628's device flow): it registers as a
  device-only client (no redirect), prints a link and a code (`BCDF-GHJK`: eight letters of RFC
  8628's vowel-free alphabet, about 34 bits), and polls the token endpoint every `interval` (5 s;
  `slow_down` adds 5). The person opens `/oauth/device` signed in, on any device (a phone), enters
  the code or follows the link, and chooses what it may do on the consent screen's own form; a
  person's device waits for an admin as any of their connections does. The page says what asks,
  the network it asked from and when, and that roll never sends anyone a code to enter (device-code
  phishing, RFC 8628 5.4), and warns when it was asked from another network than the browser's.
  The person's mail names the network the device asked from, not only the one the code was
  entered from (a phished victim's own). A request lives ten minutes and is used once; one allowed
  but never collected has its grant revoked when it expires. Only a client registered without a
  redirect may use it (`unauthorized_client` otherwise: a redirect client stays bound to its
  redirect). The user code can't be looked up by a token, and after ten wrong codes in a minute
  every code answers 429 to that caller, a right one too, so guessing gets nowhere and nothing
  tells a hit from a miss. (Many IPv6 /48s could still fill the 10,000 live requests for ten
  minutes: device sign-in waits, nothing leaks.) On a roll
  without the device endpoint, the address the browser ends on is pasted back instead.
- `$BROWSER` names the browser to open, as `gh` reads it.
- `roll token whoami` says which credential runs and where it came from (`ROLL_TOKEN`,
  `ROLL_ADMIN_TOKEN`, `ROLL_API_KEY`, or `roll login` and its file), what it may do, and each other
  credential set that loses to it (`gh auth status`); with none, it says to `roll login` and exits 1.
  `--json` adds `credential: {source, unused}`.
- The login is kept in `$XDG_CONFIG_HOME/roll/logins.json` (else `~/.config`), 0600 in a 0700
  directory (written to a fresh file of a random name, never through a symlink, then renamed), one entry per API URL. Commands use it only when no credential is set for the run
  (`ROLL_TOKEN`, `ROLL_IDENTITY`, `ROLL_ADMIN_TOKEN`, `ROLL_API_KEY`, `roll code`'s and `roll
  tasks`' own tokens), refreshing it within a minute of its access token's end under a lock
  (`logins.lock`), so two commands never spend one refresh token. A login that can't be refreshed
  says so once (`run roll login`) and the command goes on without it.
- **Which roll a command goes to**: `--api`, else `ROLL_API_URL`, else the saved login's URL
  (`roll login <url>` signs in to that one). With several logins it's the one last signed in to or
  picked, said on stderr; when none is newest (logins kept before this rule), a terminal asks which
  and remembers it, and anything else is refused with the list. With no login it's a local rolld
  (`http://127.0.0.1:7070`); roll has no hosted default yet. Whichever URL it is, only that URL's
  own login goes with the command: logins are kept and looked up by the URL that issued them.
- **An `ask` the CLI meets** (an `approval_pending` answer, "Approvals" below) is printed at once
  with its approval, the page to approve it on, and a pointer to remembering it (that page's menu
  beside Approve: a longer time than "Once", or "Always"; below), so later runs in the same
  environment and folder stop asking, then sent again with `X-Roll-Approval` until it
  runs, is denied, or its ten minutes end (then it says how to approve and run it again). A request
  of a scoped or signed-in token still unanswered after 5 seconds says it may be held for a person.
- Logging in again revokes the login it replaces; `roll logout` revokes the connection (its refresh
  token) and forgets it.

Grants are made by the operator: the operator's token, or a signed-in admin (docs/accounts.md:
an admin's session acts as the operator). The grant records who made it (`created_by`: the admin's
`usr_…`, "operator" for the token, or the grant that made it), and approvals record who decided
them. A signed-in user who isn't an admin can't manage grants yet: making tokens for their own
repos, agents and projects is the next step, with this same model (their rules can only cover what
they can reach). A grant never uses a session cookie, and a cookie never carries a grant.

Data planes keep their own credentials (Postgres passwords, Redis users, S3 keys, SASL). A scoped
token never opens a data plane. It can be allowed to **reveal** those credentials, which is then
an explicit action of its own.

### Machine clients

Automation with no person (CI, a scheduled agent) uses OAuth client credentials
(`io.modelcontextprotocol/oauth-client-credentials`): it trades a credential at the token
endpoint for an hour's access token, rather than holding a long-lived token in a header.

- **Making one:** the operator, or a person signed in, calls `POST /v1/access/clients {name,
  rules, expires_at?, public_jwk?}`. The rules are exactly a token's, and so is the token policy (maximum life, an
  expiry it must have).
- **What it is:** a confidential OAuth client plus one OAuth grant holding those rules. Each token
  it gets is one more access secret on that grant, so revoking the client
  (`DELETE /v1/access/clients/{id}`) ends them all, and grants are looked up per request as
  always.
- **How it proves itself:**
  - **`private_key_jwt`** (preferred: nothing secret is stored): a JWT signed with the key whose
    public half (`public_jwk`: Ed25519, P-256 or RSA of 2048 bits or more) was registered. It
    needs `iss` = `sub` = the client id, `aud` the token endpoint or the issuer, at most five
    minutes to live, and a `jti` never used before (a replay is refused). A JWK carrying private
    material is refused.
  - **`client_secret_basic` or `client_secret_post`:** a secret roll makes (`rlc_…`, shown once,
    stored as its hash, with a checksum, so a leaked one is found and revokes its client, as a
    leaked token does).
  - **A trusted identity** (trusted publishing, as PyPI's and npm's; AWS's and GCP's workload
    federation): CI's own OIDC token, so nothing is stored or held anywhere.
    - **Making one:** `oidc: {issuer, audience?, subject, claims?, jwks?}` in place of a key or a
      secret. The audience defaults to roll's API. `subject` is the token's `sub` or a pattern with
      `*`. A shared issuer (GitHub Actions, GitLab.com, Terraform Cloud, Buildkite, Google) must
      name one tenant literally (`repo:acme/web:…`), or anyone's job could sign in, and that rule is
      never optional. `claims` are up to ten others that must match (`repository_owner_id`, `ref`).
      `jwks` pins the issuer's keys for one roll can't reach.
    - **Signing in:** RFC 7523's grant,
      `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&client_id=mcl_…&assertion=<the OIDC token>`.
      The client id is named because a shared issuer's token says nothing about which roll client
      it's for.
    - **What's checked:** `crates/roll-oidc`, the same code roll secrets' workload identities use
      (moved there with the secrets thread's review): RS256 or ES256, `iss` exactly, `aud`, `exp`
      and `nbf` with 60 s of leeway, a lifetime of at most a day, then the subject and the claims.
      Keys are fetched only from the issuer named, through rolld's guarded client (public
      addresses, no redirects, bounded reads), cached an hour, and refetched early for a new key
      id at most every 30 s. Sign-ins are capped at 120 a minute per client. A refusal is
      `invalid_grant`, and its text never holds the token.
    - **From the CLI:** `roll token client create ci --product apps --allow read,deploy --github
      acme/web [--ref refs/heads/main | --environment production]` prints the workflow step that
      trades the job's token and exports a masked `ROLL_TOKEN`; `--gitlab group/project [--ref main
      | --environment production]` prints the `.gitlab-ci.yml` lines (`id_tokens` for roll's
      audience). The panel's "GitHub Actions" choice does the same for GitHub. Any other CI:
      `--issuer URL --subject PATTERN [--claim name=value]… [--audience A] [--jwks file]`
      (Buildkite, CircleCI, Terraform Cloud, a cluster's own with its keys pinned).
- **The request:** `grant_type=client_credentials`; its `resource`, if any, must be roll's API or
  MCP server. `scope=read-only` narrows that token to its rules' reads (no asks either), checked
  wherever the token is looked up; no scope (or `granted`) is the client's rules, and any other
  scope is `invalid_scope`: never wider. The answer has no refresh token: the client trades its credential again for the
  next one. Every failure to authenticate is the same `invalid_client`, so an unknown client and a
  wrong credential look alike. A client gets at most 120 tokens a minute per node, counted once its
  credential checks out, so junk naming it can't lock it out and a leaked credential can't mint
  without end.
- **The metadata** lists `client_secret_basic`, `client_secret_post` and `private_key_jwt`, the
  signature algorithms (`EdDSA`, `ES256`, `RS256`) and the grant type.
- **In the audit log** (source `access`, on the client's grant): `create_client`, `rotate_client`,
  `revoke_client` with who did it, and a `sign_in` row for every token a client gets. That row
  holds the client, the kind of credential (`secret`, `key`, or `oidc` with the CI token's issuer
  and subject, such as `repo:acme/web:ref:refs/heads/main`), whether it was read-only, and the
  address it came from. It never holds a secret or a token.
- **In the checkup:** a client that signs in with a secret is `secret_client`, with the fix to
  rotate to a key or to CI's own identity, as GitHub nudges secrets toward OIDC. Its sign-ins count
  as use, so one that only holds its token for later steps isn't `unused`.
- **Rotating one:** `POST /v1/access/clients/{id}/rotate {public_jwk? | oidc?, keep_old_for_s?}` (or
  `roll token client rotate <id> [--public-key f | --github owner/repo … | --gitlab … |
  --issuer …] [--keep-old 1h]`, which moves a client on a secret to CI's own identity, the
  checkup's fix) gives it a new key, or a new
  secret shown once, keeping its id and rules. The old credential stops at once, and so do the
  tokens it got, unless `keep_old_for_s` (at most a week) keeps it working while the automation
  moves over. A leaked old secret still revokes the client while it works.
- **From the CLI:** `roll token client create <name> [rules] [--public-key ci.pub.pem]` (rules
  as `roll token create` takes them; the key an openssl PEM, Ed25519 or P-256, or a JWK; without
  one, roll makes a secret and shows it once), `roll token client list` and
  `roll token client revoke <id>`.
- **Who:** the operator makes, lists and revokes every machine client. A person signed in makes,
  lists and revokes their own, which act as them, as their tokens do:
  - within their account's reach;
  - past the policy's `approve_people`, only once an admin approves (no token until then);
  - stopping when they can't sign in.

  They're mailed when one is added. A token never reaches these routes, so a credential can't
  mint another kind of credential.
- **Tests:** `roll-access/tests/access.rs::a_machine_client_trades_its_credential_for_a_token_its_rules_bound`
  (secret and key, rules bounding the token, a replayed assertion, a leaked secret, a revoked
  client); `a_persons_machine_client_acts_as_them`; `a_ci_job_signs_in_with_its_own_oidc_token`
  (the right repository and branch sign in, within the rules; another repository, branch, audience,
  key or an expired token is `invalid_grant`; a shared issuer's wildcard tenant is refused); `roll-cli/tests/token.rs::a_machine_client_from_the_cli_trades_its_secret_for_a_token_held_to_its_rules`.

### Enterprise identity providers

A company's identity provider can decide which of its people may use an app with roll, as MCP's
enterprise-managed authorization (SEP-990) and the IETF's Identity Assertion JWT Authorization
Grant ("ID-JAG", Okta's Cross App Access) describe: the app trades the provider's assertion for a
roll token acting as that person, with no consent screen and no roll sign-in.

- **Trusting a provider** (the operator): `PUT /v1/access/enterprise/idps {idps: [{issuer, jwks?,
  matching?, domains?, admins?, name?, tenant?, sso?}]}` (`roll token enterprise trust <issuer> …`). `issuer` is an
  `https` URL; its keys are fetched from its metadata through the guarded client, or pinned
  (`jwks`). `matching` says how a person is found: `email` (the default: the assertion's `email`,
  with `email_verified: true`, in one of the provider's `domains`, which email matching requires,
  ASCII only, is their roll account's) or `aud_sub` (the assertion names the roll account,
  `usr_…`). A provider names people of its own company, never anyone's: some let people set their
  own email, so an unverified one or another domain is `access_denied`. roll's admins act as the
  operator, past roll's own second factor: a provider signs them in only with `admins: true`.
  The same provider as people's single sign-on is set up once: `sso` names its SSO connection
  (docs/accounts.md, "SSO"; `--sso`, or the form's "same provider" choice), whose issuer it must
  be, and then it vouches for the domains that connection's DNS records proved, read at each
  sign-in instead of `domains` (a domain whose proof is gone, or a removed connection, trusts
  nobody).
  A multi-tenant issuer (one issuer URL many companies share) names the person's tenant in each
  assertion: roll trusts the issuer with one `tenant` (`--tenant`), and refuses that issuer's other
  tenants, and any assertion naming a tenant from a provider trusted without one (anyone can be a
  tenant there, and set up an address in your domain). An assertion has exactly one audience
  (roll's issuer, a string or an array of one), and the token is held to its `resource`: roll's
  MCP server alone makes a token for that server only (refused on the API directly); roll's API,
  both, or none makes an API token. roll's metadata advertises the ID-JAG grant profile
  (`authorization_grant_profiles_supported`, `id_jag_signing_alg_values_supported`: RS256, ES256),
  never which providers it trusts.
- **Registering an app** (the operator): `POST /v1/access/enterprise/apps {name, rules,
  public_jwk?}` answers its `client_id` (`ent_…`, what the provider puts in an assertion) and, with
  no key, a `client_secret` shown once. `rules` is the most the app may do for a person.
  `GET /v1/access/enterprise` lists both (never a secret) with roll's `audience` (what a provider
  puts in `aud`) and `token_endpoint`; `DELETE …/apps/{id}` removes an app and revokes every
  person's connection to it. `PATCH …/apps/{id} {name?, rules?}` changes an app; its people's
  connections take new rules in the same write, so narrowing it narrows every live token at once.
  `POST …/apps/{id}/rotate {public_jwk?, keep_old_for_s?}` gives it a new key, or a new secret
  answered once, keeping its client id and its people's connections; the old secret works up to a
  week longer while the provider moves over, or stops now with every token its people hold.
- **A leaked app secret** (an `rlc_` with its checksum, found by roll code's push scan or reported
  by GitHub's secret scanning) is revoked like any other: roll drops the secret, the one kept from
  before a rotation, and every token the app's people hold, in one write, and audits it. The
  connections stay; the app signs in again once the operator rotates it (`leaked` in the listing
  says where it was found; a leaked secret is never kept through that rotation).
- The access overview's "Enterprise sign-in" panel does all of this, and `roll token enterprise`
  (`trust`, `untrust`, `app create|update|rotate|remove`).
- **Signing in:** `POST /v1/access/oauth/token` with `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`,
  `assertion=<ID-JAG>` (header `typ: oauth-id-jag+jwt`) and the app's own authentication
  (`client_secret_basic`, `client_secret_post` or `private_key_jwt`); an assertion alone is
  `invalid_client`. roll checks it was signed by a trusted issuer, `aud` is roll (its API URL),
  `client_id` is the app that authenticated, it hasn't expired, its `jti` is new, and its
  `resource` (if any) is roll's API or MCP server; anything else is `invalid_grant`. A person with
  no live roll account is `access_denied`.
- **What it gets:** the person's own connection to the app (an OAuth grant, made the first time:
  the app's rules, acting as the person, so never past what their account reaches; waiting for an
  admin when roll's people policy says so, `access_denied` until then) and an hour's access token
  on it. A connection revoked or denied in roll stays so (the provider can't hand it back); one that
  lapsed (expired, or idle past roll's policy) is replaced. No refresh token: the app asks the provider again. `scope: read-only` in the assertion
  narrows the token to reads. Each sign-in is a `sign_in` row in the audit log with the provider
  and the person's subject there.
- Revoking the person's connection ends its tokens and keeps them out of that app; removing the
  app ends all of them (in the same write that removes it); un-trusting the provider stops new
  ones. A used assertion's `jti` is kept past the verifier's clock leeway, so it never works twice.

### When a token is refused

A refused request's answer says what it needed (`needs: {product, action, resource}`) and where a
person can allow it: `widen`, the token's own page with that one rule proposed
(`/dashboard/access/tokens/{id}?add=postgres:delete&id=prj_a`). MCP's refusal text carries the
link, so the agent can hand it to the person. The page shows the rule and asks: "Allow", "Ask me
each time" or "Not now". Nothing widens until they choose, the edit is theirs (the same route as
any change to the token, audited as one), and a link never proposes a wildcard product. A workload's
token, which is fixed, gets no link.

## The grant

```ts
type Grant = {
  id: string;            // tok_…
  name: string;          // "ci", "cursor on my laptop"
  kind: "token" | "oauth";
  rules: Rule[];         // at least one; nothing else is allowed
  expires_at: number | null; // unix ms; the builder defaults to 30 days
  // every answer to a token that expires carries Roll-Token-Expiration (RFC 3339, as GitHub's header); the CLI warns a week ahead
  not_before?: number | null; // unix ms: it works only from then on (Cloudflare's TTL start); before, 401 not_yet_valid saying when
  ip_allow: string[];    // CIDRs; empty: any address
  created_by: string;    // "operator", or the grant that made it ("tok_…")
  created_at: number;
  last_used_at: number | null;
  last_used_ip: string | null;
  revoked_at: number | null;
  client?: { id: string; name: string; uri?: string }; // OAuth only
  rate_per_minute?: number; // 1–100,000 requests a minute across the fleet (each node a share); absent: no limit
};

type Rule = {
  product: Product | "*";
  resources: "*" | Selector[];      // "*": every resource, including ones made later
  environments?: string[];          // only resources in these environments
  paths?: string[];                 // only resources under these paths (secrets' folders)
  actions: Partial<Record<Action, "allow" | "ask">>;
};

type Selector =
  | { project: string }             // a workspace project: every resource in it, as it is now
  | { id: string; index?: string }; // one resource, by its product's id; search and secrets: only some indices or encryption keys
// A selector has exactly these fields: any other is refused, never dropped.

type Product = "workspace" | "postgres" | "redis" | "storage" | "streams" | "nosql" | "queues"
  | "tasks" | "code" | "agents" | "secrets" | "apps" | "previews" | "search" | "observability"
  | "access";
type Action = "read" | "write" | "delete" | "reveal" | "deploy" | "admin";
```

### Actions

Every route is exactly one action on one product. Actions don't imply each other: `write` doesn't
include `read`, and nothing includes `delete`, `reveal`, `deploy` or `admin`. The builder fills in
the usual combinations, so this stays explicit without being tedious.

| action | means | examples |
| --- | --- | --- |
| `read` | see metadata and data | list projects, read a file, `SELECT` in a read-only transaction, peek messages, search, read logs and metrics |
| `write` | create and change | create a branch, run SQL that writes, set a Redis key, upload an object, publish, open a change |
| `delete` | destroy or make irreversible | drop a database or branch, delete a bucket or object, purge a queue, delete a topic, restore over a branch, delete a secret or a service |
| `reveal` | see a credential or a secret value | connection strings with passwords, reset a password, API keys, storage access keys, secret values |
| `deploy` | change production | land a change on a default branch, deploy, roll back or promote to production, bypass a merge queue |
| `admin` | manage access and settings | IAM users and policies, API keys, stream ACLs, owner settings, webhooks, tokens (product `access`) |

A route whose effect depends on its body is classified from the body. Comments are stripped
first, so they can't hide what runs, and anything that makes or changes credentials or privileges
(`CREATE ROLE`, `GRANT`, `SUPERUSER`, …) is `admin`. The routes are:

- Postgres SQL: `{"read_only": true}` is `read`, and the SQL runs as a role made for that one query
  that holds only `pg_read_all_data`, so Postgres refuses its writes whatever the SQL says (on a
  branch's primary; a read replica refuses read-only SQL). Anything else is `write`.
- Redis commands: commands that only read are `read`; `FLUSHDB`, `FLUSHALL`, `SWAPDB` and scripts
  (`EVAL`, `FCALL`, `FUNCTION`, `SCRIPT`) are `delete`; the server's own, Redis's `@dangerous`
  category (`ACL`, `CONFIG`, `MONITOR` and `SYNC`, which stream everyone's commands and data,
  `CLIENT`, `SLOWLOG`, `BGSAVE`, `CLUSTER`, `RESTORE` and the like), are `admin`; the rest `write`.
- NoSQL CQL: `SELECT` is `read`, `DROP` and `TRUNCATE` are `delete`, the rest `write`.
- roll code: creating a repo is `write`, but connecting a GitHub repository (`source: {github}`)
  is `admin` (GitHub installations span tenants, so listing them is admin too). Landing is
  `deploy`; `bypass: true` (over the required reviews, the merge queue and a secret-leak
  blocker) is `admin`.
- Streams: changing a stream's `retention_ms`, or a topic's `retention.*`, `cleanup.policy` or
  `delete.retention.ms`, is `delete` (a shorter retention drops records at once); other settings
  are `write`.
- Agents: adding a machine mints its machine token (`admin`, as approving a login is); sharing a
  thread makes an anonymous link to a copy of it (`reveal`, as other shares are).
- NoSQL settings (`PATCH /v1/nosql/databases/{id}`): changing the masking policy (applied at read
  time for every role, so dropping it shows a reader raw values) or turning `read_only` off is
  `admin`, as roll previews' project settings are; its other settings are `write`.
- Storage IAM: `Get…`, `List…`, `Simulate…` are `read`, the rest `admin`.
- git pushes (`POST /git/{owner}/{repo}/git-receive-pack`): from the ref updates at the start
  of the body (read without buffering the pack). Each update is one action: moving the repo's
  default branch, directly or through `refs/land/<default>` (which lands onto it), is `deploy`;
  deleting a ref `delete`; anything else `write`. A push needs **every** action its updates take
  (actions don't imply each other, so `delete` and `deploy` don't cover a new branch), and the
  approval and the audit log list the updates. A body that can't be read that far, or is
  compressed, needs `write`, `delete` and `deploy`. Fetches are `read`. git sends the token as a Basic password (any user
  name), which the gate takes like a bearer token; roll code's git server then takes the gate's
  word (`code_git::Vouched`) and records the push as `token:<name>`. An `ask` push is bound to its
  ref updates; push again once it's approved (the same push finds its approval), or wait on it with
  `git -c http.extraHeader="X-Roll-Approval: apr_…" push`.
- An apps shell (its WebSocket, and all of a session: opening, keys, screen, resizing, closing) runs
  any command where the service runs, with its variables: `admin` in every environment, as an
  agent's shell is. Waking a sleeping environment starts compute someone pays for: `write`.
- A service's settings (`PATCH …/services/{id}`) are `write` only for how much of it runs and how
  it's watched: its size, scale, health checks, suspend and sleep timers, alerts, rollout,
  restarts, affinity and a job's timeout (`APPS_WRITE_RUN`); `run.rollback_on_crash` is deploy
  (it has roll put an older deployment live by itself). Everything else is `deploy`: `source`
  and `build` (the next landing deploys them), `run.start`, `run.pre_deploy` (it runs with
  production's variables), `run.schedule` and `run.schedules` with their `timezone` and `retries`
  (commands run as jobs of the live deployment: a production job is deploy, so one on a timer
  is), `run.kind`, `run.port`, `run.volume`, `run.public` and `run.tcp` (on the internet or off
  it, at once), and any run key apps adds later until it's listed, so a new setting is never a
  quiet way into production. Rolling back to a deployment puts back the variables it went live with
  (unless `keep_variables`); it's the environment's act as writing its variables is (`deploy` in
  production, `write` elsewhere), and the answer names them without values.
- A route whose product filters its answer per environment and path itself, asking the caller's
  `Grant` for each finding, can be matched by rules limited to some environments or paths too
  (`Row::filters_itself`): a token for staging would get staging's findings, never production's,
  rather than a 403. Secrets' health and leak scan have it; secrets' cross-project routes
  (`GET /v1/secrets/search`, `GET /v1/secrets/findings`) take any grant and keep each project's
  results only where it may read. An approval on such a route is approved once (a rule from it
  would drop the token's environments).
- Linking another resource to an apps service (`PUT …/services/{id}/links/{kind}/{ref}`, or
  `links` in a new service's body) puts its address and password in the service's variables, where
  the service's code reads them: it also needs `reveal` on each linked resource (a `postgres` link
  on that Postgres project, `redis` on that database, `bucket`, `stream`, `queue`, `nosql` likewise;
  a kind roll doesn't know is refused). A worker scaled on a stream group's lag (`run.scale.stream`
  in a new service or a `PATCH`) has apps read that stream for it: `read` on it too. So does a roll
  code repo apps reads for the caller: a plan or an import of `repo`, a service built from
  `source: {kind: "repo"}` (made, or changed to it), each needs `read` on that repo, so no apps
  grant reaches a repo's code, head, stack or `roll.toml` by itself. When that decides, the approval is about the linked
  resource, so remembering it allows revealing that one (all of reveal on it, not only the link);
  when several linked resources aren't allowed outright, the approval names them all and is
  approved once. Each linked resource's reveal is its own audit row.
- Comparing a local copy with a secrets folder (`POST …/environments/{env}/diff`, by fingerprint,
  names only) tests a guess at a value: `reveal` there, as reading the values is.
- A secrets project's backup (every value, sealed to a key the caller chooses) and restore are
  `admin`, as in secrets itself.
- Secrets' imports (`PUT …/environments/{env}/imports`, `[{project?, environment, path}]`) are a
  copy from each in-project source: `write` on the importing folder plus `reveal` on every folder
  imported. Importing another project's folder is `admin` on the importing folder (that project's
  admin must export it too). Secrets also needs `admin` to change a protected environment's imports.
- An apps deploy hook (`POST`/`DELETE …/environments/{env}/hook`) is a credential that deploys
  outside roll's tokens (its URL is the secret): making, rotating or removing one is `admin`, as an
  API key is.
- Sending a project's data out of roll with a signing secret shown once is `reveal`: an apps
  webhook notification target, and an apps log drain (every service's output, POSTed signed).
- Copying another environment's values is reading them: a row with `copies_from` (apps' `PUT
  …/environments/{env}` and its `variables_from`, secrets' `POST …/environments/{env}/promote` and
  its `?from=`) also needs `reveal` on the same resource in the environment the body or query
  names, and the request gets the weaker of the two decisions. When the copy decides, the approval
  is about the environment and folder copied from, so remembering it allows exactly that. A query
  parameter the gate decides from (`?path=`, `?from=`) must appear once: a repeated one is
  `400 repeated_parameter`, since the product could read another copy of it.
- A search cluster created with a `parent` (`POST /v1/search/clusters`) is a branch of that
  cluster, a copy of its indices: it also needs `write` on the parent, as
  `POST /v1/search/clusters/{id}/branches` does there, so a token whose rules name one project
  can't branch a cluster outside it.
- Agents' machine commands, by `policy::agent_may`'s tiers: diffs and file reads `read`; working
  threads `write`; publishing with `land` `deploy`; a shell, writing files, rolling back, raising a
  thread's runtime or interaction mode, answering an agent's approval, endpoints, the gateway,
  projects, the machine's settings, and any type it doesn't know `admin`.

A public route (`Access::Public` in rolld's `auth.rs`: health, a shared link, OAuth's endpoints)
stays public for a request carrying a grant: it's served as anyone's, with no grant and no
operator marker.

Routes not in the catalogue are the operator's alone. A product that adds a route gets nothing
for scoped tokens until its row exists, and `crates/rolld/tests/auth.rs` fails until every route
rolld serves has a row (or an explicit "operator only").

### Resources

A request's resource is found from its route: a path parameter (`/v1/projects/{id}` is the
Postgres project `id`), or a lookup for ids that don't name their resource (a code change `chg_…`
is in a repo; a Postgres branch is in a project). The evaluator then knows the resource's
**product**, **id**, **workspace project** (from `crates/roll-workspace`, as it is at that moment)
and **environment** where the product says (a secrets environment, an app environment, a preview).

- `"*"` matches every resource of the product.
- `{project}` matches resources in that workspace project at the time of the request. Moving a
  resource out of the project takes it out of the token's reach at once.
- `{id}` matches one resource.
- `environments` narrows a rule to resources whose environment the product knows and is in the
  list. A resource without a known environment never matches such a rule. Secrets and apps carry
  the environment in their paths (`…/environments/{env}/…`); previews' resources are in `preview`.
  An entry ending in `*` is a prefix (`staging-*` covers `staging-eu`); `*` alone isn't one (leave
  `environments` out for all of them). A product can say what an environment named in a path is
  (`Resolver::environment`, registered per product in `Resolvers::environments`): a preview's own
  environment (`pv-12`) is `preview`, so "Previews only" reaches it.
- `paths` narrows a rule to resources with a path under one of these (`/api` covers `/api/db`, not
  `/apix`). Secrets' folders come from `?path=` (`/` without it).
- Ids are canonical: a Redis database, NoSQL database or stream named by its name is checked under
  its id, a Postgres endpoint under its project, a Redis replica under its database, and a code
  change, thread or work item under its repo (`owner/name`). tasks' queues, topics, schedules and
  functions are `queue:<name>`, `topic:<name>`, `schedule:<id>` and `function:<id>`, so a rule for
  a queue never matches a topic of the same name.

Moving a resource into or out of a workspace project changes which tokens reach it, so the
workspace's membership routes (`PUT`/`DELETE …/projects/{id}/resources/{kind}/{ref}`) need
`write` on the project **and** `admin`, allowed outright, on the resource where it is now.

Routes that aren't about one resource:

- **Lists** (`GET /v1/projects`) need the action on the product. Unless a rule allows the action
  outright on every resource, the answer is filtered to the items allowed outright: a token for
  one project sees only that project's databases, and items that would only `ask` are left out
  (a list is never held for a person).
- **Creates** (`POST /v1/projects`) need the action on the product with `"*"` or a `{project}`
  selector. With a `{project}` selector the request must name the project (`X-Roll-Project:
  prj_…`); the new resource is put in it, in the same request, before the answer returns. A token
  for one project can make databases only in that project.
- **Product-wide** routes (settings, usage of the whole product) need a rule with `"*"`.

### Credentials in answers

Many answers carry credentials besides what they're about: every Postgres project view has
connection strings with the password, Redis and NoSQL views have passwords and tokens, bucket views
have webhook secrets. Such rows name their credential fields, and unless the grant may `reveal` on
that resource, the gate replaces them in the answer before it leaves: a URL keeps everything but its
password (`postgres://roll:redacted@host/main`), anything else becomes `"[redacted]"`. A token that
may create a database without `reveal` gets the new database, not its password. `ask` for `reveal`
counts as not allowed here: an answer is never held for a person.

A list is judged item by item: a token with `reveal` on one database and `read` on every one lists
them all, and sees only that one's credentials (the others' are replaced). A preview's links
(`share_url`, in `GET /v1/previews` and `GET /v1/previews/{id}`) carry its access token, so they
are credentials too, as its QR code is.

### Storage accounts

Storage acts in the account a request names (`X-Roll-Account` or `?account=`), and bucket names
repeat across accounts:

- A bucket in a named account is `<account>/<bucket>`, so a rule
  `{product: "storage", resources: [{id: "acme/logs"}]}` reaches `logs` in account `acme` only.
  A plain bucket name (`{id: "logs"}`) means the default account's bucket.
- A named account's bucket list is filtered to the `<account>/<bucket>` ids a token has there.
- Anything else in a named account (keys, roles, IAM) is about storage as a whole: only a rule for
  every storage resource covers it.

### Search indices

A rule can grant some indices of a search cluster: `{id: "cl_1", index: "logs-*,metrics"}`
(the builder writes it `cl_1/logs-*,metrics`).

- It covers the index routes (`/v1/search/clusters/{id}/indices/{index}/…`: search, documents,
  bulk, mappings, the index itself) when every name or pattern in `{index}` is covered: a name
  listed, or one starting with what's before a listed pattern's trailing `*` (`logs-*` covers
  `logs-2025` and `logs-2025-*`, not `logs` or `l*`). Exclusions (`-logs-old`) only narrow, so they
  needn't be covered; `_all` is `*`; date math only by `*`; another cluster's indices
  (`remote:logs`) never.
- The cluster's own routes stay the cluster's: the index list, the console (`request`, any method
  and path as the cluster's owner), keys, branches and settings need a rule for the whole cluster.
- Search checks the rest at query time as the caller: a query's reads of other indices (terms
  lookups, `more_like_this`, lookup runtime fields, `indexed_shape`, templates after rendering, and
  the bodies of reindex, ES|QL, SQL and the other query-bearing APIs) and a body's `_index` values
  stay within `{index}` (search's `control.rs`, `per_index_routes`; feature/roll-search 5db7c2602
  and fae055c31), and a document an ingest pipeline moves (`_index`, `reroute`) is checked again
  as a write at the index it lands in (ea6be2f0f). ES|QL and SQL read as the caller too (0158b6d9c):
  `FROM`, wildcards, `LOOKUP JOIN` and SQL reach only the indices it may read.
- An approval about some indices names them (`cl_1/logs-a`), and remembering it allows those
  indices only.

### Secrets' encryption keys

A rule can grant some of a project's encryption keys: `{id: "prj_1", index: "payments,pay-*"}`,
names (letters, digits, `-`, `_`) and `prefix*` patterns. Unlike indices, names are literal: a
request names one key, so `_all`, a list or a `*` in a key's place reaches no key, and an approval
remembers only a request that named one. The builder writes it `prj_1/payments,pay-*` and lists
a project's keys to tick, as it lists a cluster's indices.

It covers the routes of one key (`/v1/secrets/projects/{project}/transit/{name}/…`: encrypt,
decrypt, sign, verify, HMAC, the public key; rotating, its minimum version and deleting it at
admin), at their own actions, so a service that signs with one key needs write on that key only,
not on the project. Roll secrets checks the key again itself (`Caller::need_key`). The list
(`GET …/transit`) shows such a rule only the keys it names: the row filters by index
(`filters_index`), roll-auth lets a rule limited to some indices match such a list and no other
request that names none (secrets' health and leaks filter only by environment and folder, so a
key-limited token gets a 403 there), and roll secrets asks the grant about each key
(`Grants::reaches_key`); with none reachable it's a 403. Creating a key stays the project's
(`roll_auth::scope::sub_resources` lists the products that have such names; secrets' rows name
`{name}` with `.index("name")`).

### Presets

The builder starts from a preset and the user narrows or widens it (`GET /v1/access/catalog` has
them; `project` means the workspace project the user picks).

| preset | rules |
| --- | --- |
| Read only | every product, `*`: `read` |
| Agent, asks before destructive (the consent screen's default) | every product, `*`: `read` `write`; `delete` `reveal` `deploy` ask |
| This project only | every product, the project: `read` `write`; `delete` `reveal` `deploy` ask |
| Dev environment only | every product, the project, environments `development`: `read` `write` `delete`; `reveal` ask |
| Previews only | every product, `*`, environments `preview`: `read` `write` `delete`; `reveal` ask |
| Full access | every product, `*`: every action (expiry still applies) |

## Evaluation

One function, `roll_access::evaluate`, in front of every route rolld serves. On every request:

1. **The token.** Look up the presented token's SHA-256 in the store. There is no cache, so a
   revoked token fails on its next request, on every node. The operator token and products' own
   credentials skip this; a request with neither passes or fails as it always has.
2. **The grant.** It must not be revoked or expired. The client's address must be in `ip_allow`
   when that's set (the peer address, or the edge's forwarded address when rolld is behind the
   edge, which hands the client's address over as the connection's own). `X-Forwarded-For` counts
   only from the peers in `ROLL_TRUSTED_PROXIES` (CIDRs or addresses: a load balancer in front of
   the edge), every line of it, and the client is its right-most entry that isn't one of them; an
   entry that doesn't parse stops the walk at the peer. With none listed the header counts for
   nothing, so a request that reaches the API listener directly can't name its own address. A
   wrong entry stops rolld. MCP tool calls carry the address the gate worked out, and the same
   address is what open registration's per-network limit counts.
3. **The route.** The matched route pattern and method find the catalogue row: an exact row (one
   naming the method beats `*`), else the longest prefix row. Routes that are a product's own
   router in disguise (tasks' `{*rest}`, storage's `nest_service`) are matched by the request's
   real path. The row gives the product, the action (from the body where it depends on it), and
   how to find the resource. No row: 403 `operator_only`.
4. **The rules.** Collect the rules whose product and resources match. The action's outcome is the
   most permissive among them (`allow` beats `ask`); no rule with the action means 403.
5. **Ask** pauses the request for a person ([below](#approvals)).
6. **Allow** sends the request on as the operator would make it, with the grant in its extensions
   (`roll_auth::Grant`: id, name, kind) so the audit log and products know who it was. Lists and
   creates are filtered or placed as above.

The decision is the same wherever the request comes from: the HTTP API, the CLI, or an MCP tool
(whose call is one request through the same router, with the same token).

A request forwarded to another node is evaluated again there, which is cheap (one indexed
read): roll code's repo owners get the request as it was, and the signed hops to storage's metadata
owner and the tasks cell's leader (`/internal/storage/control`, `/internal/tasks`) are decided as
the original request their header carries (`x-roll-storage-uri`, `x-roll-tasks-uri`), and only
their own product's (a storage hop carrying a workspace route is refused: another product's row
would decide while storage's handler ran it). Those routes
take only signed requests, so nobody outside the cluster can put a path there. On the owner:

- the client's address is the one the first node saw (`x-roll-code-client`, signed), so IP
  allowlists hold on a fleet;
- a node that allowed the request for a grant says so in the hop's signature
  (`x-roll-code-decided: <grant id>`, `roll_cluster::Decided`). For that grant the owner doesn't
  ask a person a second time, and doesn't log the request again; it still denies whatever the
  grant's rules deny there, and a revoked token stops there too. A hop from a node that didn't
  decide it (a public listener in front of the tasks leader) is decided in full.

On an open daemon (loopback, no `ROLL_ADMIN_TOKEN`), requests without a token still pass, and a
request **with** a scoped token is still evaluated. That keeps a well-behaved client to its rules,
but it contains nothing: whatever holds the token can drop it and act as the operator. Scoped
tokens protect a daemon that requires `ROLL_ADMIN_TOKEN`; the dashboard says so on an open one.

## Grants outside a request

A product that acts later on a grant's behalf (secrets' syncs push with their creator's access)
looks the grant up when it acts, through `roll_auth::Grants` (rolld gives it roll-access): the
grant as it is then, `None` once it's revoked, expired or gone, and `Grant::allows` for what it may
do. Never cache it.

## Approvals

An `ask` outcome creates an approval and holds the request:

```ts
type Approval = {
  id: string;            // apr_…
  grant_id: string; grant_name: string;
  product: Product; action: Action; resource: string | null;
  method: string; route: string; path: string;
  summary: string;       // "DELETE /v1/projects/{id}: drop Postgres project acme-db"
  args: Json;            // the body, secrets redacted
  tool?: string;         // the MCP tool that made the call
  state: "pending" | "approved" | "denied" | "expired" | "used";
  created_at: number; decided_at: number | null; decided_by: string | null;
  expires_at: number;    // 10 minutes after created_at
};
```

- An approval is bound to the method, path and body, to what the request is about, and to the
  headers that change it (`X-Roll-Account`, `X-Roll-Project`, `X-Roll-User`, `X-Roll-Agent`).
- The request waits up to 55 seconds for a decision. Approved, it runs exactly as sent and its answer
  is the request's answer. Denied, it gets `403 {"code": "denied"}`.
- Still pending after the wait, it gets `403 {"code": "approval_pending", "approval": "apr_…",
  "url": …}`. Sending the same request again (with `X-Roll-Approval: apr_…`, or without: the gate
  finds the grant's approval for exactly that request) waits again, is refused with the person's
  reason if they denied it (for the approval's ten minutes, rather than asking them again), or runs it
  once approved. An approval runs one request: the one whose method, path and body hash it holds.
  It can't be reused.
- A token's waiting reads and writes (two or more, none with a body to review) can be approved
  together from the header's waiting list, each once; delete, deploy, reveal and admin are
  approved one by one.
- The person decides on the dashboard (a notification in the header, and the list under
  `/dashboard/access/approvals`), with `roll mcp approvals` / `roll mcp approve <id>`, or through the
  API. Only the operator (or an admin) decides, and a person decides their own tokens' requests
  unless the policy asks for someone else ("Two-person approvals"). A grant never approves its own request, not even with `admin` on
  `access`.
- An approval not decided in 10 minutes expires.
- MCP tools hold the call open the same way, with keep-alive events, and return the pending
  approval as a result the agent can wait on (`access_wait_approval`).

### Ask for anything else

A token can be made so that what its rules don't allow asks a person instead of being refused
(`ask_otherwise`; the builder's and the consent screen's "Ask me for anything else", `roll token
create --ask-otherwise`), as Claude Code asks before anything it isn't allowed to do. Start an agent
read-only, and approve what it asks for once, for a while or always (below): the token grows with
what a person said yes to.

- It's still refused, never asked: the operator's routes (no row in the catalogue), a request from
  the wrong address, a revoked or idle token, and moving a resource between projects.
- `tools/list` shows such a token every tool of the products that are on.
- Only a token that asks may make one that does.

### Remembered approvals

Approving can also allow what's asked again, as Claude Code's and Cursor's "allow for this
session" or "always" do (`POST …/approve {remember: "1h" | "8h" | "1d" | "always"}`, the approvals
page's "Once ▾", `roll mcp approve --remember 1h`):

- It allows the request's action on its one resource (`{product, resources: [{id}], actions:
  {<action>: "allow"}}`); nothing broader. A request about the product as a whole or about
  something new ("new in a project") can only be approved once.
- For a while, the rule is the grant's `temporary`: it lapses by itself, and decisions read the
  grant's rules plus the live temporary ones. For good, it's added to the grant's `rules`, and the
  operator's token policy applies.
- A while never becomes for good: a token makes tokens covered by its own rules only, never by
  what it's allowed for now.
- Each rule for a while names the approval that allowed it. The operator takes it back before it
  lapses from the token's page or `roll token take-back <id> <approval>`
  (`DELETE /v1/access/tokens/{id}/temporary/{approval}`), and with it
  the approval when its request hasn't used it (the rule may have run that request instead).

### Two-person approvals

What matters most can need more than one approver, and named ones, as Teleport's access requests,
Vault's control groups, Infisical's approval policies and GitHub's required reviewers do. The
policy's `approval_rules` (`PUT /v1/access/policy`, `roll token policy --approval-rule
secrets:reveal@production=2`, the overview's "Require several approvers…"):

```json
{ "approval_rules": [
  { "product": "secrets", "actions": ["reveal"], "environments": ["production"], "approvals": 2 },
  { "product": "postgres", "actions": ["delete"], "approvals": 1, "approvers": ["usr_ana", "usr_ben"] }
] }
```

- Each rule covering a request is a gate it must pass: `approvals` (1–10) from its `approvers`.
  Left-out `actions` or `environments` mean all; a request about no one environment is covered
  (the stricter reading). The names match roll secrets' protected environments, whose change
  requests count approvals the same way.
- Who counts: with `approvers` left out, signed-in people (`usr_…`: admins, and a person for their
  own tokens' requests, except as below). With no accounts, a rule must name its approvers
  (`operator`): the policy refuses one that doesn't. The operator's token (`operator`) and chat approvers
  (`slack:U…`) count only where a rule names them: anyone may hold the token, and a chat id isn't
  tied to an account, so one human could otherwise count twice, or approve their own token's
  request from chat. A chat approver the operator linked to their account ("Approvals in chat")
  is that person, and counts as them.
- A person named in `approvers` sees and decides the approvals naming them, admin or not.
- An approver counts once; the person whose token asked never counts (as a change request's author
  can't approve it). A token made with the operator's token has no person: the operator approving
  its requests is the usual one-operator setup, so name people where that matters.
- Approving adds to `approved_by`. The approval stays `pending` until every gate has its approvals
  (`gates: [{approvals, approvers?, approved}]` on a pending approval, from the policy as it is
  then); the request waits on as before, and runs once. Anyone who may decide can deny it.
- Such a request is approved once: never remembered, since a rule for a while would let the next
  ones through with fewer people. A rule remembered for a while before a gate covered it asks
  again, and an approval approved before a gate came (one the same request would run with) asks
  again too. Gates are about what asks: a token's own rules that allow outright don't ask.
- The approvals page and chat cards say who has approved and how many more are needed, and the
  product hears everyone who approved (`Approved.by`).

### Access requests

A token can ask up front for one more rule for a while, saying why, as Teleport's access requests
and Entra's Privileged Identity Management do: an agent that knows it will need to drop a scratch
database asks once, rather than being refused or asking at every call.

- `POST /v1/access/requests {product, action, resource?, environment?, path?, index?, for, reason}`
  with the token itself (`access_request_access` over MCP; `roll token request --product … --allow …
  --for 1h --reason … --wait` with `ROLL_TOKEN`, which waits for the decision); `for` is `15m`, `1h`, `8h` or `1d`, and
  `reason` (up to 500 characters) is what the person reads. It answers `202` with the pending
  approval (method `GRANT`, `args: {for, reason}`); the same request again is the one waiting.
  Only a token asks, never the operator or a person (they change rules directly), and only one made
  to involve people (with "ask for anything else" or a rule that asks), as only a token that asks
  may make one that does; a workload's token, which is fixed, never does. A refusal says what the
  token could ask for (`request`, beside `widen`) only when it may. Nothing on `access` (tokens and approvals) is ever asked for:
  a token that could manage tokens could widen itself past any approval.
- The person reads its whole reach: "every postgres resource in every environment" when it names
  none.
- It's decided like any approval: on the approvals page, from chat or the CLI, under the policy's
  approval rules ("Two-person approvals"). Approved (in the same write, however it's decided), it's the rule it asked for (one action, on that
  resource or the product's every one, only where it named), until `for` from the approval, in the
  token's `temporary` rules, named by the approval (taken back like any). It can't be remembered
  for longer. What two people approved under a gate is marked so (`gated`): that gate doesn't
  make it ask again. A read-only token's rules for a while that a gate covers just don't apply.
- Nothing runs when it's approved: the agent waits with `access_wait_approval`, then goes on.

### What a product is told about an approval

A request that runs because of an approval carries `roll_auth::Approved {id, by, until}` beside its
`Grant` (`by`: who approved it, in order), so the product's own audit can say "revealed by token X, approved by Y" (roll secrets'
audit and timeline do). `until` is `None` when the approval let this one request run, and when a
rule remembered for a while allowed it, it's when that rule lapses (the longest-lasting one that
covers it). A rule remembered for good is one of the grant's rules, so it names no approval; its
change is in the access audit. The gate's own audit row names the approval either way. Only the
node that decided sets it: a request forwarded to an owner after the decision doesn't carry it.

### Approvals in chat

Approvals can also go to a Slack or Discord channel, through roll code's chat ops
(docs/code/chatops.md) and its install's bot, rather than a second integration:

- The operator picks an install, a channel and the people who may decide there
  (`PUT /v1/access/notify {"chat": {"install": "chi_…", "channel": "C…", "approvers": ["U…"]}}`,
  `roll mcp notify --install … --channel … --approver …`, or the approvals page). Each new approval is posted there as a card:
  who asks to do what, what would run (route, tool, arguments redacted), and the link. When it's
  decided, anywhere, the card is edited to say who decided what, and loses its buttons.
- A press arrives like every chat action (verified, recorded once by its id, handled by a worker;
  `code-control/src/chatops/access.rs`). roll-access decides it only when it came from the install
  approvals go to and from one of the approvers: not whoever else can see the channel (a Slack
  Connect guest, an install open to anyone), and not the install's member list, which the roll
  code owner manages rather than the operator. Anyone else is told so. `decided_by` is the
  person (`slack:U123`, `discord:123…`), or the roll account the operator linked them to
  (`"people": {"U123": "usr_…"}`, `--approver U123=usr_…`, `U123=usr_…` on the approvals page):
  a linked approver decides as that person, so they count toward the policy's approval rules
  ("Two-person approvals") and never for their own token's request.
- Once a day, the live tokens that expire within a week are posted there too (`expiry_digest`,
  from the hourly maintenance loop; one node posts it, the day claimed in the store).
- Posting is best effort: an approval lives in roll (the dashboard, the API, `roll mcp approvals`);
  chat is one more place to decide it.
- Nothing about an approval's binding changes: it still runs exactly the request it was asked
  for, once, within ten minutes of the decision.

## Audit

Every request made with a grant, every MCP tool call by anyone, and every change to tokens and
access made from a session or with the operator's token writes one audit row:

```ts
type AuditEvent = {
  id: string; at: number;
  grant_id: string | null; grant_name: string | null; // null: the operator; a change's: the token it's about
  source: "api" | "mcp" | "access" | "leak" | "workload";
  tool: string | null;
  method: string; route: string;      // the route pattern, never the path
  product: Product | null; action: Action | null; resource: string | null;
  decision: string;                    // a request's "allow" | "deny" | "ask"; a change's name
  approval: string | null;
  status: number;                      // the HTTP status the call ended with
  args: Json | null;                   // MCP arguments, redacted
  ip: string | null;
  duration_ms: number;
  client?: string;                     // what made it, as it says: an MCP client's name and version
                                       // (from initialize, carried in the session id), else its
                                       // User-Agent; one line, at most 120 characters
};
```

- **Changes** (`source: "access"`, as GitHub's audit log has `personal_access_token.create` and
  Stripe a key's history): a token made (`create`), changed, rotated, revoked, approved, denied or
  tightened, a remembered approval forgotten, a request's approval decided, a client connected
  (`connect`, by the consent screen or a device's code) or a device refused, the policy, where
  approvals go, and audit sinks. One layer on `/v1/access` writes them (`api::audit_changes`), so a
  new route's changes are recorded without anyone remembering to. `args.by` is who (`operator` or
  the person's `usr_…`), `grant_id` the token it's about (so a token's own log tells who made,
  widened and revoked it), `resource` what else it names (an approval, a sink). Only what worked is
  recorded; a grant's own requests are already the gate's rows, so they aren't written twice. A
  device's code is never kept. A connection its own client revokes (`oauth/revoke` with its
  refresh token: `roll logout`, an MCP client signing out) has a `revoke` row by `its client`. A
  person reading their tokens' rows sees who else changed one (an admin), not from where. roll's own acts have rows too: a leaked token revoked (`leak`), a
  workload's token revoked (`workload`).
- The resource id is recorded (the user needs to know which database was dropped); paths, query
  strings and headers are not.
- Arguments are redacted before they're stored: any field named like a password, secret, token,
  key, credential or value has its value replaced with `"[redacted]"`, and strings are cut at 2 KiB.
- The log is kept for 90 days (`ROLL_ACCESS_AUDIT_DAYS`; older rows are deleted 5,000 at a time,
  so a large prune never holds the database's write lock) and read through `/v1/access/audit`,
  filtered by grant, product, decision and time.
- An OAuth connection (each `roll login`, each MCP client's) that ended as long ago as the log
  keeps is forgotten with its secrets, networks and approvals, and so is a kept client no grant
  names any more: neither piles up. Scoped tokens stay listed.

Each node writes its audit rows through one writer, which takes what's queued (up to 512 rows)
and writes it in one statement (`Store::queue_audit`): a write per request, serialized, held a
token's requests to ~9k a second on one node; batched, ~110k (`gate_latency`, release, 32
callers), every row kept.

- A full queue (4,096 rows, which drain well inside sinks' 30-second settling time, so no row lands
  behind a sink's cursor) has the request write its own row: nothing is dropped.
- A batch that fails is written row by row, so one bad row loses only itself; a NUL (a path's
  `%00`) is dropped from text, since Postgres refuses it.
- At shutdown, rolld waits (up to 5 seconds) for the queue to be written. As before, these writes
  aren't synced: a crash can lose the last moment's rows.

### Audit sinks

The operator can stream the log to a SIEM (Datadog, Splunk, a Lambda): the audit page's "Stream to
a SIEM", `roll mcp sinks add https://…`, or `POST /v1/access/audit/sinks {url, from_start?}`
(`roll-access/src/sinks.rs`).

- A sink is a public HTTPS URL (roll-net's guard, no redirects, 10 s, the answer read to 64 KiB).
  It gets new rows in order, in batches of up to 200: `{sink, source: "access", events:
  AuditEvent[]}`, signed as every roll webhook is (roll code's), so one receiver takes all of
  them: `X-Roll-Signature-256: sha256=<hex HMAC-SHA256 of "{timestamp}.{body}">`,
  `X-Roll-Timestamp: <unix seconds>`, `X-Roll-Delivery` (the same when a batch is sent again, so a
  receiver drops what it has) and `X-Roll-Sink: <id>`.
- The signing secret (`whsec_…`) is shown once and kept sealed with `ROLL_SECRET_KEY`, bound to
  its sink; without the key, streaming is off. `ROLL_ACCESS_SINK_ALLOW_PRIVATE` allows plain http
  on a private address (local development only).
- Each sink keeps a cursor (the last row's time and id). Every node sweeps every 10 seconds, sending batches until a
  sink has no full one waiting (up to 5,000 rows a sink a sweep); a claim in the store keeps a
  batch to one node, and only rows 30 seconds old go (a row is written
  just after its request, so a cursor never passes one still being written). A failed batch is
  sent again 5 minutes later, or at once with "Retry now" (`POST …/sinks/{id}/retry`). Rows the
  log prunes before a sink catches up are gone. At most 10 sinks.

## Users' own tokens

With accounts (docs/accounts.md), a signed-in person who isn't an admin makes tokens and connects
MCP clients for themselves, as GitHub's fine-grained tokens act for the person who made them:

- Sudo mode (docs/accounts.md): a token made from a session (a person's, or an admin's) needs a
  sign-in or a confirmation in the last two hours, else 403 `{code: "reauth_required"}`; the
  dashboard asks "Confirm it's you" and makes it then. The operator's token and a token's own
  tokens have no session to prove.
- `POST /v1/access/tokens` with their session makes a grant with `user: "usr_…"`, and so does the
  consent screen. A token theirs makes is theirs too.
- **An admin approves what reaches far** (GitHub's organization approval of fine-grained tokens).
  The policy's `approve_people` (`["admin", "reveal"]`, set on the access page) names actions.
  A person's own token whose rules reach one of them, or that asks for anything else, is made
  `pending`: it answers 403 `pending_approval` until an admin approves it (403, not 401: an MCP
  client given a 401 signs in again, for a connection that's only waiting)
  (`POST /v1/access/tokens/{id}/approve`) or denies it (`…/deny`: revoked, "an admin denied it").
  Admins are mailed when one waits (who asked, what it may do, where to decide; never a secret).
  A person is told first: `GET /v1/access` lists `approve_people`, and the new-token page and the
  consent screen say "an admin approves this first" as soon as the rules reach one of them.
  Each decision is kept (`approved_at`, `decided_by`) and audited (source `access`, decision
  `approve` or `deny`, as GitHub's organizations log a token request approved or denied).
  Only an admin decides, never the person or a token, and the person is mailed either way
  (`roll token list --pending`, `roll token approve|deny <id>` from the CLI).
  Widening an approved token's rules past the policy makes it wait again, in the same
  transaction. A waiting token's rules can't change until an admin decides (409), so an approval
  never takes rules the admin didn't see. A person's OAuth connection waits the same way. One
  their token makes stays within that token, so it doesn't wait. A person's token made before the
  policy asked keeps working, and an admin's checkup lists it as `unapproved` to approve or deny.
  Admins are mailed at most once an hour per waiting token.
- **It acts as them, never as more.** On every request the evaluator looks the person up as they
  are now (`roll_auth::Sessions::person`, never cached): disabled or gone, the token answers 401.
  A product's own background work that looks a stored grant up (`roll_auth::Grants::grant`: a
  sync a token made) gets `None` for it then too (`Sessions::person_now`).
  An admin's token runs as the operator's, narrowed. Anyone else's token runs as their session
  would: only routes their session may call (the gate's `User`, `Product` and `Public` rules, which
  it hands the evaluator as `roll_auth::RouteAccess`), as them (`roll_auth::SignedIn`, no
  `Operator`), then narrowed again by its rules. So roll code sees their own owner (git checks the
  repo as them before it takes the gate's word), projects and search clusters are their own, and
  every other product answers `403 operator_only`, whatever the rules say.
- They see and manage only their own tokens, connections, approvals and audit rows, and **they
  decide their own token's approvals** (a token never decides one). The operator sees everyone's.
- A token or connection made from an admin's session is that admin's too: it acts as the operator
  while they're an admin, as their own session would once they aren't, and stops when their
  account does. Tokens made with the operator's token belong to nobody.
- A person holds at most 100 live tokens and connections (`USER_MAX_GRANTS`), so nobody signed in
  can fill everyone's room (10,000 live grants in all).
- The builder and the consent screen offer only what they reach (`GET /v1/access` answers
  `{kind: "user", reach: ["code", "agents", "workspace", "search"]}`).
- **Their tokens' news is mailed to them** (GitHub mails a token's owner), as well as posted
  where approvals go: a token, MCP client or device added to their account (at once, as GitHub
  mails "a new personal access token was added": what it may do, the network it came from and
  where to revoke it, so a phished device code or a stolen session reaches its victim), an
  approval one of their tokens waits on (they decide it), a token of theirs used from a new network, one roll revoked as leaked, and, once a day, the ones that expire
  within a week (made for more than a day). Each person hears of their own tokens only, a
  disabled person hears nothing (`roll_auth::Sessions::tell`, accounts' mailer).
  - **A mail holds roll's words, not anyone else's.** Names are shown as words only (`mail_name`:
    letters, digits, spaces, `-` and `_`, 40 characters), since a token's or client's name is
    anyone's text; an approval's mail names the token, the product and the action, never the
    resource or the request's arguments (a request can name anything); a leak's mail doesn't say
    where it was found (the token's page does). No secret, and every link is to roll's own pages.
  - **No floods:** one approval mail per token per ten minutes (more show on the approvals page),
    and at most 20 mails a person an hour on a node.

## Managing grants

- The operator manages grants. A grant may too when it has `admin` on product `access`, and then
  only grants it covers entirely, before and after a change: rules within its own, an expiry no
  later than its own, (when it has an IP allowlist) addresses within its own, and (when it has a
  rate limit) a limit no higher than its own. It can never make a token more powerful than it is,
  one that outlives it, one usable from elsewhere or one that may make more requests, and never
  changes its own addresses.
- A grant reviews and comments in roll code only as itself (`token:<name>`): the gate drops
  `X-Roll-User` and sets `X-Roll-Agent`.
- The operator's policy (`/v1/access/policy`, `roll token policy`) caps every grant's lifetime
  (`max_days`) and can require that every grant expires; a grant made or changed after must fit,
  OAuth connections included (GitHub's organisation token policies, Cloudflare's TTLs).
  `max_idle_days` stops a grant nobody has used for that long (since its last use, or since it was
  made): an unused credential is risk with nothing to show for it. The evaluator reads the policy at
  most every 10 seconds, so a change reaches every node within that.
- The secret is shown once, at creation. The store keeps its SHA-256, and a scoped token's `hint`
  (`rlt_…a1b2`: its prefix and last four characters, as Stripe lists keys) so a token found in a
  config or a leak report can be matched to its row; rotating changes it. The last six characters
  are a checksum of the rest, so the hint tells nothing about the secret. An OAuth connection has
  none (its tokens change every hour). The access page's filter takes a whole token pasted in and
  finds its row by the hint, in the browser: the secret is never sent.
- A product that makes tokens of its own for its caller (roll secrets' project tokens) makes them
  with `Access::create_grant` and the caller's request: the function `POST /v1/access/tokens` is,
  with the same owner, coverage, policy and caps, never a second way in. Its page lists them with
  `GET /v1/access/tokens?product=&resource=`.
- A workload that proved who it is (roll secrets' identity logins: a CI job's OIDC token) gets a
  token for 5 to 60 minutes from `Access::mint_workload`: rules within the identity's scope, owned
  by whoever set the identity up (none while they can't sign in), its `ip_allow` the identity's.
  The login is the authentication, so no caller is gated. Workload tokens (`kind: "workload"`)
  stay out of the token list (`?kind=workload` lists the latest 500), the checkup, the expiry
  digest, new-network notices and the tokens' caps (10,000 live of their own, 1,000 per owner), and go a day after
  they end. One is fixed: never edited, rotated or grown by a remembered approval (`409`), only
  revoked or left to end. It makes and changes no tokens (`403`): what it made would be a
  credential nobody set up. The product that minted it stops it with `Access::revoke_workload(id,
  why)` (its identity deleted): only a workload grant, with why and an audit row. Their requests are audited as any token's, and each mint is a row.
- `last_used_at` and `last_used_ip` are written at most once a minute per grant.

### Tighten to what it used

A token's page (or `roll token tighten <id> [--apply]`) proposes its rules narrowed to what its
requests used in the last 30 days, allowed ones and ones that ran on an approval
(`GET /v1/access/tokens/{id}/tighten?days=`, `roll_access::tighten`), as AWS's access analyzer
writes a policy from activity:

- Each proposed rule is within the rule it came from: the same product (one rule per product a `*`
  rule was used for), environments, paths and effects (`ask` stays `ask`), only the actions used,
  and the resources used, by id, when the rule named every resource or ids (at most 20).
- A rule placed by project keeps its project: a resource can leave the project, and its id would
  still match. An action used without a resource (a list, the product as a whole) keeps the rule's
  resources. What a person allowed for a while is never made permanent.
- The server checks the proposal is covered by the token's rules and passes the rules' own checks,
  else proposes them unchanged; a token that made no allowed requests is proposed unchanged (revoke
  it instead).
- Applying it (`POST …/tighten {from, days?}`, the operator's) makes the proposal again and swaps
  the rules only if they're still `from`, the rules it was shown from: a change in between is 409.
- What it went by: one audit row per request, plus one per second check (a copy's or a link's
  reveal), so an action a request needed twice isn't dropped. A request that ran on a rule for a
  while counts for a rule with the same product and action, which can keep an action that rule
  didn't need; it never widens one.

### Checkup

The access page opens with what needs attention (`GET /v1/access/checkup`, `roll_access::checkup`,
`roll token checkup`, which prints each fix's command),
as Google's security checkup and GitHub's security overview do: each live token with a finding,
most serious first, and its fix.

| finding | when | fix |
| --- | --- | --- |
| `everything` | a rule allows delete, reveal or admin on every product's resources, without asking | tighten |
| `never_expires` | no expiry | give it one |
| `unused` | no request in 30 days (or never, 30 days after it was made) | revoke |
| `narrower` | its requests in the last 30 days needed less than its rules allow (its tighten proposal is narrower; checked for up to 200 recently used tokens) | tighten |
| `roams` | used from five or more networks, with no address limit | limit its addresses |

### New networks

A token used from a network it hasn't been used from before is told where approvals go (GitHub
and Google mail a sign-in from somewhere new): a leaked token is used from somewhere new.

- A network is an address's /24 (IPv4) or /48 (IPv6), so one office or one pool of runners is one;
  an IPv4 client on a dual-stack listener (`::ffff:a.b.c.d`) is its IPv4 network.
- Its first network is where it starts, and isn't news. Once a token has been used from five
  (`ROAMING`: CI runners, phones), it roams; the message about the fifth says that's the last.
- Each node writes a token's use at most once a minute per network, so a new network is seen on its
  first request, and the row that's inserted decides what's new (two nodes tell one new network
  once). The token's page lists its networks with when each was first and last seen
  (`GET /v1/access/tokens/{id}/networks`); one unused for the log's days is forgotten.
- Names in these messages, and in the daily expiry list, are plain text: markup a person or an
  OAuth client put in a name (Slack's `<!channel>`, Discord's `[label](url)`) is dropped, and Slack
  text is escaped.

## Revoking at once

An incident's cut-off revokes many tokens in one write: `POST /v1/access/tokens/revoke {ids, why}`
(the operator's; at most 500 ids) revokes every listed token or none (an unknown id refuses the
lot), puts `why` on each (`revoked_why`), and writes an audit row per token naming who and why.
roll secrets' reach chooses the ids (the tokens and identities that can read a folder) and calls
`Access::revoke_many` in process; roll-access never matches scopes a second way for it.

## Decoy tokens

A decoy (a honeytoken, as Thinkst's canary tokens are) is a token made to be found: `POST
/v1/access/tokens {…, decoy: true}` (the builder's "A decoy"). Plant it where only an intruder would
look (an old repo, a wiki page, a backup's config):

- It never allows anything, whatever its rules say, and never waits for an admin's approval.
- Any use answers exactly as a revoked or made-up token does (401 `invalid_token`), so whoever
  holds it learns nothing, and the gate checks it before anything else (the idle-token rule would
  otherwise retire a token nobody uses).
- Each use is written on the token (`last_used_at`, `last_used_ip`), to the audit log
  (`source: "decoy"`, the route and the client), and mailed to its owner and every admin (once an
  hour per decoy and person). A decoy can't be turned into a working token.
- It's checksummed like any `rlt_`, so scanners report a leaked decoy too (roll revokes it, which
  ends the trap: plant another).

## Leaked tokens

Other products' secrets made the same way are found and revoked the same way: apps' deploy hooks
(`rlk_`, the secret in a hook's URL), previews' deploy hooks (`rlb_`) and SSO connections' SCIM tokens (`rlp_`, docs/accounts.md)
are in `SECRET_PREFIXES`, and a product registers how to revoke its own with `Access::on_leaked`
(rolld wires apps' when apps is in, previews' when they are, and accounts' always). The audit log has each revocation.


A token pasted into a file is a token anyone who reads that file holds. roll revokes one as soon as
it's found, as GitHub does for its own tokens:

- **In roll code.** Everything that lands, in any repository, is read after it lands. The scanner
  reads the store's durable land log, never inside the lander, with a cursor of its own and one
  node at a time (`code-control/src/leaks.rs`). It reads every changeset a landing added (up to
  50), and the changed files up to 1 MiB each, at most 500 files and 64 MiB a landing; a file it
  can't read is skipped, never retried forever. A file that mentions a token prefix goes to
  roll-access, which matches checksums and revokes each live token. A token found in a private
  repository says only that and the repository's id (whoever owns the token may not read it).
- **From GitHub.** `POST /v1/access/leaks/github` takes reports from GitHub's secret scanning
  partner program: tokens found in public repositories, gists and issues. Each report is signed
  with one of GitHub's ECDSA P-256 keys (`Github-Public-Key-Identifier` and
  `Github-Public-Key-Signature`, checked against
  `api.github.com/meta/public_keys/secret_scanning`: cached for an hour, fetched again at most once
  a minute and one fetch at a time, and a failed fetch leaves the keys there were). roll revokes the live tokens and answers each with its
  `token_hash` and `true_positive` or `false_positive`. An unsigned or wrongly signed report
  changes nothing.
- A revoked token answers 401 on its next request. The grant keeps `revoked_why` ("found in
  acme/site .env at 1a2b…", shown in the dashboard), and the audit log gets a row (`source:
  "leak"`, `decision: "revoke"`).

## Rate limits

A grant may have `rate_per_minute` (the builder's "Requests a minute", `roll token create --rate`,
the consent screen), like Stripe's and GitHub's per-key limits. It protects roll from a runaway
agent loop; it isn't billing.

- A GCRA per grant (`roll-access/src/rate.rs`): a full minute's worth may come at once, then one
  request every `60 s / rate`. Counted in memory on each node, with no shared write per request:
  each of N serving nodes enforces its share, the rate over N rounded up (`Access::rate_share`, N
  from the cluster's live nodes), so the fleet allows about the rate. A client whose requests all
  reach one node gets that node's share. The `RateLimit-*` headers give the share this node enforces.
- Every MCP tool call counts, as the route it calls; the `/v1/mcp` envelope doesn't, so a batch of
  calls (at most 100 messages) counts each call. A hop to an owner counts where it was decided.
- Allowed answers carry `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset` (seconds).
  Over the limit the answer is `429` `{code: "rate_limited", retry_after, limit}` with
  `Retry-After`, before anything is looked up or written: no audit row, no `last_used_at`, so a
  flood stays cheap. Over MCP the `/v1/mcp` request itself answers that `429` (the MCP SDKs raise it
  with its body, so the agent sees when to retry).

## Storage

On a single node the data is SQLite at `<home>/access.db`; with the system database
(`ROLL_SYSTEM_DATABASE`) it's that database's `access` schema, shared by every node without
leases (each read-then-write is one `code_sql` write).

```sql
grants(id PRIMARY KEY, name, kind, rules JSON, expires_at, not_before, ip_allow JSON, created_by, created_at,
       last_used_at, last_used_ip, revoked_at, client_id)
secrets(hash PRIMARY KEY, grant_id, kind /* token | access | refresh */, expires_at, created_at)
approvals(id PRIMARY KEY, grant_id, …, request_hash, state, created_at, decided_at, decided_by, expires_at)
audit(id PRIMARY KEY, at, grant_id, …)
oauth_clients(id PRIMARY KEY, name, redirect_uris JSON, uri, created_at, last_used_at)
oauth_codes(hash PRIMARY KEY, client_id, grant JSON, redirect_uri, challenge, resource, expires_at)
```

## HTTP API

Under `/v1/access`. Bodies are JSON; errors are `{"error": string, "code"?: string}`.

| route | who | what |
| --- | --- | --- |
| `GET /v1/access` | any grant | the caller: the operator, or its own grant (rules, expiry) |
| `GET /v1/access/catalog` | any grant | products, actions, presets and every classified route |
| `POST /v1/access/check` | any grant | `{method, path, body?, token?}` → `{decision, product, action, resource, route}`: what the evaluator would decide, without running it (a grant checks itself; the operator names `token`) |
| `GET /v1/access/tokens` | access read | grants, newest first (no secrets), each with `activity`: its last day's requests `{allowed, denied, asked}`; `?product=` (and `&resource=`) only those with a rule on that product by name (naming that `{id}` or `{project}`), as a product's page lists its own tokens; `?kind=workload` the workload tokens instead |
| `POST /v1/access/tokens` | access admin | `{name, rules, expires_at?, ip_allow?, rate_per_minute?, decoy?}` → 201 `{grant, token}`; `token` is shown this once; `decoy`: a honeytoken ("Decoy tokens"); 403 `reauth_required` from a session without a recent proof |
| `GET /v1/access/tokens/{id}` | access read | a grant |
| `PATCH /v1/access/tokens/{id}` | access admin | `{name?, rules?, expires_at?, ip_allow?, rate_per_minute?}`: narrow or widen (within the caller's own); `rate_per_minute: null` takes the limit off |
| `DELETE /v1/access/tokens/{id}` | access admin | revoke, at once |
| `POST /v1/access/tokens/revoke` | operator | `{ids, why}`: every listed token revoked at once, all or none (404 for an unknown id), `why` on each → `{revoked: Grant[]}` |
| `POST /v1/access/requests` | the token itself | `{product, action, resource?, environment?, path?, index?, for, reason}`: ask for one more rule for a while ("Access requests"); `202` the pending approval |
| `DELETE /v1/access/tokens/{id}/temporary/{approval}` | operator | take back what an approval allowed it for a while, and the approval if its request hasn't used it |
| `POST /v1/access/tokens/{id}/tighten` | operator | `{from, days?}`: apply the proposal, made again now, if its rules are still `from` (else 409) → the grant |
| `GET /v1/access/checkup` | access read | the live tokens that need attention: `[{grant, findings: [{kind, why, fix}]}]` ("Checkup") |
| `GET /v1/access/tokens/{id}/networks` | access read | the networks it's been used from: `[{network, first_at, last_at}]`, newest first ("New networks") |
| `GET /v1/access/tokens/{id}/tools` | access read | what an agent connected with it is offered over MCP: `{tools: [{name, title, product, read_only, destructive}], prompts, context_tokens}` (served by roll-mcp, which reads the token as the caller first); the token page shows it |
| `GET /v1/access/tokens/{id}/tighten` | access read | `?days=` (1-90, 30): `{since, days, used: [{product, action, resource, count, last_at}], rules, narrower}`: its rules narrowed to what it used ("Tighten to what it used") |
| `POST /v1/access/tokens/{id}/rotate` | access admin | `{keep_old_for_s?}`: a new secret with the same rules; the old one stops at once, or works that long (at most 604800, a week: Stripe's roll key); in its grace the old one calls products but manages no tokens (403 `rotated_secret` on `/v1/access`) → `{grant, token}`; not for OAuth connections |
| `GET /v1/access/approvals` | access read | newest first; `?state=pending`, `?grant=<id>` (one token's), `?limit=` (at most 500) |
| `GET /v1/access/approvals/{id}` | the grant that asked, or access read | an approval; `?wait=55` waits for a decision |
| `POST /v1/access/approvals/{id}/approve` | operator | `{remember?: "1h" \| "8h" \| "1d" \| "always"}`: also allow the same action on the same resource ("Remembered approvals") |
| `POST /v1/access/approvals/{id}/deny` | operator | `{reason?}` |
| `GET /v1/access/audit` | access read | `?grant=&product=&action=&decision=&source=&resource=&before=&limit=`; `resource` is who touched it: that resource or anything under it (`cl_1` finds `cl_1/logs-a`); `&format=jsonl` answers one event per line and `&format=csv` a row each (UTC times, every cell quoted; a formula character is defused with `'` at a cell's start and after a `;`, tab or `|` inside it, which some spreadsheets split on), as a download (the audit page and `roll mcp audit --format csv` or `jsonl`). Rows that name people (`usr_` ids: an account event's actor, a change's `by`, a resource) carry `actor_handle` and `resource_handle`, disabled and deleted people too, looked up for each answer; the CSV has them as its last two columns |
| `POST /v1/access/leaks/github` | public (GitHub's signature) | GitHub secret scanning reports: `[{token, type, url, source}]` → `[{token_hash, token_type, label}]`; live tokens are revoked |
| `GET /v1/access/audit/sinks` | operator | where the log streams: `Sink[]` (`{id, url, status, error, cursor_at, delivered_at, created_at, created_by}`) |
| `POST /v1/access/audit/sinks` | operator | `{url, from_start?}` → 201 `{sink, secret}`; the secret is shown this once |
| `POST /v1/access/audit/sinks/{id}/retry` | operator | a failed sink's batch at the next sweep → 204 |
| `DELETE /v1/access/audit/sinks/{id}` | operator | stop streaming → 204 |
| `GET /v1/access/enterprise` | operator | `{idps, apps}`: the trusted identity providers and apps (never a secret) |
| `PUT /v1/access/enterprise/idps` | operator | `{idps: [{issuer, jwks?, matching?, name?}]}` |
| `POST /v1/access/enterprise/apps` | operator | `{name, rules, public_jwk?}`: 201 `{client_id, client_secret?}` (the secret once) |
| `PATCH /v1/access/enterprise/apps/{id}` | operator | `{name?, rules?}`: the app as changed; its people's connections take new rules in the same write |
| `POST /v1/access/enterprise/apps/{id}/rotate` | operator | `{public_jwk?, keep_old_for_s?}`: a new key, or a new `client_secret` answered once; the old secret works that long (at most a week) or stops now with every token its people hold |
| `DELETE /v1/access/enterprise/apps/{id}` | operator | remove an app, revoking its people's connections: `{revoked}` |
| `GET /v1/access/policy` | operator | `{max_days, require_expiry, max_idle_days, approve_people, approval_rules}`: roll's rules for every grant, and what needs several approvers ("Two-person approvals") |
| `PUT /v1/access/policy` | operator | change them; a grant made, rotated, or whose rules, lifetime or addresses change, must fit (consent too) |
| `GET /v1/access/notify` | operator | where approvals are posted: `{chat: {install, channel, approvers} \| null}` |
| `PUT /v1/access/notify` | operator | `{chat: {install, channel, approvers} \| null}` |
| `GET /v1/access/oauth/clients` | access read | MCP clients with a live connection |
| `GET /v1/access/oauth/clients/{id}` | access read | a registered client (the consent screen shows it); only the operator's lookups fetch a metadata document |
| `POST /v1/access/oauth/authorize` | operator | the consent screen's answer: `{client_id, redirect_uri, code_challenge, state, resource, name, rules, expires_at?}` → `{redirect}` |
| `POST /v1/access/oauth/device_authorization` | public | RFC 8628: `client_id`, `scope` (form) → `{device_code, user_code, verification_uri, verification_uri_complete, expires_in, interval}`; 10 a minute per network |
| `GET /v1/access/oauth/devices/{code}` | operator or person | the device request a code names (any case, dash optional): `{user_code, client, scope, network, created_at, expires_at}`; 404 for a wrong one, 429 after ten wrong in a minute |
| `POST /v1/access/oauth/devices/{code}` | operator or person | allow it what the body says (`{name?, rules, expires_at?, ip_allow?, rate_per_minute?, ask_otherwise?}`, as the consent screen) → `{grant_id, pending}` |
| `DELETE /v1/access/oauth/devices/{code}` | operator or person | refuse it: its next poll answers `access_denied` |

OAuth's public endpoints (discovery, registration, token, revocation) are in [mcp.md](mcp.md#authorization).

## Classification

Every route rolld serves is in the catalogue, or deliberately left to its own credentials. At
startup rolld logs each route with its access and its classification
(`RUST_LOG=rolld::auth=debug`: `route=/v1/projects scoped=postgres access=operator`), and
`crates/rolld/tests/auth.rs` fails on any route that no grant could be classified for, outside
the list below. It also fails on a method decided only by a prefix row (`/v1/previews/{id}/`, by
method: no row of the route's own names that method; rolld logs each route's methods) that isn't
in `crates/roll-access/src/prefix-reviewed.txt` as `METHOD route`: such a route takes the prefix's
action whatever it answers (a preview's variables, with their values, were a read until they got
a row of their own), so each new one is either given its own row or looked at and listed.

A scoped token reaches a product as its operator (roll code's `Caller::Admin`), because the gate
has decided already. So a product's own admin-only check passes for whatever the catalogue lets
through. An admin-only refusal says "needs the admin token", and
`crates/roll-access/tests/admin_only.rs` finds every such message in the products' sources. It
fails unless `crates/roll-access/src/admin-only.txt` lists a request that reaches the message
and the catalogue classifies that request `admin`. Connecting a GitHub repository and landing
with `bypass` were write and deploy before this test existed.

The operator's routes (public routes, which anyone may call without a token, aren't a grant's to
classify, so the check skips them):

| routes | why |
| --- | --- |
| `/t/…`, `/_roll/tenants` | hosted tasks tenants: their own API keys and the tasks token |
| `/kafka/v3` | Confluent REST: stream API keys |
| `/git` | git smart HTTP: roll code tokens |
| `/internal/…` | signed hops between nodes |
| `/v1/code/chat`, `/v1/code/github`, `/v1/code/linear` | platforms' signed webhooks and install flows |
| `/v1/code/agents/connect`, `/v1/code/agents/login` | machines' relay link and device login |
| `/v1/code/agents/shares/{id}`, `/v1/code/names` | public pages |
| `/v1/code/review-apps` | review apps' own tokens |
| `/v1/accounts` | people's accounts (docs/accounts.md): sign-in, sessions and invites are a person's, never a token's |
| `/v1/code/mcp` | roll code's MCP server calls roll code without the gate; grants use `/v1/mcp` |
| `/v1/access/approvals/{id}/approve` and `/deny`, `/v1/access/oauth/authorize`, `/v1/access/oauth/devices/{code}`, `/v1/access/notify`, `/v1/access/policy`, `/v1/access/audit/sinks` | the operator decides |

Rows match a route by its shape, not its parameters' names (`…/secrets/{key}/value` finds the row
written `…/secrets/{name}/value`), so renaming a parameter can never change an action. To check a
branch's routes before it merges, list them as `METHOD<TAB>PATTERN` lines and run
`cargo run -p roll-access --example classify < routes.txt`: each line comes back with its product,
action and row, `NONE` for a route no grant could call, and `MISSING PARAMS` when a row's resource
or environment parameter isn't in the route.

The rest of the work list is in [mcp.md](mcp.md#next).
