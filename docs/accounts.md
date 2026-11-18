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
         public_key TEXT NOT NULL,                                -- COSE, base64url
         sign_count INTEGER NOT NULL, name TEXT NOT NULL,
         created_at INTEGER NOT NULL, last_used_at INTEGER, synced INTEGER)
sign_in_places(user_id TEXT, place TEXT, first_at INTEGER, last_at INTEGER,  -- new-sign-in mail
         PRIMARY KEY (user_id, place))                            -- place: sha256(browser, network)
passkey_challenges(id TEXT PRIMARY KEY, user_id TEXT, purpose TEXT NOT NULL,  -- register | signin
         challenge TEXT NOT NULL, expires_at INTEGER NOT NULL)     -- used once, 5 minutes
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
| SSO client secret | AES-256-GCM under a key of its own from `ROLL_SECRET_KEY`, the connection's id as associated data | until changed or removed |
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

Signing in has no session yet, so the header can't guard it. Instead, a write to `/v1/accounts`
without an `Authorization` header must send its body as `Content-Type: application/json` (else
`415`). A form on another site can post any text as its body, and the browser keeps the cookie the
answer sets. Without the rule, such a form could post the attacker's own email code and sign the
visitor in to the attacker's account. JSON from another origin needs a CORS preflight, which only
the dashboard's origins pass.

## What a session can reach (phase 1)

The gate (`roll_auth::check`) resolves the cookie, then:

| caller | roll code and agents (`/v1/code`) | projects (`/v1/workspace`) | search (`/v1/search`) | `/v1/accounts` | every other product |
| --- | --- | --- | --- | --- | --- |
| operator token | admin | all projects | all clusters | everything | everything |
| admin user | admin (the gate adds `Operator`) | all projects | all clusters | everything | everything |
| other user | their own owner (`Caller::Owner`, token name `session`) | their own projects | their own clusters | their own account | `403` |
| anonymous | public repos | `401` | `401` | sign-in routes | `401` |

The 403 is:

```
403 {"error": "your account can use roll code, agents, projects and search; this API is for roll's admins for now", "code": "operator_only"}
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

Tokens an admin minted for other owners while an admin stay those owners' tokens when the admin
flag is taken away: revoke them on those owners if needed.

With `ROLL_ADMIN_TOKEN` set and no `ROLL_DASHBOARD_ORIGIN`, CORS (with credentials) answers only
the dashboard's own origin, from `ROLL_WEB_URL`: never every local port, since a page there could
read the session's CSRF token.

## Rate limits

Counted durably in `throttle` (they hold across restarts and nodes) and only for things that
exist (a user, an invite, a challenge), so made-up input can't grow the table.

Each try is counted before it's checked, in the same write as the check, so tries sent in
parallel (to any node) can't get past a limit.

- Email codes sent: 5 per address per hour. Past it, `POST /email/start` answers the same 202
  and sends nothing.
- Wrong email codes: 5 per code (then it's dead), and 10 per user per 15 minutes across codes
  (then every code is refused until the window passes).
- Wrong TOTP or recovery codes: 10 per user per 5 minutes.
- A TOTP code is accepted once: its time step must be newer than the last one accepted.
- Request-rate limits per client address are Cloudflare's (a rule on `/v1/accounts/*`).
- Known limit: anyone can start challenges for an address and spend that user's 10 wrong codes
  per 15 minutes, holding off their email sign-in for the window (GitHub, Google and recovery
  codes still work). Cloudflare's per-address rule bounds how fast.

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

### SSO (a company's OpenID Connect provider)

Okta, Entra ID, Google Workspace, JumpCloud or any OpenID Connect provider signs people in, chosen
by their email's domain, as Linear's, Vercel's and GitHub's enterprise sign-in do.

- **A connection** (admin): `POST /v1/accounts/sso {name?, issuer, client_id, client_secret,
  domains, enforce?, provision?}`. The issuer's discovery document must answer and name the same
  issuer before it's saved; roll keeps the issuer as the document states it (with or without a
  trailing `/`: what its ID tokens carry). The answer has the `redirect_uri` to register at the
  provider (`<api>/v1/accounts/sso/{id}/callback`). The client secret is sealed (`ROLL_SECRET_KEY`
  is required) and never returned. A domain has one connection; public mail domains (gmail.com,
  outlook.com…) are refused.
- **Domains are proven** before they do anything: a TXT record at `_roll-sso.<domain>` with the
  connection's value (`dns` in the answer), then `POST /v1/accounts/sso/{id}/verify`. Only
  verified domains route sign-ins, link accounts, provision people or enforce. Records are read
  over DNS-over-HTTPS (Cloudflare's, or `ROLL_ACCOUNTS_DOH`). Dropping a domain from a connection
  unverifies it and removes its people's links there.
- **Signing in**: the dashboard asks for an email; `POST /v1/accounts/sso/start {email, invite?,
  return_to?}` answers the provider's authorize URL (code flow, PKCE, a nonce, `login_hint`) and
  the browser cookie, as GitHub's and Google's do (at most 10 starts per address in ten minutes,
  600 per connection a minute). roll trades the code (`client_secret_post`, or Basic where the
  provider only takes that), then checks the ID token with roll-oidc against the keys the
  discovery document names: signature, issuer exactly, audience (its client id; `azp` when there
  are several), times, and the nonce. Discovery and keys are reused for ten minutes (a provider
  that didn't answer, for one); a token signed by a key roll hasn't seen reads the keys again,
  once. Only an email the provider says it verified, in plain ASCII, in a verified domain of the
  connection, counts. Entra ID doesn't send `email_verified`: its optional claim `xms_edov` (the
  domain's owner verified the address) counts instead. Google's issuer also vouches for consumer
  accounts, so its token must carry `hd` (the Workspace domain), one of the verified domains.
- **Who it is**: a person linked before, while their account's email is still in the
  connection's verified domains; else the account with that email, linked now, unless it's an
  admin's (`link_required`: admins link from their own session, `POST /v1/accounts/sso/start
  {intent: "link"}`, and the provider must sign in the account's own address) or it's linked
  there to another subject already (`linked_elsewhere`: an address reassigned at the provider;
  the person or an admin removes the old link, `DELETE /v1/accounts/me/identities/sso:<id>`).
  With no account, an invite, or `provision: true` makes one (never an admin). TOTP still applies
  to people who turned it on.
- **Enforced** (`enforce: true`): people of its verified domains sign in only through the
  provider. Turning it on (or verifying a domain while it's on) ends their sessions that didn't
  come from SSO. An email code is refused at once (`403 sso_required`, decided by the domain
  alone, so it says nothing about whether the address has an account); GitHub, Google, or an
  email code that reaches a session is refused there, and so is a second factor owed by a
  sign-in that began another way. The operator's token is never refused, so an outage at the
  provider never locks roll out.
- The provider is fetched only through a client that refuses private addresses (and IP
  literals), uses no proxy, follows no redirects and bounds every answer
  (`ROLL_ACCOUNTS_SSO_ALLOW_PRIVATE=1` allows private addresses, and plain http, for local
  development and tests only). Failures are logged with their detail; the browser is told only
  that the provider didn't answer.
- Removing a connection deletes its people's links; their accounts stay and sign in other ways.

The same provider can sign people's agents in too (enterprise sign-in, docs/permissions.md,
"Enterprise identity providers"): an enterprise identity provider with `sso` set to the
connection's id vouches for the connection's verified domains, as they are at each sign-in
(`roll_auth::Sessions::sso_trust`), so a domain is proven once for both.

### SCIM (the provider manages its people)

A connection's provider (Okta, Entra ID, JumpCloud) creates, updates and deactivates its people
over SCIM 2.0 (RFC 7643 and 7644), so someone who leaves loses roll at once.

- An admin makes the connection a token: `POST /v1/accounts/sso/{id}/scim` answers `{token,
  base_url}` once (`rlp_…`, kept as a hash; a new one replaces it, `DELETE` turns SCIM off). The
  provider uses `base_url` (`<api>/v1/accounts/scim/v2`) and sends the token as `Authorization:
  Bearer`. It's looked up on every request.
- The token is checksummed like roll's other secrets (`rlp_`, 34 random base32 characters and 6
  of CRC-32), so leak scanning finds it: one found in a landing or reported by GitHub's secret
  scanning stops at once (`Accounts::revoke_leaked_scim`, registered with roll-access's
  `Access::on_leaked`), the audit log has it, and the connection shows where it was found
  (`scim_leaked`) until an admin makes a new token. Tokens made before checksums still work.
- It reaches only people whose email is in the connection's verified domains, and never an
  admin (admins are managed in roll): anyone else is a 404 to it. It never changes an email.
- A person's roll address is their primary email (`emails`), else a `userName` that is an email.
  The provider's `userName` (Entra's userPrincipalName, say) is kept beside it: map the work email
  to `emails[type eq "work"].value` in Entra. Filters: `userName eq` (the provider's userName or
  the address), `externalId eq`, `emails.value eq`; `startIndex` and `count` (up to 200) page,
  in the database.
- `GET /Users`, `GET /Users/{id}`, `POST /Users` (a new person in its domains, never an admin,
  made in one write; 409 `uniqueness` when the email has an account, which the provider then finds
  by filter; `Location` on the 201), `PUT` and `PATCH /Users/{id}` (name, `externalId`,
  `userName`, `active`: `"False"` and `false` both count, anything else is a 400; `remove` of what
  roll keeps nothing of is fine), `DELETE /Users/{id}` (deactivates). `ServiceProviderConfig`,
  `ResourceTypes` and `Schemas` describe this: users only, no groups, no bulk; any other path is a
  SCIM 404 (turn group provisioning off in the provider).
- Deactivating is roll's own disable, done every time it's asked (a retry finishes cutting off
  what a failure left): the person's sessions end, and their roll code owner (its tokens and
  machine links) is cut off. Their own roll tokens and connections pause with the account, and
  come back if it's reactivated. SCIM reactivates only an account it deactivated: one an admin
  disabled stays so (403 `admin_disabled`).

### Second factor

When the user has TOTP on, every sign-in (email code or OAuth) ends at level `mfa`.
`GET /v1/accounts/session` then says `mfa_required`, and the dashboard asks for a code:
`POST /v1/accounts/signin/totp {code}` or `POST /v1/accounts/signin/recovery {code}`.

A passkey (below) gives the second factor too: `methods` lists `passkey` when the person has one.

### Passkeys

Passkeys (WebAuthn Level 3, `src/passkeys.rs`) sign a person in without a code, and can't be
phished: a passkey works only on roll's own site.

- **Made** on the account page: `POST /v1/accounts/me/passkeys/options` answers `{id, publicKey}`
  for `navigator.credentials.create`, and `POST /v1/accounts/me/passkeys {id, name, credential}`
  keeps it. The options ask for a discoverable credential, ES256, EdDSA or RS256 (2048 bits and
  up), and no attestation: roll trusts no attestation statement (whatever `fmt` a browser sends,
  it's ignored), so any authenticator works and none is vouched for. At most 20 per person.
- **On its own** (`POST /v1/accounts/signin/passkey/options` with no session, then
  `POST /v1/accounts/signin/passkey {id, credential}`): any passkey the browser holds for roll
  (user verification `required`) signs its person in, as both factors, TOTP on or not (GitHub
  counts a passkey the same way). An authenticator that didn't verify the person (no PIN or
  biometric) is refused. An enforced SSO domain still signs in only through its provider.
- **As the second factor** (the same two routes with a session owing one): the person's own
  passkeys, user verification `preferred` (presence is enough on top of the first factor).
- A challenge is used once, within five minutes, and only for the sign-in it was made for: one
  made for a second factor never signs anyone in on its own, and one made without a session
  never finishes someone's second factor.
- Checked every time: the ceremony type, the challenge, the origin (the dashboard's,
  `ROLL_WEB_URL`; never inside another site's frame), the RP id hash (the dashboard's host), user
  presence, the signature over the authenticator data and the client data's hash, and the sign
  count: a count that doesn't go up when either side counts means a copied key, refused (synced
  passkeys count 0 and stay there). `synced` shows whether the passkey is backed up across the
  person's devices (the authenticator's `BS` flag).
- Everything from the browser is hostile: the CBOR reader takes definite lengths only, believes
  a length or count only as far as the bytes present hold it, and stops at a small depth; a
  credential id is at most 1023 bytes; the blobs at most 16 KiB.
- Anonymous challenges are throttled per address (60 every 10 minutes). Passkeys need
  `ROLL_WEB_URL` (503 `{code: "passkeys_off"}` without it); `GET /v1/accounts` says
  `providers.passkey`.
- A refused passkey sign-in is in the passkey owner's security log (by `anonymous` when no
  session owed a second factor); adding, renaming and removing one are there too.

### Recovery

Someone who lost their authenticator app and their recovery codes asks an admin to reset their
second factor, as Okta's and Google Workspace's admins reset factors (`src/recovery.rs`):

- They sign in with their first factor (an email code, GitHub, Google), then, on the
  second-factor step, ask with a reason: `POST /v1/accounts/signin/reset-request {reason}` (a
  session owing a second factor; 1–500 characters). Their one pending request is answered again
  rather than a second made; three a day at most.
- Every other admin is mailed the request and its reason, with a link to the account page's
  "Second-factor resets". An admin checks it's them some other way (a call, in person: the email
  code alone got them this far), then approves or declines (sudo mode; never their own).
- Approving takes their TOTP and recovery codes away and ends their sign-ins waiting for one;
  their next sign-in is whole and they set it up again. They're mailed either way, and each step
  is in their security log (`recovery.request`, `recovery.approve`, `recovery.deny`).
- Passkeys aren't taken: one still works on its own.

### New sign-ins

As GitHub does, the first whole sign-in (nothing more owed) from a browser and network a person
hasn't signed in from before mails them: "A new sign-in to your roll account", with the browser
in words ("Firefox on Windows"), the address, and a link to their Recent activity. A stolen code
or link shows up in their inbox, not only in their security log.

- A place is the browser and OS from the User-Agent plus the network (an IPv4 /24, an IPv6 /48),
  kept hashed in `sign_in_places` (the newest 50 per person), so a new address at the same home
  or office isn't new.
- A person's first sign-in mails nothing, and neither does the first after this shipped (nobody
  is mailed for what they did before). Without mail configured nothing is sent.
- The mail goes out off the request, after the sign-in's answer.

### Sudo mode

As GitHub does, a change to how an account is reached needs the person to have proved who they
are in the last two hours: their sign-in (a session's start), or a confirmation since. A stolen
session cookie alone can't add its own passkey or authenticator app, unlink the owner's GitHub,
or make someone an admin.

- What needs it, from a person's own session: adding a passkey (`POST /me/passkeys/options` and
  `POST /me/passkeys`), removing one, setting up an authenticator app (`POST /me/totp`), unlinking
  a sign-in method. From an admin's session: changing a person's `admin` or `disabled`, an invite
  that makes an admin, and adding, changing or removing an SSO connection or its SCIM token. The
  operator's token has no session: it's exempt. Turning TOTP off and new recovery codes already
  take a code. Making a roll token from a session (`POST /v1/access/tokens`, a person's or an
  admin's) needs it too: roll-access asks `roll_auth::Sessions::fresh`, so a stolen cookie can't
  mint a long-lived token (a token a token makes is within that token).
- Without a recent proof: 403 `{code: "reauth_required"}`. `GET /v1/accounts/me/reauth` says
  until when the session is fresh and how its person can confirm (`totp`, `passkey`, `email`),
  and `POST /v1/accounts/me/reauth` takes `{code}` (TOTP or a recovery code), `{challenge, code}`
  (an email code from `POST /me/reauth/email`, sent to their own address; anyone else's code
  proves nothing) or `{id, credential}` (a passkey, from `POST /me/reauth/passkey/options`, only
  their own). It's fresh for another two hours (`sessions.verified_at`).
- The dashboard asks in place: the change's call answers `reauth_required`, a "Confirm it's you"
  dialog (mounted by the app shell) takes the proof, and the call runs once more (`withReauth` in
  `lib/reauth.ts`, used by the accounts client and every product's `call` in `roll-api.ts`).
- Each confirmation, and each refused one, is in the person's security log (`reauth`).

## Routes

Bodies are JSON. Every error is `{"error": string}`, with a `code` where listed. Routes marked
**session** need a full session cookie (or, where it says so, the operator token); **csrf** means
the browser sends `X-Roll-CSRF` (every non-GET with a cookie does). **admin** means the operator
token or an admin's session.

### Shapes

```ts
type User = { id: string; email: string; handle: string; name: string; admin: boolean;
              disabled: boolean; created_at: number; totp: boolean;
              identities: { provider: "github" | "google" | `sso:${string}`; login: string; email: string }[] };
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
| `GET /v1/accounts` | | `{providers: {email: bool, github: bool, google: bool, sso: bool, passkey: bool}}`: which sign-ins are configured |
| `GET /v1/accounts/session` | | `{status: "anonymous"}`, `MfaRequired` or `SignedIn` (always 200) |
| `POST /v1/accounts/invites/peek` | `{token}` | `{email, expires_at}`; 404 for a used, revoked, expired or unknown invite |
| `POST /v1/accounts/email/start` | `{email, invite?}` | 202 `{challenge}`; 400 bad email; 403 `{code: "sso_required"}` for a domain whose SSO connection enforces it; 503 `{code: "email_off"}` when mail isn't configured |
| `POST /v1/accounts/email/verify` | `{challenge, code}` | 200 `SignedIn` or `MfaRequired`, with `Set-Cookie`; 400 `{code: "bad_code"}` (wrong, used, expired or too many tries: one answer); 400 `{code: "invite_invalid"}`; 403 `{code: "disabled"}` |
| `POST /v1/accounts/oauth/{provider}/start` | `{intent?: "signin" \| "link", invite?, return_to?}` | `{url}` and `Set-Cookie` (the browser cookie); 400 `{code: "invite_invalid"}`; 404 provider not configured; `link` needs a session (401) |
| `GET /v1/accounts/oauth/{provider}/callback` | query `code`, `state` (or `error`) | 303 to the dashboard, see above |
| `POST /v1/accounts/sso/start` | `{email, invite?, return_to?}`, or `{intent: "link"}` with a session (csrf) | `{url}` and `Set-Cookie` (the browser cookie); 400 `{code: "no_sso"}` when no connection answers for the email's (verified) domain, `throttled`, `provider` |
| `GET /v1/accounts/sso/{id}/callback` | query `code`, `state` (or `error`) | 303 to the dashboard; `?error=` `unverified_email`, `domain`, `not_invited`, `link_required`, `linked_elsewhere`, `link_email`, `state` or `provider` |
| `POST /v1/accounts/signin/passkey/options` | | `{id, publicKey}` for `navigator.credentials.get`: with a session owing a second factor, that person's passkeys; without, any (throttled per address); 503 `{code: "passkeys_off"}` |
| `POST /v1/accounts/signin/passkey` | `{id, credential}` | 200 `SignedIn`, with `Set-Cookie`; 400 `{code: "passkey"}` (unknown, wrong site, unverified, copied, a used or another sign-in's challenge); 403 `{code: "sso_required"}` |
| `POST /v1/accounts/signin/reset-request` | `{reason}` (a session owing a second factor) | 202 `{id, state, created_at}`: admins are mailed; 401 without a first factor |
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
| `GET /v1/accounts/me/activity` | query `before?` (Unix ms), `limit?` (1–200, 50) | `{events: [{at, action, actor, ok, status, ip, user_agent, detail}]}`: your security log, newest first |
| `DELETE /v1/accounts/me/sessions/{id}` | | 204; 404 if it isn't yours |
| `DELETE /v1/accounts/me/identities/{provider}` | | 204; 404 if not linked |
| `POST /v1/accounts/me/totp` | | `{secret, otpauth_url, qr_png}` (`qr_png` a `data:image/png;base64,…` URL): a new seed, not on until confirmed; 409 if on; 503 `{code: "secret_key"}` without `ROLL_SECRET_KEY` |
| `POST /v1/accounts/me/totp/confirm` | `{code}` | `{recovery_codes: string[10]}`, shown once; 400 `{code: "bad_code"}`; 404 nothing to confirm |
| `DELETE /v1/accounts/me/totp` | `{code}` (TOTP or recovery) | 204; 400 `{code: "bad_code"}` |
| `POST /v1/accounts/me/recovery-codes` | `{code}` (TOTP) | `{recovery_codes}`, replacing the old ones |
| `GET /v1/accounts/me/reauth` | | `{fresh_until, methods: ("totp" \| "passkey" \| "email")[]}` (sudo mode) |
| `POST /v1/accounts/me/reauth` | `{code}`, `{challenge, code}` or `{id, credential}` | `{fresh_until}`; 400 `{code: "bad_code"}` or `{code: "passkey"}` |
| `POST /v1/accounts/me/reauth/email` | | 202 `{challenge}`: a code to the person's own address |
| `POST /v1/accounts/me/reauth/passkey/options` | | `{id, publicKey}`: the person's own passkeys |
| `GET /v1/accounts/me/passkeys` | | `{passkeys: [{id, name, created_at, last_used_at, synced}]}` (never a key) |
| `POST /v1/accounts/me/passkeys/options` | | `{id, publicKey}` for `navigator.credentials.create`; 409 at 20 passkeys |
| `POST /v1/accounts/me/passkeys` | `{id, name, credential}` | 201 `Passkey`; 400 `{code: "passkey"}`; 409 already registered |
| `PATCH /v1/accounts/me/passkeys/{id}` | `{name}` | `Passkey` (1–64 characters) |
| `DELETE /v1/accounts/me/passkeys/{id}` | | 204; 404 if it isn't yours |

Linking: `POST /v1/accounts/oauth/{provider}/start {intent: "link"}` with the session, then the
same callback; on success it redirects to `return_to` (default `/dashboard/account`).

### Admin (operator token or an admin's session)

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/accounts/reset-requests` | | `ResetRequest[]`: `{id, user, email, handle, reason, ip, user_agent, created_at, state, decided_by, decided_at}`, pending first |
| `POST /v1/accounts/reset-requests/{id}/approve` | | `ResetRequest`: TOTP and recovery codes gone (sudo mode; 403 `own_request` for your own; 409 decided) |
| `POST /v1/accounts/reset-requests/{id}/deny` | | `ResetRequest` (sudo mode) |
| `POST /v1/accounts/invites` | `{email, admin?, handle?}` | 201 `Invite & {token, url}` (the token is shown once, and mailed when mail is on); 409 an account has the email |
| `GET /v1/accounts/invites` | | `Invite[]`, newest first |
| `DELETE /v1/accounts/invites/{id}` | | 204 (unused invites only; 409 once used) |
| `GET /v1/accounts/users` | | `User[]`, oldest first |
| `PATCH /v1/accounts/users/{id}` | `{admin?, disabled?}` | `User`; disabling deletes the user's sessions and cuts off their roll code owner (its tokens are revoked and its machines' links closed; turning the account back on gives none of them back) |
| `DELETE /v1/accounts/users/{id}/sessions` | | 204: signs the user out everywhere |
| `GET /v1/accounts/sso` | | `SsoConnection[]` (`{id, name, issuer, client_id, domains, verified, enforce, provision, created_at, redirect_uri, dns: {name, value}, scim}`; never the secret) |
| `POST /v1/accounts/sso` | `{name?, issuer, client_id, client_secret, domains, enforce?, provision?}` | 201 `SsoConnection`; 400 when the issuer's discovery doesn't answer, `public_domain`; 409 a domain has a connection already; 503 `{code: "secret_key"}` without `ROLL_SECRET_KEY` |
| `POST /v1/accounts/sso/{id}/verify` | | `SsoConnection`: its claimed domains whose `_roll-sso.<domain>` TXT record has its value are verified |
| `PATCH /v1/accounts/sso/{id}` | `{name?, client_id?, client_secret?, domains?, enforce?, provision?}` | `SsoConnection` |
| `DELETE /v1/accounts/sso/{id}` | | 204: its people's links go; their accounts stay; its SCIM token stops |
| `POST /v1/accounts/sso/{id}/scim` | | 201 `{token, base_url}`: its SCIM token, shown once (replacing the old one) |
| `DELETE /v1/accounts/sso/{id}/scim` | | 204: SCIM off (the token stops at once) |

A non-admin session on these is 403 `{code: "admin_only"}`; no credentials at all is 401.

## Audit

People's accounts are in roll's audit log (`/dashboard/access/audit`, `GET /v1/access/audit?product=accounts`,
and the SIEM sinks), as Okta's System Log and GitHub's audit log keep them:

- every write to an accounts route, once answered: its method and route (`PATCH
  /v1/accounts/users/{id}`), the id it names, who made it (a person's id, `operator`,
  `anonymous`), its status, the client's address (read as the gate reads it: trusted proxies) and
  user agent: invites, users disabled or made admin, sessions ended, SSO connections and SCIM
  tokens, TOTP, links;
- every sign-in that ended, well or not (`sign_in`): the person (or `anonymous`), the method
  (`email`, `github`, `google`, `sso:<connection>`, `totp`, `recovery_code`) and why it was refused
  (`bad_code`, `sso_required`, `disabled`, `not_invited`…);
- SCIM's changes (`scim.create`, `scim.update` with `active`), as `scim:<connection>`.

Never a body: no code, token, secret or password is in an event. Starting a sign-in isn't
recorded (its end is).

The audit log names people by handle as well as id (`actor_handle`, `resource_handle`; the CSV's
last two columns), disabled and deleted people too, so filtered to accounts it reads as Okta's
and Google Workspace's sign-in reports do.

Each person reads their own (`GET /v1/accounts/me/activity`, "Recent activity" on the account
page, as GitHub's security log): their sign-ins, refused ones too, with the method, address and
device, and every change to their account, whoever made it (them, an admin, SCIM). An event is
theirs when it's about them: its resource is their id (a person's own routes, `/v1/accounts/me…`,
are about them). A wrong email code for their account is theirs too (refused, by `anonymous`: whoever
tried isn't known), so someone guessing at their sign-in shows up there; a sign-up's code, which no
account has yet, is the operator's log only.

## Mail

- `ROLL_ACCOUNTS_MAIL_FROM` (e.g. `roll <noreply@roll.sh>`) turns on mail through SES v2
  (`ses:SendEmail`; the SDK's default credentials and `AWS_REGION`).
- `ROLL_ACCOUNTS_OUTBOX=<dir>` writes each mail to `<dir>/<ms>-<id>.json`
  (`{to, subject, text}`) instead: local development and tests only. It's never on unless set.
- Without either, email sign-in is off (`/email/start` is 503) and invites return their link
  for the operator to pass on.
- `POST /email/start` answers as soon as the address is valid. Everything that depends on whether
  the address has an account (lookups, the throttle, hashing the code, storing it, the mail) runs
  after that, off the request, so the answer's timing doesn't say.

## Settings

| env | |
| --- | --- |
| `ROLL_ACCOUNTS_MAIL_FROM`, `ROLL_ACCOUNTS_OUTBOX` | see "Mail" |
| `ROLL_ACCOUNTS_GITHUB_CLIENT_ID`, `ROLL_ACCOUNTS_GITHUB_CLIENT_SECRET` | a GitHub OAuth app; callback `<api>/v1/accounts/oauth/github/callback` |
| `ROLL_ACCOUNTS_GOOGLE_CLIENT_ID`, `ROLL_ACCOUNTS_GOOGLE_CLIENT_SECRET` | a Google OAuth client (web); callback `<api>/v1/accounts/oauth/google/callback` |
| `ROLL_SECRET_KEY` | seals TOTP seeds and SSO client secrets; TOTP and SSO are off without it |
| `ROLL_ACCOUNTS_SSO_ALLOW_PRIVATE` | `1`: SSO providers on private addresses and plain http (local development and tests only) |
| `ROLL_ACCOUNTS_DOH` | the DNS-over-HTTPS endpoint (`application/dns-json`) SSO domains' TXT records are read from (default Cloudflare's) |
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
- **Search**: done. A cluster's `owner` is the person who made it (none for the operator's and
  admins'; a branch keeps its parent's); `/v1/search` is a `User` route that lists a person's own
  clusters and answers 404 for another's; workspace `Owners` answers for `search` from it
  (docs/search/README.md, "Who reaches it").
- **Workspace**: done in phase 1.
- **Billing** (Home's spend): per user once the above carry owners.
- **Orgs**: users in roll code orgs, and invites into an org.
- **Passkeys**: done (see "Passkeys").
- **Handles**: renaming a handle renames the roll code owner.
- **CLI**: `roll login` through the device flow, giving a session-backed token.
