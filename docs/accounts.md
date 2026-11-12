# Accounts and sign-in

roll's own accounts: people sign in to roll.sh with GitHub, Google or a one-time code sent by
email, with an authenticator app (TOTP) as a second factor. Nobody signs up without an invite.
There are no passwords.

- Code: `crates/roll-accounts` (store, flows, mail, OAuth, the `/v1/accounts` router), mounted by
  `crates/rolld/src/accounts.rs`. The session check is part of rolld's one gate,
  `crates/roll-auth` (`SignedIn`, CSRF), so every product sees a signed-in user the same way.
- Tests: `crates/roll-accounts/tests/flows.rs` (the router behind the real gate, on SQLite, with
  fake GitHub and Google servers and the file outbox) and `crates/rolld/tests/auth.rs` (every
  operator route refuses a non-admin session with 403).
- Phase 1 is the backend and this contract. The dashboard's sign-in screen and account settings
  are built against it.

Why a crate of its own: accounts need a database, HTTP clients, SES and crypto. `roll-auth` is the
gate every product links, so it stays free of all that and only defines what a session looks like
on a request (`roll_auth::SignedIn`) and the trait rolld's gate calls to resolve one
(`roll_auth::Sessions`). roll code can be off on a node, so accounts can't live in `code-control`.

## Where it runs

Accounts are always on, like workspace projects: not a product in `ROLL_PRODUCTS`.

- On a single node, the data is SQLite at `<home>/accounts.db`.
- With the system database (`ROLL_SYSTEM_DATABASE`), the data is Postgres, schema `accounts`.
  Every node serves every route. Nothing that matters is kept on a node: sessions, OAuth states,
  codes and failure counts are rows, and every read-then-write is one `code_sql` write.

## Tables

Times are unix milliseconds. Ids are a prefix and 14 random characters of `[0-9a-z]`.

```sql
users(id TEXT PRIMARY KEY,               -- usr_…
      email TEXT NOT NULL UNIQUE,        -- lowercased, verified
      handle TEXT NOT NULL UNIQUE,       -- the roll code owner's name
      name TEXT NOT NULL,
      admin INTEGER NOT NULL,            -- 1: acts as the operator
      disabled INTEGER NOT NULL,
      code_owner_id TEXT,                -- own_…, set once the owner exists
      invited_by TEXT NOT NULL,          -- 'operator' or a usr_ id
      created_at INTEGER NOT NULL)
identities(provider TEXT, subject TEXT,  -- 'github' | 'google', the provider's stable user id
           user_id TEXT NOT NULL, email TEXT NOT NULL, login TEXT NOT NULL,
           created_at INTEGER NOT NULL, PRIMARY KEY (provider, subject))
invites(id TEXT PRIMARY KEY,             -- inv_…
        token_hash TEXT NOT NULL UNIQUE, -- sha256 of the token
        email TEXT NOT NULL, admin INTEGER NOT NULL, handle TEXT,
        created_by TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
        used_at INTEGER, used_by TEXT)
sessions(token_hash TEXT PRIMARY KEY,    -- sha256 of the cookie's token
         id TEXT NOT NULL UNIQUE,        -- ses_…, what the API shows
         user_id TEXT NOT NULL,
         level TEXT NOT NULL,            -- 'full' | 'mfa' (second factor still owed)
         created_at INTEGER NOT NULL, seen_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
         user_agent TEXT NOT NULL)
email_codes(id TEXT PRIMARY KEY,         -- chl_…
            email TEXT NOT NULL, user_id TEXT, invite_id TEXT,
            code_hash TEXT NOT NULL,     -- argon2id
            attempts INTEGER NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL)
oauth_states(state_hash TEXT PRIMARY KEY, provider TEXT NOT NULL, intent TEXT NOT NULL,
             user_id TEXT, invite_id TEXT, verifier TEXT NOT NULL, nonce TEXT NOT NULL,
             browser_hash TEXT NOT NULL, return_to TEXT NOT NULL,
             created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL)
totp(user_id TEXT PRIMARY KEY, seed TEXT NOT NULL,  -- sealed with ROLL_SECRET_KEY
     enabled INTEGER NOT NULL, last_step INTEGER NOT NULL, created_at INTEGER NOT NULL)
recovery_codes(user_id TEXT, code_hash TEXT, PRIMARY KEY (user_id, code_hash))
passkeys(id TEXT PRIMARY KEY, user_id TEXT NOT NULL, credential_id TEXT NOT NULL UNIQUE,
         public_key TEXT NOT NULL, sign_count INTEGER NOT NULL, name TEXT NOT NULL,
         created_at INTEGER NOT NULL, last_used_at INTEGER)      -- phase 2, see below
throttle(key TEXT PRIMARY KEY, n INTEGER NOT NULL, since INTEGER NOT NULL)
```

## Secrets at rest

| secret | at rest | lifetime |
| --- | --- | --- |
| session token (`rls_` + 64 hex) | sha256 | 30 days, 7 days idle; 10 minutes while a second factor is owed |
| invite token (`rli_` + 64 hex) | sha256 | 7 days, single use |
| email code (6 digits) | argon2id (19 MiB, 2 passes), salted | 10 minutes, single use, 5 tries |
| recovery code (16 base32, `xxxx-xxxx-xxxx-xxxx`) | sha256 | until used or replaced |
| TOTP seed (20 bytes) | AES-256-GCM under `ROLL_SECRET_KEY`, the user id as associated data | until disabled |
| OAuth state, browser nonce | sha256 | 10 minutes, single use |
| PKCE verifier, OIDC nonce | as is (useless without the state and the browser) | 10 minutes |

None of these is ever logged, `Debug`-printed or echoed in an error. Mail bodies and addresses
are logged nowhere; rolld logs only which kind of mail went out. Expired codes, OAuth states and a
user's ended sessions are deleted by the next write of the same kind, so no sweeper runs.

## Sessions

The dashboard is at `https://roll.sh` and the API at `https://api.roll.sh`: different origins, the
same site. A session is an httpOnly cookie that only the API's host gets:

```
Set-Cookie: __Host-roll_session=<token>; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=2592000
```

- The `__Host-` prefix means no `Domain`: another `*.roll.sh` host can't set or overwrite it.
  When the API's public URL is plain `http://` (local development), the cookie is `roll_session`
  without `Secure`. Run the dashboard and the API on the same host name then
  (`VITE_ROLL_API=http://localhost:7070` with the dashboard on `http://localhost:3000`), since
  `localhost` and `127.0.0.1` are different sites.
- The browser sends it with `fetch(url, { credentials: "include" })` (and
  `new EventSource(url, { withCredentials: true })`). CORS answers the dashboard's origins
  (`ROLL_DASHBOARD_ORIGIN`) with `Access-Control-Allow-Credentials: true`.
- Every request looks the token's hash up (one read, joined with the user). Nothing is cached:
  signing out, revoking a session, disabling a user or removing their admin flag takes effect
  on the next request. `seen_at` is written at most every 5 minutes.
- A request with an `Authorization` header is judged by that header alone; the cookie is
  ignored.
- Signing in always issues a new token. A user with a second factor first gets a session at
  level `mfa`, which only the second-factor routes below accept; passing it replaces that
  session with a new `full` one.

### CSRF

`SameSite=Lax` keeps the cookie off cross-site subrequests. Inside the site, and for defence in
depth, every request that carries a session cookie and isn't `GET`, `HEAD` or `OPTIONS` must
also send:

```
X-Roll-CSRF: <csrf>
```

`csrf` comes in every answer that describes the session (`GET /v1/accounts/session` and the
sign-in answers). It's derived from the session token (base64url of
`SHA-256("roll-csrf:" ‖ token)`), so it changes with every new session and needs no storage. The
header can't be sent cross-origin without a CORS preflight, which only the dashboard's origins
pass. If the request has an `Origin` header, it must be one of the dashboard's origins or the
API's own. A missing or wrong header or origin is:

```
403 {"error": "this request needs the X-Roll-CSRF header from GET /v1/accounts/session", "code": "csrf"}
```

## What a session can reach (phase 1)

The gate (`roll_auth::check`) resolves the cookie, then:

| caller | roll code and agents (`/v1/code`) | projects (`/v1/workspace`) | `/v1/accounts` | every other product |
| --- | --- | --- | --- | --- |
| operator token | admin | all projects | everything | everything |
| admin user | admin (the gate adds `Operator`) | all projects | everything | everything |
| other user | their own owner (`Caller::Owner`, token name `session`) | their own projects | their own account | `403` |
| anonymous | public repos | `401` | sign-in routes | `401` |

The 403 is:

```
403 {"error": "your account can use roll code, agents and projects; this API is for roll's admins for now", "code": "operator_only"}
```

A route that takes a product's own credential (storage console sessions, tasks and stream API
keys) still passes with that credential in `Authorization`.

Every user gets a roll code owner of kind `user` named after their handle, created when the
account is (or on the next sign-in if roll code was off on that node). The handle is the invite's
`handle`, else the GitHub login, else the email's local part, made a valid owner name and made
unique with `-2`, `-3`, … against users and roll code's owners. Handles don't change in phase 1.

Workspace projects have an owner: the user who made them, or empty for those made with the
operator token. A user who isn't an admin lists, reads and changes only their own; admins and the
operator see all. Names are unique per owner, and a resource's place is per owner, so two users can
each put the same repository on their canvas (docs/workspace.md).

## Rate limits

Counted durably in `throttle` (they hold across restarts and nodes) and only for things that
exist (a user, an invite, a challenge), so made-up input can't grow the table.

- Email codes sent: 5 per address per hour. Past it, `POST /email/start` answers the same 202
  and sends nothing.
- Wrong email codes: 5 per code (then it's dead), and 10 per user per 15 minutes across codes
  (then every code is refused until the window passes).
- Wrong TOTP or recovery codes: 10 per user per 5 minutes.
- A TOTP code is accepted once: its time step must be newer than the last one accepted.
- Request-rate limits per client address are Cloudflare's (a rule on `/v1/accounts/*`).

## Sign-in flows

### Invite

1. The operator or an admin creates an invite for an email. roll mails a link
   `<web>/signin?invite=<token>` and returns the token once.
2. The dashboard reads the token from the link and passes it as `invite` to whichever sign-in
   the person picks. The account is created when that sign-in proves the invite's email (the
   code arrived there, or GitHub's primary verified email or Google's verified email is it).
3. The invite is used up in the same write that creates the user.

### Email code

1. `POST /v1/accounts/email/start {email, invite?}`. The answer is the same whether or not the
   address has an account: `202 {challenge}`. A code goes out only to a user's address, or to
   the address of a valid invite passed as `invite`.
2. `POST /v1/accounts/email/verify {challenge, code}`: the session cookie and the session.

### GitHub and Google

1. `POST /v1/accounts/oauth/{provider}/start {intent, invite?, return_to?}` answers
   `{url}` and sets a short browser-binding cookie (`__Host-roll_oauth`, 10 minutes). The
   dashboard then navigates the window to `url`.
2. The provider sends the browser to `GET /v1/accounts/oauth/{provider}/callback?code&state`.
   roll checks the state (single use, 10 minutes, made for this browser's cookie), exchanges the
   code with the PKCE verifier (S256), and reads the verified email:
   - GitHub (scope `read:user user:email`): the user's id and login from `GET /user`, and the
     email marked `primary` and `verified` from `GET /user/emails`.
   - Google (OpenID Connect, scope `openid email profile`): the ID token from the token endpoint
     (received directly over TLS, so its signature isn't checked; `iss`, `aud`, `exp` and the
     nonce are), with `email_verified` true.
3. It then redirects (303), with the session cookie set, to `<web><return_to>` (default
   `/dashboard`), or to `<web>/signin?step=second_factor&return_to=<return_to, encoded>` when
   the user owes a second factor. A failed sign-in goes to `<web>/signin?error=<code>`; a failed
   link goes to `<web><return_to>` with `error=<code>` added to its query:

| `error` | meaning |
| --- | --- |
| `state` | the state is unknown, used, expired, or from another browser |
| `provider` | the provider refused or failed |
| `unverified_email` | no verified email |
| `not_invited` | no account has this identity, and no invite came with the sign-in |
| `invite_email` | the invite is for another email address |
| `invite_invalid` | the invite is used, revoked or expired |
| `link_required` | an account has this email: sign in another way, then link the provider in settings |
| `linked_elsewhere` | (linking) this identity belongs to another account |
| `disabled` | the account is disabled |

A sign-in with an identity no account has never joins an existing account by email: linking
needs the person signed in (`intent: "link"`).

`return_to` must be a path starting with `/` (not `//`); anything else becomes `/dashboard`.

### Second factor

When the user has TOTP on, every sign-in (email code or OAuth) ends at level `mfa`.
`GET /v1/accounts/session` then says `mfa_required`, and the dashboard asks for a code:
`POST /v1/accounts/signin/totp {code}` or `POST /v1/accounts/signin/recovery {code}`.

Passkeys (WebAuthn) are phase 2: the `passkeys` table is in place; registration and assertion
need a CBOR/COSE parser and attestation handling that didn't fit this phase cleanly. They will
be a second factor (and later a first one) next to TOTP, through
`/v1/accounts/me/passkeys` and `/v1/accounts/signin/passkey/{options,verify}`.

## Routes

Bodies are JSON. Every error is `{"error": string}`, with a `code` where listed. Routes marked
**session** need a full session cookie (or, where it says so, the operator token); **csrf** means
the browser sends `X-Roll-CSRF` (every non-GET with a cookie does). **admin** means the operator
token or an admin's session.

### Shapes

```ts
type User = { id: string; email: string; handle: string; name: string; admin: boolean;
              disabled: boolean; created_at: number; totp: boolean;
              identities: { provider: "github" | "google"; login: string; email: string }[] };
type Session = { id: string; created_at: number; seen_at: number; expires_at: number;
                 user_agent: string; current: boolean };
type Invite = { id: string; email: string; admin: boolean; handle: string | null;
                created_by: string; created_at: number; expires_at: number;
                used_at: number | null; used_by: string | null };
type SignedIn = { status: "signed_in"; user: User; session: Session; csrf: string };
type MfaRequired = { status: "mfa_required"; methods: ("totp" | "recovery")[]; csrf: string };
```

### Public

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/accounts` | | `{providers: {email: bool, github: bool, google: bool}}`: which sign-ins are configured |
| `GET /v1/accounts/session` | | `{status: "anonymous"}`, `MfaRequired` or `SignedIn` (always 200) |
| `POST /v1/accounts/invites/peek` | `{token}` | `{email, expires_at}`; 404 for a used, revoked, expired or unknown invite |
| `POST /v1/accounts/email/start` | `{email, invite?}` | 202 `{challenge}`; 400 bad email; 503 `{code: "email_off"}` when mail isn't configured |
| `POST /v1/accounts/email/verify` | `{challenge, code}` | 200 `SignedIn` or `MfaRequired`, with `Set-Cookie`; 400 `{code: "bad_code"}` (wrong, used, expired or too many tries: one answer); 400 `{code: "invite_invalid"}`; 403 `{code: "disabled"}` |
| `POST /v1/accounts/oauth/{provider}/start` | `{intent?: "signin" \| "link", invite?, return_to?}` | `{url}` and `Set-Cookie` (the browser cookie); 400 `{code: "invite_invalid"}`; 404 provider not configured; `link` needs a session (401) |
| `GET /v1/accounts/oauth/{provider}/callback` | query `code`, `state` (or `error`) | 303 to the dashboard, see above |
| `POST /v1/accounts/signout` | | 204, clears the cookie and deletes the session (any level; csrf) |

### Second factor (a session at level `mfa`, csrf)

| Route | Body | Answer |
| --- | --- | --- |
| `POST /v1/accounts/signin/totp` | `{code}` | `SignedIn` with a new cookie; 400 `{code: "bad_code"}`; 401 without an `mfa` session |
| `POST /v1/accounts/signin/recovery` | `{code}` | `SignedIn` with a new cookie (the code is used up); 400 `{code: "bad_code"}` |

### Your account (session, csrf)

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/accounts/me` | | `User` |
| `PATCH /v1/accounts/me` | `{name}` | `User` (1–100 characters) |
| `GET /v1/accounts/me/sessions` | | `Session[]`, newest first |
| `DELETE /v1/accounts/me/sessions/{id}` | | 204; 404 if it isn't yours |
| `DELETE /v1/accounts/me/identities/{provider}` | | 204; 404 if not linked |
| `POST /v1/accounts/me/totp` | | `{secret, otpauth_url, qr_png}` (`qr_png` a `data:image/png;base64,…` URL): a new seed, not on until confirmed; 409 if on; 503 `{code: "secret_key"}` without `ROLL_SECRET_KEY` |
| `POST /v1/accounts/me/totp/confirm` | `{code}` | `{recovery_codes: string[10]}`, shown once; 400 `{code: "bad_code"}`; 404 nothing to confirm |
| `DELETE /v1/accounts/me/totp` | `{code}` (TOTP or recovery) | 204; 400 `{code: "bad_code"}` |
| `POST /v1/accounts/me/recovery-codes` | `{code}` (TOTP) | `{recovery_codes}`, replacing the old ones |

Linking: `POST /v1/accounts/oauth/{provider}/start {intent: "link"}` with the session, then the
same callback; on success it redirects to `return_to` (default `/dashboard/account`).

### Admin (operator token or an admin's session)

| Route | Body | Answer |
| --- | --- | --- |
| `POST /v1/accounts/invites` | `{email, admin?, handle?}` | 201 `Invite & {token, url}` (the token is shown once, and mailed when mail is on); 409 an account has the email |
| `GET /v1/accounts/invites` | | `Invite[]`, newest first |
| `DELETE /v1/accounts/invites/{id}` | | 204 (unused invites only; 409 once used) |
| `GET /v1/accounts/users` | | `User[]`, oldest first |
| `PATCH /v1/accounts/users/{id}` | `{admin?, disabled?}` | `User`; disabling deletes the user's sessions |
| `DELETE /v1/accounts/users/{id}/sessions` | | 204: signs the user out everywhere |

A non-admin session on these is 403 `{code: "admin_only"}`; no credentials at all is 401.

## Mail

- `ROLL_ACCOUNTS_MAIL_FROM` (e.g. `roll <noreply@roll.sh>`) turns on mail through SES v2
  (`ses:SendEmail`; the SDK's default credentials and `AWS_REGION`).
- `ROLL_ACCOUNTS_OUTBOX=<dir>` writes each mail to `<dir>/<ms>-<id>.json`
  (`{to, subject, text}`) instead: local development and tests only. It's never on unless set.
- Without either, email sign-in is off (`/email/start` is 503) and invites return their link
  for the operator to pass on.
- Mails go out after the answer is decided, off the request, so the answer's timing doesn't say
  whether an address has an account.

## Settings

| env | |
| --- | --- |
| `ROLL_ACCOUNTS_MAIL_FROM`, `ROLL_ACCOUNTS_OUTBOX` | see "Mail" |
| `ROLL_ACCOUNTS_GITHUB_CLIENT_ID`, `ROLL_ACCOUNTS_GITHUB_CLIENT_SECRET` | a GitHub OAuth app; callback `<api>/v1/accounts/oauth/github/callback` |
| `ROLL_ACCOUNTS_GOOGLE_CLIENT_ID`, `ROLL_ACCOUNTS_GOOGLE_CLIENT_SECRET` | a Google OAuth client (web); callback `<api>/v1/accounts/oauth/google/callback` |
| `ROLL_SECRET_KEY` | seals TOTP seeds; TOTP is off without it |
| `ROLL_CODE_PUBLIC_URL`, `ROLL_WEB_URL` | the API's and the dashboard's public URLs (the edge sets `https://api.<domain>` and `https://<domain>`) |
| `ROLL_DASHBOARD_ORIGIN` | the origins CORS and the CSRF check accept |

A provider is on when both its id and secret are set.

## Phase 2: per-user separation of the other products

Each product needs its resources to have an owner, and its routes to scope by the signed-in user:

- **Postgres** (`pg-control`): an owner on projects; list and every `/v1/projects/{id}`,
  `/v1/endpoints` route checks it; connection strings stay per project.
- **Redis** (`redis-control`): an owner on databases; same for `/v1/redis`.
- **Storage**: already has accounts and IAM (`s3-store/src/accounts.rs`): map a user to a storage
  account (created on first use) and sign the dashboard's requests as that account's console
  session, through the one evaluator.
- **Streams**: SASL tenancy already scopes the data plane; give each user a tenant and scope the
  control plane's `/v1/streams` to it.
- **Tasks**: hosted tenants exist (`/t/{tenant}`); give each user a tenant and route the
  dashboard to it.
- **Queues**: a vhost per user, and scope `/v1/queues` by vhost.
- **NoSQL**: an owner on databases; scope `/v1/nosql`.
- **Workspace**: done in phase 1.
- **Billing** (Home's spend): per user once the above carry owners.
- **Orgs**: users in roll code orgs, and invites into an org.
- **Passkeys**: see "Second factor".
- **Handles**: renaming a handle renames the roll code owner.
- **CLI**: `roll login` through the device flow, giving a session-backed token.
