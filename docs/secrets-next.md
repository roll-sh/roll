# roll secrets: Next

The ranked work list and the log of roll secrets' review rounds. It's internal: the product's documentation is [secrets.md](secrets.md).

## Next

**State, 2026-11-28 07:20.** The forty-fourth review is done. It found two live bugs, both fixed. The user then asked for lighter tests and a merge gate under a minute, which comes first. Landed since 15:45, each with main's CI green and after a check of the merged tree wherever main had moved under it:
- The forty-fourth review's live bugs, fixed in #1238 (19e9b7fd1):
  - `Secrets::use_once` read a `sensitive` key for a product's fixed host (#1225), so someone who could reveal the folder but not read the key could have roll write DNS records with it in every zone it can edit. A one-off read now never returns a sensitive key. `to` is still audited;
  - `roll run` handed the command roll's own credentials (`ROLL_ADMIN_TOKEN`, `ROLL_TOKEN`…). It now drops inherited `ROLL_*` names ending `_TOKEN`, `_TOKEN_FILE`, `_KEY`, `_PASSWORD`, `_SECRET` or `_CREDENTIAL_FILE`, unless the project sets that name itself; `--keep-roll-credentials` passes them on.
- #1239 (4a33eb72b): item 5. Every release asset gets build provenance (`actions/attest-build-provenance`). The release job checks it the way `install.sh` and `install.ps1` do (`gh attestation verify`, `ROLL_VERIFY=auto|attestation|checksum`), and step 0 shows the manual check and the from-source prerequisites.
- #1244 (0f1da4f13): items 3 and 4.
  - A Paddle rotation holds its claim until its write, so the secret ends on the key Paddle keeps.
  - Route 53 claims are keyed by hosted zone, and a claim held over an hour is a problem on the registration.
- #1241 (14f6055f0): items 6 and 7.
  - The machine credential file is refused if another user owns it or can write it. Its directory is made 0700, and it's tied to the server that made it.
  - A bare break-glass login counts only while the pushing token is scan-only now (asked of roll-access, never the column).
- #1247 (68600ac85): item 8. `Grants::limits_keys` has no default, so no wrapper can silently widen a key-scoped grant.
- #1249 (af1754e64): items 9 and 10.
  - The open gate's 421 names `ROLL_DASHBOARD_ORIGIN` and `ROLL_ADMIN_TOKEN`, and `*.localhost` Hosts are answered (never as an Origin, never the preview domain), with an end-to-end test in `rolld/tests/auth.rs`.
  - GitHub's `public` and `repository` events flip a link's `private`.
- #1226 (42e5a8c43): key-scoped secrets rules (`keys: [..]`), reviewed by roll MCP.
- #1256 (5475474ba): from roll search's review of #1211. roll's one Discord bot posted wherever a tenant named a channel, which roll secrets' notifications allowed. Every Discord post or edit through an install now checks that the channel is in the install's server.
- #1242 (08fd495bb): the merge gate's only job over a minute was `sdk` (72 s). It's now `sdk-go` (one `go` command over a `go.work`, cached: 19 s warm, 52 s cold) and `sdk` (32 s). The Terraform provider's tests run in parallel (27 s → 5 s), and the operator's long-poll test no longer waits out a held poll (3.1 s → 0.16 s).

In flight:
- lighter tests: the roll-secrets crate (api.rs's 164 tests, nodes, units), and rolld, the CLI, the dashboard and the SDKs (measured: `pnpm test:secrets` 139.5 s → 106.2 s warm, 330 → 322 Rust tests, 32 → 25 dashboard tests, before the crate's cut);
- #1257: a key-scoped token's watch says at the start why it can't watch (item 8's UX half). Per-key watching is left.

Left:
- the forty-fifth review, after the test cut;
- the `discord.post` step's DM path can still DM anyone who shares a server with roll's bot (roll code's).

**State, 2026-11-27 15:45.** The forty-third review is done. It found two live bugs, both fixed. Landed since 05:00, each with main's CI green and after a check of the merged tree wherever main had moved under it:
- The forty-third product review's live bugs:
  - medium: a rolld with no `ROLL_ADMIN_TOKEN` (the guide's step 0 for an evaluation) answered any `Host`, so a web page could act as the operator through DNS rebinding, or open a shell over a WebSocket. Fixed by #1193 (fedf38742), where an open gate answers only loopback hosts and step 0 gets a token, and #1202 (0d628850d), where an open gate refuses a foreign `Origin` whatever the method, after roll MCP's review;
  - low: `//` passed the 8-hour cap on a whole environment. Fixed by roll MCP in #1186, which also checks folders canonically at the gate.
- #1199 (df9d5d905): item 8. `import --check` exits 2, naming how many keys weren't compared.
- #1225 (97bac47b1): items 3, 7, 10 and 5 (roll secrets' half).
  - Machines' processes share one login.
  - A bare break-glass login binds only to a scan-only hook that named it first.
  - The `Debug` canary is enforced by a source scan (`debug-no-credential.txt`).
  - `use_once` reads a sensitive key only for a product's fixed host.
- #1219 (11fbd4453): items 4 and 6. A certificate's AWS role works with the documented trust condition, and Route 53 names are claimed fleet-wide and read back after `INSYNC`.
- roll MCP's #1201: item 5's lend part. A root lend is approved once only.
- #1203 (a03f8065e): Health flags a public repository whose project has `leaks.rotate` off.
- #1206 (a2be4c27f): SaaS rotation for Prisma Cloud (Palo Alto Networks), and Paddle's self-rotating keys. Netskope and Cisco aren't added, with the reasons in docs/secrets.md.
- #1188 (ac7c9b54b): rolld is released with the CLI (Linux and Apple Silicon tarballs, checksummed), `install.sh --with-rolld`, and step 0 uses it.

In flight:
- key-scoped secrets rules (#1226, `keys: [..]` on a secrets rule), reviewed by roll MCP. After it lands, roll MCP lets a lend that names its key be remembered for that key.

Left:
- the forty-fourth review.

**State, 2026-11-27 05:00.** The forty-second review is done, except item 9, which roll MCP has (docs/permissions.md's lend wording and the approval screen's remembered scope). Landed since 22:40, each with main's CI green and after a check of the merged tree wherever main had moved under it:
- #1167 (fb06af79e): item 8. The guides' step 0 runs one rolld on SQLite for an evaluation, moving-from covers Kubernetes Secrets, SOPS and CI variables, and `roll secrets import --from … --check` compares a source with roll by fingerprint without writing.
- #1172 (d1feb5b5a): item 1's rest and item 5. A break-glass login is bound to the hook token that named it (`alice@token:<id>`), and a token's total is its scan budget, so invented pushers buy nothing.
- #1173 (ef3430284): items 3 and 4. `Secrets::use_once` reads a reference nothing keeps, once, as the caller; roll apps' Cloudflare DNS token uses it, and the gate lends its folder. Machines' logins count per chain, off their maker's token cap.
- #1174 (e734da5c6): items 2, 6 and 7. Route 53 keeps other clients' `_acme-challenge` values, the panel replaces a certificate's DNS credential, and a certificate's AWS session is named `roll-public-cert-…` and limited by an inline session policy.

In flight:
- SaaS rotation for Palo Alto and the other vendors AWS added, and Paddle's self-rotating keys;
- the forty-third review.

Left:
- rolld isn't released as a binary or an image, so an evaluator needs Rust (#1167 found it);
- Health's public-repo hint.

**State, 2026-11-26 22:40.** The forty-first review is done. The forty-second (2026-11-26, cold, read only) found one live bug, which is fixed, and its other items are in flight. Landed since 08:40, each with main's CI green and after a check of the merged tree wherever main had moved under it:
- #1098 (ed973e153): the fortieth's item 1, follow-up. A token's backup carries no sync credentials or encryption key bytes.
- #1101 (5e5b346ef): the thirty-ninth's item 11, part 3. The guide moved to `docs/secrets/` (getting started, moving from another manager, platforms); docs/secrets.md is the reference. A citation check: 443 citations, none broken.
- The forty-first product review found one live local bug, low-medium. On Windows, a local user could create `%ProgramData%\roll\agent` before `--install-service` and plant the agent's token. It never shipped in a release. Landed:
  - #1108 (846eddc79): item 1, the live fix. `trusted_dir` checks owners and access lists, and its Windows test runs on both Windows runners.
  - #1103 (08c12e14b): item 6. No credential in a request's `Debug`, with a canary test, `no_debug_prints_a_credential`, that each type holding one gets a line in.
  - #1117 (84ed24957): items 3 and 4. Scan budgets per token and pusher, and break glass reported as a leak.
  - #1128 (c941bc102): items 5, 8 and 9. Machine fleets count live chains, a restore says machines need new bootstrap tokens, and MCP lists machines.
  - #1142 (24c396d34): items 2 and 7. Guardrails' token lifetime binds every grant roll-access makes: clamped for consent, step-up and remembered approvals; refused for tokens and machine clients; a day for enterprise apps under `require_expiry`. The operator's `GET /v1/secrets/policy/report` lists what's below the policy. Reviewed by roll MCP.
  - #1110 (eeb2236f3): a bind refused for `reveal` says why an approval can't stand in.
- #1144 (a3b382517): public certificates in the CLI (`roll secrets public-certs`) and the dashboard, and Route 53 for their DNS challenge (a project AWS role or an access key).
- The forty-second product review's live bug, low, came from #1117: break glass rotated a managed value for a caller with only `scan`. Fixed by #1154 (2a6771731): it rotates only where the hook's token may write, as a reported leak does.

With other threads:
- roll MCP's #1113 and #1118: a secret another product reads or writes as the caller is approved at the gate, as an exact folder allow in the grant it hands over. Binds stay refused for `ask` grants.
- roll search's #1097 keeps a cluster's admin password in a project secret, through `check_batch` (shared with `batch`).
- roll apps' #1131 writes a customer's Cloudflare DNS records with a `roll://` token, read once as the caller.

In flight (the forty-second's items):
- 1's rest and 5: a break-glass pusher bound to the hook's token, and no budget for invented pushers;
- 2, 6 and 7: Route 53 keeps other clients' challenge values, the panel replaces a DNS credential, and a certificate's AWS session is tagged;
- 3 and 4: a one-off read as the caller for a reference nothing keeps, and machines' logins off their maker's token cap.

In flight (`feature/secrets-saas-palo-alto-paddle`):
- Palo Alto rotation, as Prisma Cloud access keys of a service account (System Admin credential, each new key signed in with before it's kept);
- Paddle's model: a self-rotating vendor whose registration is the key itself. Each rotation is made with it and replaces it there, and Paddle ends the old secret (roll can't, before Paddle's grace ends). `leaks.rotate` rotates it with no grace, so the leaked one ends at once.
- Not built, with why in docs/secrets.md's SaaS section: Netskope (no API to make a token), Cisco Security Cloud Control (the rotating value is a refresh token every consumer spends) and AWS's self-rotating Palo Alto tenant key (no public API documents one).

Left:
- the forty-second's items 8 and 9 (the guides' 15-minute path with no server, and docs/permissions.md's lend wording);
- Health's public-repo hint.

**State, 2026-11-26 08:40.** The thirty-ninth and fortieth reviews are done. Landed since 2026-11-25 15:00, each with main's CI green and after a check of the merged tree wherever main had moved under it:
- #1019 (dd7420791): the thirty-ninth's item 6. `roll secrets scan --pre-receive`, and a match-only `scan` right for scanner tokens.
- #1036 (226bc65f8): item 11, part 1. `roll secrets --help` grouped under six headings, and an address list that can't be read refuses.
- #1037 (04ea6a51e): item 2, a Windows build. `roll run`, scan, render, login and the local agent as a service; install.ps1, winget and Scoop.
- #1006 (6ef229ce5): item 8, guardrails. A policy of floors every project may only tighten.

The fortieth product review (2026-11-25, cold, read only) found one live bug of the bypass kind, medium: a token with `admin` but not `reveal` could take every value, and since #1001 every registration's key, through `POST …/backup` sealed to its own key. Landed:
- #1049 (5ef394d78): item 1, the live fix. A backup and a backup verify need admin and reveal over the whole project.
- #1057 (deded187f): items 4, 6 and 10. Machine credentials are capped per project, a refresh whose answer was lost is retried safely, and an outside database's admin URL replacement moves the maker.
- #1056 (4e005025c): items 3, 5 and 7. Scan matches whole values only, the pre-receive hook fails closed on what it can't read, and break glass is named and allowed (guardrails' `no_breakglass`).
- #1077 (9b37145a4): items 9 and 12. A leak's renewals count toward the public certificate's caps, and the dashboard makes bootstrap and scan-only tokens and lists a machine's chains.
- #1073 (d2ed08d20): item 1's follow-ups, and items 2 and 8. A token's backup carries no registration keys, and its verify answers counts only (rate-limited). Schedules need admin and reveal. Reach, cut-off, offboarding and Health see backups. A restore runs the checks that making each identity and CA runs.
- #1094 (747fb4360): item 11. Pushes to GitHub repos are scanned by roll code's App, and a value found is reported, rotated and fails the commit's check.
- #1080 (fb15cbaf0): from roll search's review of its password store. No value or credential appears in a `Debug` (eight types).

With other threads:
- roll search stores a cluster's rotated admin password straight into a project secret, as the caller, through `caller_of` and `set_secret_confirmed`, with `Secrets::check_batch` (one prelude shared with `batch`) before rotating;
- roll MCP fixed roll-workspace's "everything" fixture after `scan` joined `Action::ALL` (#1038).

In flight:
- a token's backup without sync credentials or encryption key bytes (`fix/secrets-backup-token-syncs-transit`);
- the forty-first review.

Left:
- the thirty-ninth's item 11, part 3: split docs/secrets.md's guide (getting started, moving from another manager, per platform) from the reference;
- Paddle's model (keys that can only rotate themselves).

**State, 2026-11-25 15:00.** The thirty-eighth review is done, and most of the thirty-ninth (2026-11-25, cold, read only) has landed. Landed since 00:10, each with main's CI green, and after a check of the merged tree wherever main had moved under it:
- #959 (62ba416bb): the thirty-eighth's item 8. A mounted `.env`'s readers each get a whole copy, and a read roll doesn't answer says why.
- #971 (53d78cb3e): its item 6, part 2, Salesforce rotation. Paddle isn't added: its API can't create or revoke a key, only rotate the one it's called with.
- #976 (d3032de33): its item 10. Reach lists only the tokens that can name the project.
- #977 (56b349d94): the thirty-ninth's items 1 and 9, its live gap. A leaked public certificate's key is revoked at the CA (`keyCompromise`), and asked-for renewals are capped.
- #985 (611e0b0e4): items 4 and 5. A credential replacement makes the replacer the maker everywhere, and an intermediate must exclude every IP address.
- #1004 (67c8a3757): item 10. The public-certificate and notification sweeps read only what may be due.
- #1001 (eb341356b): item 7, disaster-recovery backups. Archive v2 carries CA keys and registrations, there are `restore --dry-run` and `backup verify`, and a "Disaster recovery" section covers a whole server.
- #1015 (e89350727): item 3. VMs log in without a long-lived token: GCE and Azure through their metadata servers, bare metal through bootstrap tokens and machine credentials.
- #994 (acda9f8a5): two roll-cli tests that broke gates under load (one left an orphaned `sh` loop holding the build lock), and the shared-certificate 409 says a person confirms (roll MCP's #982).
- This change: item 11's move of this list out of docs/secrets.md, which is product documentation.

In flight:
- item 8, guardrails (#1006): a policy of floors every project may only tighten;
- item 6, a pre-receive hook and a match-only scanner right;
- item 2, a Windows build of `roll`;
- item 11, part 1: grouped CLI help, and an address list that can't be read refuses.

Left:
- item 11's split of the guide (getting started, moving from another manager, per platform) from the reference, once fewer changes are editing docs/secrets.md;
- Paddle's model (keys that can only rotate themselves);
- the fortieth review, once these land.

**State, 2026-11-25 00:10.** The thirty-seventh review is done. Landed since 06:10:
- #880 (c6105209a): a value-less set needs an existing key.
- #886 (4ea3a51d9): `roll secrets mount` gives each reader its own pipe, one copy per open.
- #898 (410e8dd74): item 5b, certificate syncs to ACM, Key Vault and Cloudflare.
- #900 (bcd36b606): item 8b, "notify me" by Slack, Discord or email.
- #923 (db5752ae7): item 5c, publicly trusted certificates through ACME (dns-01 via Cloudflare), on roll apps' edge ACME client, now shared.

The thirty-eighth product review (2026-11-24, cold, read only) found one live bug, low-medium and only under `leaks.rotate = managed`: a caller who could only reveal a managed key could rotate it with no grace, repeatedly, by reporting it `leaked: true`. Landed since, each with main's CI green and most after a check of the merged tree:
- #909 (47cea7593): items 1 and 3. A reported leak rotates only where the reporter may write, at most once an hour per key, and a leak request whose key was rotated meanwhile applies as "already rotated".
- #921 (10dfd86c2): items 2 and 9. An intermediate needs a critical `nameConstraints` within its domains (or an explicit, audited `unconstrained: true`), and `DELETE …/syncs/{id}?remove=true` removes a certificate sync's object at its target.
- #925 (ab2940875): items 4 and 7. Replacing a SaaS credential moves its maker, and `leaks.rotate` is settable from the CLI and Terraform's `roll_secrets_project`.
- #945 (925f9aa09): item 5. AWS syncs take a project role, and Google and Azure syncs use workload identity federation through roll's issuer (`sub roll-secrets:<project>:sync:<id>`); Health flags stored cloud keys.
- #956 (f3da93010): item 6, part 1. SonarQube and Jenkins rotation (Jenkins verifies the new token before revoking the old), and an unknown request `state` is a 400.
- #927 (707b1e657): listing requests filters `state` in SQL and pages with a cursor, from roll MCP's gap review, mapped by their #933.

With other threads:
- search's #922 seals cluster passwords always (the thirty-sixth's item 12);
- search's #808 (create sessions held to the person's session in roll-access's evaluator, our search link's owner guard, `rekey_links`) is reviewed and approved here, and in flight there.

In flight:
- item 8, `roll secrets mount` serving concurrent readers each a whole copy, and saying so when a read fails;
- item 10, reach and the deletion preview reading only tokens that can name the project.

Left:
- item 6's other vendors (Salesforce, Paddle, Palo Alto);
- DNS providers for ACME beyond Cloudflare and Route 53.

**State, 2026-11-24 06:10.** The thirty-sixth review is done: #830 (99cb3b05e; Cloudflare, Databricks, Confluent) and #825 (the fresh-project production test) landed.

The thirty-seventh product review (2026-11-23, cold, read only, against efafbcac6) found one live bug, low-medium: an environment's deletion preview named every roll-access token on the server whose rule was "every project, environment `<slug>`". Landed since, each with main's CI green, most after a check of the merged tree:
- #847 (4d73111dd): items 1 and 9. The preview names only tokens that reach the project, judged as reach does (one shared `token_actions`) and narrowed before resolving. A sandbox copying from a deleted, recoverable project keeps its values.
- #836 (a8a5eb3b4): item 2. An agents machine's `roll://` key is sent as refused only on a refusal; anything else retries with backoff.
- #844 (c0af796a8): item 3. The events cursor moves to the head past filtered rows (an index on kinds), and a folder-scoped reader sees a change across folders, trimmed to its folders.
- #881 (3f5aaf25b): item 4. With `leaks.rotate: managed`, a leaked managed key rotates at once without grace, once per credential. A change request opens instead where approvals are needed. With `off`, the event says `rotatable` and roll code offers a Rotate button.
- #872 (c6dcb2ae7): item 5a. A CA can be an intermediate under the customer's own root (CSR, then `PUT …/cas/{id}/chain`), with name constraints narrowed and leaves carrying the chain.
- #857 (528cae7d6): item 6. Vendor-side expiry is tracked (GitLab, Databricks, Entra), `rotate_days` must beat it, and Health warns on a registration's own `credential_expires_at`.
- #850 (63a7ff4a3): item 7. `roll secrets mount .env` is a FIFO served from the CLI, never a file on disk.
- #856 (c4afb6c56): item 8, first half. An Events tab in the dashboard, live, with kind and environment filters sent to the server.
- #876 (9f3c36324): item 10. Doppler's references stay references, and Infisical and Doppler bring whole projects with folders, comments and a dry run.
- #867 (fa9400fe0): item 11. `PATCH …/saas/{id}` replaces a registration's credential in place, plus Terraform's `roll_secrets_saas_rotator` (`credential_wo`), `roll_secrets_folder_policy`, and approvals on environments.

With other threads:
- previews' #865 (8d0ddeda7): `Subject::Preview`, roll's issuer for a trusted preview's own services, reviewed here (preview subjects need an ephemeral environment);
- search's #808 (in flight): creates naming a project are held to the person's session in roll-access's shared evaluator, and our search link refuses a cluster whose owner isn't in its home project;
- roll workspace's events feed #813 (8b2470590) for roll MCP's `workspace.event`.

In flight:
- item 5b, certificate syncs to ACM, Key Vault and Cloudflare;
- a value-less set never creating a secret (roll MCP's finding);
- `roll secrets mount` serving one copy per open, however slow the reader.

Left:
- item 5c, an ACME client for public CAs;
- item 8's second half, "notify me" by Slack or email.

**State, 2026-11-23 16:50.** The thirty-fifth review is done: item 6B (#763, 5583dfbfd; `Reference::parse` moved into `crates/roll-secrets-ref`, so agent-proto, the `roll` binary and the gateway don't link roll-secrets) and 6C (#794, dbb4d65af; a refused upstream key fails closed without trying another key of that provider) landed. Also #778, the 05:20 entry below.

The thirty-sixth product review (2026-11-23, cold, read only, against 5430a1b0d) found no live security bug. Its top items were seams where the night's fixes meet each other:
1. A sandbox stopped on any broken key in its folder, not just the ones it copies.
2. An environment's delete warning didn't name the services, sandboxes, syncs, identities and tokens that read it.
3. The events feed had no value changes.
4. Reach unsealed every roll apps variable to find references.
5. The CLI didn't follow #720.
6. `roll secrets agent` kept serving after a 403.
7. An environment delete revoked credentials before deciding.
8. Offboarding walked every project.
9. Five SaaS rotators against AWS's twenty.

Landed since, each with main's CI green and most after a check of the merged tree:
- #796 (cd59a6cc8): items 1 and 6. A sandbox fails only the keys it copies; a deleted environment in its window is "unsure", so a recovery brings values back. The local agent drops a refused folder at once.
- #793 (1815bac01): item 3. `secret.changed`, `secret.deleted`, `rotation.succeeded` and `access.changed`, each in the change's own audit row.
- #798 (ccd0fcdbd): items 2, 5 and 7. The deletion preview names every reader and any break needs `confirm`. `envs rm` previews and says what happened, and `envs recover` exists. The delete decides in one write, then revokes.
- #816 (efafbcac6): items 4 and 8. roll apps keeps a `reference` column, so reach opens no value. Offboarding reads only the person's rows (`SecretsMemberRechecks`).
- #819 (3dd33c1d5): item 9, part 1. GitLab (create, then revoke), OpenAI project service accounts and Datadog keys.
- #800 (e9d6e59b5) and #803 (d40c853b1): delta reads in the operator, CSI provider and Lambda extension, and in the Python, Java, .NET and Ruby clients. All six SDKs and the Go consumers now read only what changed.
- #813 (8b2470590): roll workspace's project events feed (members, groups, invites) for roll MCP's `workspace.event`. It uses the same cursor rule as secrets' feed.

In flight:
- item 9, part 2 (Cloudflare, Databricks, Confluent);
- a test that a fresh project's developer can't reveal production (the thirty-fifth's last gap).

Left from the thirty-sixth:
- item 10 (time-boxed access for people) and item 13 (the root of trust beyond AWS KMS) are the user's call;
- item 12 (search's clear passwords without `ROLL_SECRET_KEY`) is roll search's, raised with them;
- `Watch` with a consumer still reads `/resolve` whole, because there's no delta route there.

**State, 2026-11-23 05:20.** Landed since 19:25, each with main's CI green (most after a check of the merged tree, since main moved about ten commits an hour and the merge queue's CI runs only `cargo test --lib`):
- #720 (fcafbf9fc): item 2, an environment's delete is kept for the recovery window, and a protected one's is a change request.
- #732 (de8f381a4): the events feed (`GET …/projects/{project}/events`), roll MCP's `secrets.event`.
- #743 (b19e0e201): agent-host's `pi_compaction` passes in the shared gate set (item 10).
- #744 (6010d6093): item 7, linked fields carry `{kind, ref}`. roll apps' branch environments drop only links to what they branch.
- #762 (aafd32f2f): item 8, `roll secrets agent`, the Lambda extension's surface on any machine (127.0.0.1, `X-Roll-Token`, digest-followed cache).
- #772 (296e5064e): item 10, the agents end-to-end run starts rolld on port 0 and reads `--addr-file`.
- #754 (2bb91aff8): delta reads for watchers. After a one-key change at 1,000 watchers on a 9,900-key folder, 216 → 7.7 CPU-s on SQLite and 214 → 15 on Postgres.
- #681 (90dd155df): members v1.1 phase 1, groups and SCIM Groups.

In flight:
- item 6B, an agents endpoint's key as a `roll://` reference (#763). `Reference::parse` moves into `crates/roll-secrets-ref`, so agent-proto doesn't link roll-secrets.
- the thirty-sixth product review (cold, read only).

Next after those:
- **Item 6C** (the model gateway's upstreams as `roll://`, on #763's ref crate).
- **Delta reads for the rest** (S each): the Kubernetes operator, the CSI provider and the Lambda extension don't use `Watch`, so they still read whole folders. `Watch` with a consumer goes through `/resolve`. The server's full-set fallback keeps all of them working.
- **Gate flakes under load** (another product's tests, seen on every secrets gate tonight at loads of 55–93): roll-cluster's Postgres lease and routing tests, code-control's `the_merge_queue_continues_a_dead_owners_batch` and `an_idle_sandbox_sleeps_and_a_command_wakes_it`. Each passes alone. They're roll code's and roll-cluster's tests, so they aren't fixed here.

**State, 2026-11-22 19:25.** Landed since 07:55, each with main's CI green:
- #665 (57deeb0d1): a placeholder swaps only for its own consumer; references never smuggle a bound key's value; fallbacks skip bound keys before decrypting.
- #697 (a74036a64): item 4, an offboarded person's leases, shares and sandboxes end with them.
- #688 (03e1d9520): item 1, a new project's production is protected (`approvals: 0` writes directly; existing 0-approval environments kept their requests by migration), and the default developer rule never matches production unless named.
- #705 (9d5032147): an interception CA stays live until its consumer's hosts change; the per-connection check (`intercept_ca_stands`) is one row read.
- #707 (01199a351): the first digest after a change is shared per permission view on a node. At 1,000 watchers that's 74.6 → 0.4 CPU-s on SQLite and 72.6 → 1.8 on Postgres.
- #714 (054e6edb7): item 3, reach names people, OAuth clients, services, sandboxes and who knew.
- #734 (bfebaa3fc): item 5, agents sandboxes read their copied values fresh as the copier at every start (agent37 edits env in place).
- #733 (055ab1adb): item 6 part A, reviewer provider keys as `roll://` references (`code:review:<owner>` in reach).

Other threads: previews #686 (CA reuse until hosts change), apps #689 (CA state per network).

In flight:
- item 2, environment deletes recoverable and a protected one's delete as a change request (#720);
- item 7, linked fields carry their kind;
- the event feed (`GET …/projects/{project}/events` for roll MCP's Events; the cursor taken in each row's own write);
- members v1.1 phase 1 (#681, review fixes done, merging main);
- agent-host's `pi_compaction` in the shared gate set (item 10).

New since the ranking:
- **Delta reads for watchers** (performance, M; building in `feature/secrets-delta-reads`). After a change each pod still reads all its values: about 190 CPU-s per change at 1,000 pods, bound by the database connections. A watcher holding digest X asks for what changed since X and gets only the changed keys ("Delta reads").
- **Item 6, parts B and C** (agents' endpoints and the model gateway's upstreams as `roll://`): a relay-side maker table, an `agent:<machine>` resource, the value sealed to the daemon after each `hello`, `EndpointSpec::check` taking `roll://`.

**State, 2026-11-22 07:55.** The thirty-fifth review's ranking below still stands. Since it:
- **roll apps' #603 review is closed on apps' side:**
  - #667 (listings and staleness mint nothing, the hash by binding, dynamic keys refused, an opened
    environment ends its interception);
  - #669 (the CA renews and restarts without breaking: the next CA rides in the bundle, sleeping
    environments re-mint, adopted instances roll, new hosts re-mint);
  - #674 (a deleted environment's interception state goes; which runtimes read the bundle).

  roll-intercept's leaf keys are wiped from memory (previews' #671). The response scrub is live
  (previews' #630).
- **Ours from that review:** #665 (a placeholder swaps only for its own consumer, references never
  smuggle a bound value, fallbacks skip bound keys) is gated green and re-levelling a second time,
  after apps' #667 and #669 rewrote rolld's `resolve_closed`.
- **Building:**
  - item 1, production protected by default (`fix/secrets-production-protected`);
  - item 4, offboarding (`fix/secrets-offboarding`);
  - the stampede measurement (`perf/secrets-change-stampede`);
  - members v1.1 phase 1, groups and SCIM Groups (`feature/members-groups`, option (a)).
- **Item 2 is on its PR** (`fix/secrets-env-delete-window`): a standard environment's delete is kept
  for the window and recoverable, a protected one's with approvals is a change request, the 409
  names what breaks, and `Held` counts registries and sinks.
- **Next to start, as build slots and disk allow (the server is at 88%):**
  - item 3, reach naming people, clients, services and sandboxes.

**Ranked, 2026-11-22 06:20** (the thirty-fifth product review, cold, on main 6575a213a, against
Vault/OpenBao, Infisical, Doppler, AWS, 1Password, Akeyless, Bitwarden SM, GitGuardian, Google Secret
Manager and Azure Key Vault). Every item of the thirty-fourth review that roll secrets owns is on main,
and a cold read found the new code sound where it was aimed. The new seams sit next to those fixes.
Newest entries first; older State and Ranked entries follow further down, as history.

1. **A new project's production isn't protected** (security, high, S; fixing in
   `fix/secrets-production-protected`). It's made with `protected: false`, and protection is "has a
   policy", so members' developer default (`unprotected: true`), checks and sandboxes all reach it, and
   developers write it with no request. The fix:
   - a new project's production is protected (with `approvals: 0` writing directly for a solo owner);
   - the default rule never matches `production` unless named, as D3 promised;
   - Health flags an unprotected production that others reach, and the invite sheet says what a
     developer reaches;
   - a fresh-project test.
2. **Deleting an environment erases it at once** (security and data loss, M). Its secrets, versions,
   changes and requests go behind a typed slug (`envs.rs:882`), while deleting one folder there needs a
   change request. #585's `Held` doesn't count registries (GitHub Apps, cloud accounts, outside
   databases) or sinks, so a project holding only those goes on workspace delete alone. The fix:
   - #585's window for environments (`forgotten_at`, recover, purge);
   - a protected environment's delete as a change request;
   - the 409 names the references that will break;
   - `Held` counts registries and sinks.
3. **Reach doesn't name people, OAuth clients, services or sandboxes** (governance, S–M). roll MCP's
   leak-response prompt (#661) builds its plan from reach. It gains `people` (members whose role
   reveals there), `clients` (`rlo_` by person), `services` (apps services resolving the folder, with
   their last deploy), `sandboxes` (live copies) and `knew` (people who revealed it and no longer can).
4. **An offboarded person's leases and one-time shares outlive them** (security, medium-low, S).
   Disabling an account (an admin, or SCIM deactivate or delete) doesn't run `member_changed`, so
   their dynamic credentials live out their TTL (up to 7 days) and their shares still open.
   roll-accounts' disable writes an outbox event, and rolld treats a disabled person as no member
   (fixing in `fix/secrets-offboarding`, with item 5's "a disable stops the copier's running
   sandboxes").
5. **Sandbox copies are frozen at create** (S–M; fixing in `fix/sandbox-fresh-values`). Resolve
   `secrets_from` afresh at each start as the maker (`values_as_maker`). agent37 edits the
   instance's env (`PATCH`, merged; `null` unsets) before its start, on the same disk. A member event
   or a disable stops the copier's running sandboxes (#697).
6. **roll's own AI and chat credentials can't be `roll://` references** (integration, M):
   - reviewer provider keys (code-control's own table): **done**, `bind_ref` for `code:review:<owner>`;
   - agents' endpoints (pasted or `op://`): **done**, `bind_ref` for `agent:<machine>`, sent sealed
     by the relay (`endpoint.key`);
   - the model gateway's upstreams (`env:`/file): **done**, `agent:<machine>`, sent as `gateway.upstream_key`.

   A leaked model key, the commonest leak, gets no reach, health, validity check or rotation.
   Bind with `bind_ref` and read with `resolve_as`. The daemon resolves over its machine link.
   All three are done (#733, #763, and this item's part C).
7. **apps' environments with their own data drop every linked key** (S). `Values.linked` gains
   `{name, kind, ref}`, so apps drops only what it branched.
8. **A local agent outside Lambda** (S–M): the Lambda extension's surface as a sidecar.
9. **Test gaps** for items 1–4, one per flow.
10. **Gate hygiene** (S):
    - `scripts/agents-e2e.mjs` picks rolld ports (use `--addr-file`);
    - agent-host's `pi_compaction` under the shared gate set, plausibly serde_json's `float_roundtrip`
      unified through the search crates (compare the numbers as values).
11. **Search stores cluster passwords in the clear without `ROLL_SECRET_KEY`** (in flight on #601 and
    #318; track).
12. **The root of trust beyond AWS KMS, and FIPS** (L; the user's call).

In flight from before: #665 (a placeholder swaps only for its own consumer; references never smuggle a
bound value; the fallback skips bound keys) is re-levelling on previews' #630, which made the response
scrub live. Also `perf/secrets-change-stampede` (measuring) and `feature/members-groups` (members v1.1
phase 1). Elevation waits on the user's D13, D14 and D16.

Done in #20 (merged 2026-11-15 as 1ad248420):
- the store and its crypto (the deployment key from the environment, a file, or AWS KMS);
- rolld's mount and link providers (Postgres, Redis, streams, buckets, NoSQL, search: checked
  live against a real roll search cluster), and
  rotators for values outside roll;
- `roll secrets`, `roll secrets setup` and `roll run [--watch]`;
- checks, sandboxes and previews, resolving untrusted; preview environments untrusted themselves;
- leaked secrets don't land, plus scans of a repo's default branch and history, and `roll secrets
  scan [--staged]` before a commit;
- credentials roll doesn't hold yet, by their shape: a notice on a change (a blocker with
  `roll.toml`'s `[secrets] unstored_credentials = "block"`), `.roll/secrets-allow`, and `roll secrets
  scan` listing them, refusing them `--staged`, and `--store` moving one in; the change page's
  "Store in roll" storing one from the change itself into the project the repo is trusted for;
- required and typed keys; insights, a daily notice of each environment's health, and expiry dates for values that expire
  outside roll; weak credentials; values as they were after any change; a protected environment's values that are also set in an unprotected one; read
  alerts (a machine's new address, many reveals in an hour);
- one-time sharing, and sharing across projects;
- syncs: GitHub, GitLab CI, CircleCI, Bitbucket Pipelines, Terraform Cloud, Vercel, Netlify, Render, Fly.io, Railway,
  Heroku, DigitalOcean App Platform, Cloudflare Workers, AWS
  Parameter Store and Secrets Manager, Google Secret Manager, Azure Key Vault, Kubernetes, sealed webhooks and deploy notifications;
- audit sinks, purge and retention, workload identities (OIDC logins for CI), tokens and
  identities pinned to client addresses; encrypted project backup and restore;
- the dashboard's Secrets surface, with the deployment key's source and a rotation's progress,
  suggested links for the canvas's resources, and one key set in several environments from the
  overview (one change each, a request where protected), and each environment's activity (its
  changes, reads, requests and syncs' pushes as one timeline, by key);
- finding a key across every project the caller may read (`roll secrets find`, the dashboard's
  "Find a key"), for rotating an organisation-wide key;
- exports as a Docker `--env-file` and a Kubernetes Secret manifest, next to dotenv, shell, JSON and
  YAML; `roll secrets render [--watch --exec]` fills `${KEY}` in a config file and keeps it current;
  `roll secrets check` compares an environment with a `.env.example`;
- the fixes from three security reviews and a phone-width pass;
- every route checked to be in the HTTP table (`every_route_is_in_the_http_contract`), which
  roll-access's catalogue is checked against.

Then change policies per folder (#53), as Infisical's approval policies on secret paths.

A ninth independent review (2026-11-14, of `roll run`'s masking and the eighth review's fixes)
found no high-severity issue; it found a held tail flushed on a timer (a block-buffered program
pausing mid-value leaked it in halves), masking on by default for data written to files and pipes,
background processes holding roll open, `--watch` dropping SIGTERM, a SIGINT judged by stdin (#54),
a folder's admin counting toward a protected environment's own gate and an import guard outside its
transaction (#53), a Vault refusal told by an error's text (#52), and a write during a watch's
re-read going unnoticed (#49): all fixed and checked live, tested where a test guards behaviour.
Since then a values answer carries the folder's digest, taken just before its read, and `--watch`
compares from those answers, which closes the last window (#49).
Every stacked pull request's API tests (#49, #50, #51, #53, #55, #57) also pass on Postgres, and
#49 adds **Copy reference** to a key's menu (`roll://ENV[/folder]/KEY`, as 1Password's). Every
stacked pull request is mergeable, and #20 is level with `main`.
Then roll secrets for VS Code (#58): hovers and warnings where code reads a variable, names
completed, and `.env` values moved into roll as `roll://` references (checked in code-server).
Then `roll completions` and direnv documented (#59).
Docker builds (BuildKit secrets) and Compose are documented and checked. Then a server's
credential under a name frameworks put in the browser bundle (`NEXT_PUBLIC_`, `VITE_`…) is
flagged on write and in health (#60). Measured again (9,900 keys, alternating with #20): health
is unchanged by #60 and stays under the grid; a values answer takes its digest only when a watch
asks (`digest=1`, #49), since that costs about a read.
Then reach (#61): for a folder or key, the tokens and identities that reach it, whether untrusted
code can, where it flows (syncs, shares, imports), and who read it in 30 days.

A tenth independent review (2026-11-14, of ESO, renewing logins, pinned keys and the ninth review's
fixes) found no high-severity issue; it found ESO writing an unresolved key as an empty value and
taking a query through its key (#55: `strict=1`, `urlquery`, checked live on k3s), stdout unmasked in
Jenkins, Buildkite, pods and units and a JSON value's secret fields (#54), a login with no timeout
that dropped a working token and ignored 401s (#56), and no way to rotate pinned keys in place
(#57): all fixed. roll MCP has `secrets_reach` and `secrets_folder_policies` tools for #61 and #53.

An eleventh independent review (2026-11-14, of VS Code, completions, browser-bundle names, reach and
the tenth review's fixes) found two high-severity issues in #58, both fixed: "Move to roll" misread
multi-line and commented `.env` entries (it now reads them as the CLI does and replaces the whole
entry), and a repository's settings could send the token elsewhere (`roll.apiUrl` and
`roll.project` come from your own settings only). It also found masking that changed data (#54:
stdout is masked only in CI and never into a file; `ROLL_MASK=1` for pods and units), re-keyed
identities outliving their re-keyer (#57), readers missed through imports and previews (#61), a
kind of credential shown to read-only callers and references not judged (#60), a stalled renewal
(#56) and a refused strict read keeping its leases (#55): all fixed.

A twelfth independent review (2026-11-14, of the eleventh review's fixes) found no high-severity
issue; it found a login racing a key replacement could still get a token (#57: the login's write
checks the keys it verified), Jenkins' file log unmasked (#54: documented, `ROLL_MASK=1`, since a
file can't be told from a dump), late logins and late 401s overwriting fresh tokens (#56), reach
looking one step away and at unshared importers (#61), a write's warning naming a credential's
kind without reveal (#60), and `.env` corner cases and a repository choosing the environment (#58):
all fixed.

A thirteenth review (2026-11-14, narrow: the twelfth review's fixes) found reach naming another
project's internal environments and walking past where reads go (#61: one hop into a shared
project, named only as a project, capped at 256 folders), reference warnings shown without reveal
(#20), `.env` files over roll's limits (#58) and a background login outliving a 401 (#56): all
fixed. The pinned-keys, relogin and masking fixes it checked were correct.
Reach measured at its cap (300 importing folders): 5–7 ms on SQLite and 120–150 ms on Postgres, once
the audit is read per project rather than per folder (#61).

Then (2026-11-14, latest): `roll run` masks values in what the command prints when it isn't a
terminal (#54), as 1Password's `op run` and GitHub Actions do; pulling into Kubernetes with External
Secrets Operator (#55: its webhook provider, checked against ESO 2.15 on k3s) and
`values?dynamic=false` for readers that can't renew a lease. #50, #51 and #53 merge cleanly
together, and the merged tree passes roll-secrets', rolld's auth and the dashboard's tests. #52's
import dialog names Vault trees and AWS (screenshots on #52). `roll secrets kubernetes` and the
Export menu print ESO's manifests with a fresh token (#55), and a workload identity's login renews
itself (#56), so a pod running `roll run --watch` needs no stored token, as with Vault's Kubernetes
auth; and identities pin their issuer's keys (#57), so clusters roll can't reach (k3s, kind,
private EKS) log pods in too (checked on k3s, with #56: a 5-minute login renewed by itself).
`roll run`'s masking was measured and made 5x faster (#54: 330 MB/s with 300 values and a 3 KB
key).

An eighth independent review (2026-11-14, of moving in, change policies per folder and the
seventh review's fixes) found no high-severity issue; it found folder gates a reviewer, a
promotion, an import or a folder delete could get around, apply checking the gates outside its
transaction, backups without folder policies (#53), Vault walks that took any error for an empty
folder and had no bound, colliding AWS keys (#52), and a `--watch` that restarted on every poll
after its references changed (#49): all fixed, the Vault and AWS ones checked live. Screenshots
of folder policies and request gates are on #53.

A seventh independent review (2026-11-14, of RabbitMQ, the OpenSearch names and the sixth
review's fixes) found no high-severity issue; it found a RabbitMQ template copied in part (an
exchange left open), `render --watch` rendering twice, throttled OCSP answers marked cacheable,
repeated flavor lookups, and the short public-suffix list: fixed, or documented for the last.
Moving in from Vault trees and AWS Secrets Manager is #52 (checked against Vault 1.20 and
LocalStack).

Then RabbitMQ outside roll (#39, merged up into #41 and #42; checked live against RabbitMQ 4) and
OpenSearch clusters' fields named as their clients read them (#51).

A sixth independent review (2026-11-14, of `roll://` references, check-and-set, OCSP, OpenSearch
links and the grant fix) found no high-severity issue; it found an OCSP route an anonymous caller
could make sign and cache per request, references whose leases were renewed at the wrong project,
check-and-set dropped on protected environments, and smaller ones (a provider asked before the
project check, a project name two owners share, render --watch, env-file errors, restore names,
public suffixes): all fixed, OCSP re-checked live. The project's tokens now read rules naming it by
id and show key-scoped ones (#30).

Later: the grid sends check-and-set too (#50), OCSP for project CAs (#44: checked with `openssl
ocsp`), search links on OpenSearch clusters (#51, agreed with the search thread), and a fix from roll
MCP's review on #30 (a grant naming the project by id reaches it inside roll secrets too).

Then (2026-11-14, latest): `roll://` references in files (#49: `roll run --env-file`, `render`) and
check-and-set writes (#50: `if_version`, the dashboard's editor), each its own pull request on #20.

A fifth independent review (2026-11-14, of host and user SSH certificates, ClickHouse,
certificates ending, `--rotate-due` and the fourth review's fixes) found no high-severity issue; it
found a CLI that would upload a private key given its path, SSH CAs that could claim a top-level
domain, best-effort signing audits, a stale-CRL race, ClickHouse's kill-then-drop order, a
known_hosts order and `--rotate-due --json` that didn't rotate: all fixed. Screenshots of the new
surfaces are on #44, #45 and #48.

Later still: SSH user certificates for keys people keep (#45: `…/sign-user`, `roll ssh --key`,
checked with an agent-held key against a real `sshd`) and `roll secrets rotations --rotate-due`
(#48).

Then (2026-11-14, latest): certificates ending as health findings (#44: `certificate_expiring`)
and ClickHouse outside roll (#39, merged up into #41 and #42: its HTTPS interface, `sha256_hash`
passwords, checked live against ClickHouse 25.8).

A fourth independent review (2026-11-14, of the rotations overview, transit signing, HMAC and
brought keys, CSR signing, revocation and the CRL, SQL Server) found no high-severity issue; it
found an ended SQL Server lease whose login could stay usable, generated passwords SQL Server's
policy refuses, deleted CAs undoing their revocations, a public CRL signed per request, P-384 CSRs
mislabelled, unrevocable serials, a batch failed by one retired version and a restore that forgot
brought keys: all fixed, tested where a test guards behaviour, and checked live (SQL Server 2022,
OpenSSL). Then SSH host certificates (#45, checked against a real `sshd`).

Built on 2026-11-14 (later), again without a merge: revoking certificates (#44: a list of what a
CA signed, `…/revoke`, revocation when a lease ends, a public CRL every certificate names, checked
with `openssl verify -crl_check`); SQL Server outside roll (#39, merged up into #41 and #42;
checked live against SQL Server 2022); bringing an encryption key (#47: Vault's `vault:v<n>:`
ciphertexts decrypt and rewrap, checked against Vault 1.20).

Built on 2026-11-14 from the last ranking, without waiting on a merge: a rotations overview (#48:
`GET …/rotations`, Health's "Managed credentials", `roll secrets rotations`); signing and HMAC
with encryption keys (#47: Ed25519 a stock library checks with the exported public key, HMAC
blind indexes, `transit(name)` in `@roll/secrets`); CSR signing (#44: `…/cas/{id}/sign`, checked
with OpenSSL's P-256 and RSA CSRs under `openssl verify -x509_strict`).

A third independent review (of #46 and #47) found four ways a sensitive value still reached a person
(older versions, change requests that add the tag, syncs and sandboxes counted as runtimes) and
guessing oracles, plus backups leaving out encryption keys: all fixed and tested. roll apps (#26)
now seals sensitive variables and redeploys when an environment's digest moves (#31 implements
it). An independent security review of #43–#45 found no cross-tenant path; it found managed
certificates that could lapse, an issuer key that a deployment-key rotation would have broken, a
fail-open decoy check and leases outliving cloud tokens, all fixed and tested. Measured again on 2026-11-14 (load about 20): values at 9,900 keys took 89–122 ms on SQLite and
155 ms on Postgres, within the noise of the Performance table; nothing to fix. Reviewed again on
2026-11-14: an independent security review of #39–#41 found racing rotations
could reset the login in use (now a claim never takes a busy role), a `#` in a password shown as
the host, Postgres connections outliving their timeout, and deleted projects leaving credentials
unrevoked (now revoked first, the rest retried by the sweeper); all fixed and tested. Earlier,
2026-11-13 (later): the Link dialog crashed on a kind that isn't a canvas
resource (a rotator, an AWS role): `KIND_ICON` only knows the workspace's. It names roll secrets'
own kinds now and falls back to a plain link icon for any it doesn't know. Earlier, 2026-11-13 (late): #39's own review found two ways a rotation could reset the
role in use (a refused `ALTER ROLE`, two environments sharing the pair); both fixed and tested.
The other threads' heads need nothing from roll secrets (MCP lists outside databases; previews
waits on #31). Earlier, on 2026-11-13 (night): the core and the stacked pull requests are level with
Doppler, Infisical, Vault and 1Password (clients, IaC, CI, Kubernetes, BYOK). The gap that's left
is Vault's signature: dynamic credentials for clouds and databases roll doesn't run. CI stays
under a minute (the new `sdk` and `terraform` jobs take 39 s and 22 s).

Ranked (twenty-ninth review, 2026-11-17 morning, against Vault, Infisical, Doppler, AWS Secrets
Manager, 1Password, Akeyless and step-ca, and the other threads' branches): first what can be built
now, then what waits on a merge or another thread. Since the twenty-eighth: #68 landed; the preview
hole is closed on both sides (#151 with previews' #160); the gateway (#166) covers every outside
engine, a second node, identity logins and revocation at once, and opens a stream in ~1 ms; MongoDB
Atlas (#173), outside search and CQL engines (#165), TOTP keys (#194) and guard scans (#169) are in
PRs; a screenshot pass fixed four dashboard bugs; the read path measured unchanged by #151. The
bottleneck is no longer building: about thirty green pull requests wait on a lock five threads share,
so landing them in batches comes first. Reading roll apps' #191 found that values from roll secrets
not tagged `sensitive` still reach an app's log; preview service logs aren't masked at all (both
asked of their threads).

On main: #20, #62, #63 (the CSI provider), #65 (conditional reads and client caches), #85
(roll-access's tokens), #74 (live links' digests), #66 (Terraform without values in state),
#67 (transit in the Go and Python clients, 2026-11-17 as 0fa832ddf), #151 (previews follow
their branches, 2026-11-17 as 48868a1e2; with roll previews' #160, fdffd596f, a preview's every
followed key, renamed or not, gets its branch's value or a blank: the hole the twenty-eighth review
found is closed, checked live), #68 (AWS through roll's issuer; roles and users edited in
place, 2026-11-17 as 3f829ca42) and #198 (plain link fields, 2026-11-17 as bc805ae41: roll apps
hides every value from roll secrets in its logs unless it's plain connection detail).
On main since 2026-11-18: #69–#73, #75–#77 and #147 (batch 1); #132, #136, #144, #165, #166, #173 and
roll redis's #375 (batch 3, 2026-11-19); #79, #81–#83, #87, #88, #92, #95,
#97, #102, #105, #110, #113, #117, #121, #124, #128, #145, #169, #194, #212, #213, #227, #229 (batch 2).

**State, 2026-11-19 18:40.** Batch 4 is on main (ba8cdc422, ci green): #248, #262, #264, #274, #276,
#279, #284, #329, #365, #370 and #392 in round 1; #303 and #367; #281; #290; then #304, #86 and #394, in
one lock turn. Main's tree after it was the combined gate's tree minus #330, exactly. The gate's three
failures came from outside the batch, and each was checked:
- redis-server's compat had a flake: green on rerun, and green on main;
- roll-cluster's Postgres suite had timing failures under load: 3/3 green alone on the tree and on main;
- `mq-amqp`'s client matrix is blocked by the server's firewall (Docker to host), like four `test:pg`
  restore tests.

Batch 5 (each gated on its own, none enqueued):
- #277: secret requests, with an `rlq_` answer token apart from the id, roll MCP's finding, at 7da58b03c;
- #357: the issuer, with no token for an untrusted preview and an emergency `rotate --now --retire`,
  at b58f4d2f8; #281 is now on main;
- #407: references resolved as their maker (`bind_ref`, `bind_ref_runtime`, `resolve_as`,
  `digest_as`), at 4ea68773a;
- #330: Store in roll (its own gate was still running at the land);
- #397: roll.toml's `[secrets]`, which the person approves;
- #430: least-privilege token defaults, the first hour, and the Syncs tab split in three;
- #432: Postgres decoy roles (on #392, now main);
- the agents-sandbox reveal check (#440).

The other threads' work that builds on these:
- search's #361 builds on #407;
- previews' #436 (`roll://` in env, approved);
- apps' git-login and `roll://` variables (on #329, then #407);
- roll MCP's value-request tools (on #277) and `secrets_verify_audit` (on #281).

roll MCP adds no cut-off tool: #304 is a person's call, since it can revoke the agent's own token.

**State, 2026-11-21 03:10.** #455 is on main (f9bb26c17, CI green): roll code's keys out of `Debug`,
and webhooks' and review apps' signing secrets sealed. Still open:
- #501 members v1: its last gate merged a main from before batch 6, so it's merging main again and
  fitting members' scoped `maker_reaches` to #520's placeholders and #513's decoys (only members who
  manage a project see them). Then #445 and #501 land as batch 7, ahead of previews' #524;
- two follow-ups from batch 6 (`fix/secrets-ca-shrink-decoy-mail`): a consumer's interception CA ends
  as soon as its bound hosts shrink (a placeholder revoked, hosts cleared, a cut-off), not only with
  the consumer, and a decoy planted from an admin's session mails that person, not the operator;
- the thirty-fourth product review (ranked below): items 1 and 3 are being fixed together in
  `fix/secrets-reveal-as-asker`, and item 2 in `fix/secrets-project-delete-window`;
- the #432 decoy-oracle fix: computes pinned by digest, and each image's pre-auth answers learned from a
  compute of that image (about 2 s per image, never waking a suspended endpoint). Its full gate is
  running.

**Ranked, 2026-11-22 05:45** (after the thirty-fourth review's items landed; built from a review of
roll apps' #603 and what gating found). The thirty-fifth cold review is running and will replace this
list. In order:

1. **A placeholder swaps only for its own consumer** (security, high; fixing in
   `fix/secrets-placeholder-consumer`). `Secrets::substitute` checked a placeholder's maker, binding,
   host and liveness, but not who was asking, so a leaked `rlph_` could be swapped through another
   tenant's proxy (apps' closed environments, fork previews). The proxy passes its interception CA's
   consumer and a mismatch is refused with an alert (`WrongConsumer`).
2. **A reference never smuggles a bound key's value** (security, medium; same PR). In a placeholders
   read, `AUTH="Bearer ${STRIPE_KEY}"` resolved the real value. Such a key is withheld with a problem
   naming the bound key.
3. **The maker-refusal fallback reads bound keys** (security, low; same PR). rolld's apps fallback
   decrypted and audited the host-bound keys it then withheld, and a binding added in between leaked
   once. A read mode that leaves bound keys out before decrypting.
4. **The stampede after a change** (performance, M; measuring in `perf/secrets-change-stampede`). The
   first digest after a write per polling identity times 1,000 pods; a fix only if it spikes.
5. **Members v1.1** (completeness, L). The design is `docs/members-v1-1.md` (#662).
   - Phase 1: groups and SCIM `Groups`, building on option (a) in `feature/members-groups`.
   - Phase 2: time-boxed elevation, waiting on the user's D13, D14 and D16.
6. **Sandbox copies are frozen at create** (UX/security, M). A rotated value doesn't reach an agents
   sandbox until it's recreated, and agent37 can't drop keys, so it refuses to start once its copier
   loses access. Deliver env over the relay link at each start.
7. **A `kind` on Values' linked fields** (integration, S). roll apps' branch environments drop every
   linked variable, failing closed, because a linked field doesn't say which resource it came from.
   With a `kind`, they'd drop only Postgres and Redis.
8. **Test-infrastructure leftovers** (S):
   - `scripts/agents-e2e.mjs` still picks a port and releases it before rolld binds (roll MCP owns
     `test-mcp-sweep.sh`'s);
   - agent-host's `pi_compaction` fails in the shared gate package set, because search's crates turn
     on serde_json's `float_roundtrip` through rolld.
9. **A local agent outside Lambda** (UX, S–M): the Lambda extension's surface as a sidecar.
10. **The root of trust beyond AWS KMS, and FIPS** (L; the user's call).

Other threads, from the #603 review: roll apps' renewal and restart fixes (H1–H4, M1) in
`feature/roll-apps-closed-renewal`; search's sealing without a key (item 5) in
`fix/search-security-1120`.

**State, 2026-11-22 05:05.** Every item of the thirty-fourth review that roll secrets owns is on main, with batch 7 (members) and its follow-ups:
- **Items 1 and 3: landed**, #621 (658cecf0e). apps' reveal and shell read roll secrets as the asker
  (any `from_secrets` key that doesn't resolve for them is a 403, no partial answer; a bound `roll://`
  needs reveal on its own folder), and an agents sandbox re-checks its copier's reach on each start,
  starting without the copied values when refused.
- **Item 2: landed**, #585 (a2e1d8b2f), after an independent review (1 high, 5 medium, all fixed). A
  workspace project's delete asks roll secrets first (403 unless a secrets admin; 409
  `project_holds_secrets` until `?confirm=`). Its secrets are kept for a window (7 days by default,
  `ROLL_SECRETS_DELETE_WINDOW_DAYS`), unreadable and recoverable (`POST …/recover`, `roll secrets
  recover`, the dashboard's "Recently deleted"), then purged after a signed closing digest. ACME,
  interception CAs, gateways and workload grants all stop at the delete, and members and homes come
  back on recovery.
- **Item 4: landed**, #628 (8a02a23b8). A binding names its placement (a header template or a query
  parameter; untrusted bindings must), the filter swaps only there, and `proxy::Scrub` looks for the
  swapped values (plain, base64, percent-encoded, across chunks) in the response. Responses are scrubbed
  once previews' #630 wires `Scrub` into `roll-intercept`; until then rolld's adapter drops it.
- **Item 5:**
  - previews: done, #581 (4c575f044). The rules' pasted values, a preview's own variables and the
    screenshot links' key are sealed. Without a key a pasted value is refused, and the signing key
    lives in memory. An upgraded database keeps nothing in its file or WAL;
  - search: `fix/search-security-1120` and #318;
  - roll code: done, #627 (4af6bb3b5). Webhooks' and review apps' URLs are sealed, nothing is made
    without `ROLL_SECRET_KEY`, and an upgraded database keeps no clear URL or secret.
- **Item 6: phase 1 built** (groups and SCIM `Groups`, D10–D12 and D18 as option (a)); elevation is
  still designed only, in [members-v1-1.md](members-v1-1.md). Groups hold
  viewer, developer or admin roles on projects (never owner), and roles add up, read in one query
  with the session grant. SCIM `Groups` is opt-in per connection. A member's time-boxed elevation is
  a roll-access `ELEVATE` approval with the two-person rule. It lapses at `until` with nothing to
  revoke, and it never counts for deciding for others (reviews, accepts, approvers, members,
  tokens, decoys). Its open decisions are D10–D18.
- **Batch 6's follow-ups: landed**, #582 (32dd78392). A consumer's CAs end as soon as it loses a host;
  `maker_unreachable` (403) when a placeholder's or a chosen reference's maker no longer reaches it;
  a decoy planted from a session mails that person.
- **The #432 decoy oracle:** fixed, #575 (8e16604f2). Compute images are pinned by digest, and each
  image's pre-auth answers are learned from a compute of that image, ahead of time at start, so no
  decoy login waits and none answers with bytes the compute wouldn't.
- **Members (#501): landed** as 991d98225 (batch 7, 2026-11-21 22:06), carrying #445's design docs.
  Projects now have members with four roles, session grants and invites, and what a member made runs
  at their current rights, scoped to what it does.
- **Item 8** (placeholders had no consumer on main): done. Previews' #535 (a06876bd1) swaps them in
  `crates/roll-intercept`, shared with apps, and is checked end to end against a TLS echo host
  (`scripts/preview-intercept.sh`). The echo step flips to expect a 502 once item 4's scrub lands.
- **Found while gating: fixed**, #596 (518aa3188). rolld's test binaries raced each other for ports
  (`free_port` only avoided repeats within one process; four gates hit it on 2026-11-21). rolld now
  binds port 0 and writes its real addresses to `--addr-file`, and its tests read that file.

**Ranked, 2026-11-21 03:40** (the thirty-fourth product review, cold, on main f9bb26c17, against
Vault/OpenBao, Infisical, Doppler, AWS, 1Password, Akeyless, Bitwarden SM, GitGuardian, Google Secret
Manager and Azure Key Vault). The store is still level with or ahead of the category, and a cold read
of the newest seams (placeholders, the interception CA, `bind_ref`/`resolve_as`, validity checks)
found them sound. What's left is at the edges:

1. **roll apps' reveal and shell show other environments' values** (security, high, S–M; fixing in
   `fix/secrets-reveal-as-asker`):
   - `…/variables/values`, `…/shell` and the other `check_secret_reveal` routes ask only `Reveal` at
     `(project, env, "/")` (`rolld/src/secrets.rs` `ForApps::may_reveal`);
   - what they show is the service's runtime read, which follows bases, imports and
     `${production.X}`, so a person who may reveal only staging sees production through apps;
   - members (#501) widen it: they hold reveal on single environments;
   - the fix: answer as the asker (any `from_secrets` key that doesn't resolve for them is a 403), and
     a bound `roll://` variable needs reveal on its own folder.
2. **Deleting a workspace project erases its secrets, history, encryption keys and audit at once**
   (security and data loss, M; fixing in `fix/secrets-project-delete-window`):
   - workspace `delete` is enough: `on_delete` → `forget_project` deletes everything in one write;
   - no protected-environment confirm, no transit-key name, no recovery window, nothing audited;
   - the dashboard says "only the project and its canvas go";
   - AWS waits 7–30 days and Azure keeps deletes 7–90 days;
   - the fix:
     - a veto hook (`Workspace::before_delete`): refuse while a protected environment or transit
       key exists, unless a secrets admin confirms;
     - a 7-day recovery window, after which a signed closing digest goes to the sinks before the
       purge;
     - the delete audited, and the dialog saying what goes.
3. **The agents sandbox keeps copied values after their copier loses access** (security, S; fixing
   in `fix/secrets-reveal-as-asker`): #440 checks reveal at copy time, then seals the values and hands
   them out on every start. The fix: store `{project, environment, by}` and resolve as the maker on
   each start.
4. **A placeholder may ride in any header, and responses aren't scrubbed** (security, S; the
   mechanism is confirmed, an echoing host isn't; fixing in `fix/secrets-placeholder-placement-scrub`):
   - a placement is required (the dashboard and CLI default to `Authorization: Bearer {}`), and
     untrusted bindings need one;
   - the proxy contract adds a bounded scrub of the swapped value (plain, base64, URL-encoded), as
     `roll run`'s masking does.
5. **Credentials other products keep in the clear, or in the clear when unsealed** (with their
   threads):
   - previews' rule variables in `settings.json` and the screenshot-link key;
   - roll code's webhook URLs, and signing secrets falling back to the clear without
     `ROLL_SECRET_KEY`;
   - search's remote, snapshot and connector credentials on a node without a sealer;
   - each is sealed, and refused rather than stored in the clear.
6. **People: groups, SCIM groups and time-boxed elevation** (completeness, L; members v1.1, #445's
   "later"): IdP groups as role holders, SCIM `Groups`, and a person's time-boxed role request through
   roll-access's approvals with the two-person rule.
7. **The stampede after a change** (performance, M; `perf/secrets-change-stampede`): measured, it
   spiked. For 1,000 identities on one folder of 9,900 keys, the first digest after a one-key write
   cost 75 CPU-seconds on SQLite and 73 on Postgres (rolld plus the database), about 73–75 ms per
   identity at any count. Each identity's read of its values then cost another 213–218 ms. Now a
   node computes the first digest once for every reader with the same answers: 0.4 and 1.8
   CPU-seconds, 0.1 and 0.6 s wall ("Performance"). The reads are what's left (~190 CPU-seconds per
   change at 1,000 pods), bound by secrets' 4 connections, and they are per reader by design.
8. **Placeholders have no consumer on main yet** (integration): previews' #535 is first; then apps'
   egress and the agents sandbox's proxy. Add one end-to-end swap test through the first real proxy,
   with item 4's scrub.
9. **A local agent outside Lambda** (UX, S–M): `roll secrets agent`, the Lambda extension's surface as
   a sidecar (a self-renewing workload login, an SSRF header, conditional reads), as AWS's Workload
   Credentials Provider does.
10. **Leaks outside your own repos, and credentials in other vaults** (completeness, L, low): public
    monitoring and a metadata-only inventory of other clouds' secrets, as GitGuardian does.
11. **Test gaps:** items 1, 2, 3 and 8 each lack the one test that guards them.
12. **The root of trust beyond AWS KMS, and FIPS** (L; the user's call, R4/R13).

Checked and sound: apps' git login, `roll://` variables, sealed registry logins, drains and tokens;
search's #361, #134 and #485; roll MCP's #572 and #573; #455's sealed signing secrets; storage's
replication `roll://` (while setting a target stays operator-only); placeholders' issue and swap
re-checking the reader, the maker, the binding, tags and references on every use.

**State, 2026-11-21 02:52.** Batch 6 is on main (802935600, CI green, exactly the gated tree), in three
rounds:
1. #499 claims, #511 batched writes, #487 validity checks, #513 decoys session-only, #467 decoy dashboard,
   #494 first hour;
2. #517 interception CA, #493 decoy where;
3. #520 placeholders, wired to #517.

Still open:
- #501 members v1: its final gate on the main-merged tree;
- #455: cleared, lands in a gap;
- the #432 decoy-oracle fix: building.

**Found 2026-11-21 02:20: a decoy oracle on main (#432).**
- **Cause:** computes run the unpinned `postgres:{v}-alpine`, which moved past the 18.6 error text #432
  ported. A real compute now says `L816` where a decoy says `L791`, so a malformed startup packet tells a
  decoy role from a real login.
- **Fix, building (`fix/pg-decoys-version-oracle`):**
  - pin compute images by digest;
  - take the decoy's pre-auth bytes from the compute's real version.

**State, 2026-11-20 23:00.**
- **Batch 6:** per-PR gates green so far: #487, #499 (its Terraform step re-run on Go 1.25), #511, #513,
  #517, #520, and the #520 and #493 prepared merges. #493 is gating; the combined gate comes next.
- **#455:** cleared at da336d428. SQLite passed 923/0, and Postgres' failures were load flakes: code-sql
  untouched, the rest 2/2 alone. It lands in a free gap or with batch 7.
- **#501:** its re-review fixes are on a seventh gate.
- **#546:** AGENTS.md now carries the reference and issuer rules for other products.

**State, 2026-11-20 16:55.** Batch 5 is on main (8b20ce290, CI green, exactly the gated tree):
#407, #430, #440, #451; #277, #397, #432, #488; #357, #330; #465, #514, in four rounds.

Batch 6, each gated green, none enqueued:
- #499 claims;
- #511 batched writes;
- #494 the first hour;
- #487 validity checks;
- #513 decoys session-only;
- #520 placeholders;
- #517 interception CA;
- #467 then #493, the Postgres decoy dashboard and decoy where, now on main's #432.

#501 (members v1): the re-review closed most findings. Two are being fixed:
- things made through a person's token (shares, exports, leases) outlived their removal;
- registrations stopped when the token that made them expired.

Kubernetes, GitHub App and Snowflake also gain the maker check. #501 lands with #445 after that, ahead of
previews' #524. #455 waits for a full
`--docker` run at low load.

**Ranked, 2026-11-20 04:30** (the thirty-third product review, cold, after batch 4 and #475). The store
is still level with or ahead of the category. The gaps are at the new features' seams.

1. **What a person made keeps their old rights after a demotion** (security, S; fixing in
   `feature/roll-members`):
   - `maker_reaches` asked only `Read` on the project, so a demoted admin's backup schedule kept
     exporting every value, and their identities, gateways and ACME kept running;
   - the fix: the action and scope each exercises, and identities capped by their maker's current
     grant, as `Within` caps tokens.
2. **Placeholders' seams** (security, M; all fixed in #520, CI green; the swap 45–69 ms → 0.08 ms):
   - refuse wildcards under shared-hosting suffixes (`*.vercel.app`);
   - sensitive keys only by an opt-in per binding;
   - the person behind an in-process consumer re-checked;
   - reach lists placeholders and a cut-off ends them;
   - alerts limited per placeholder;
   - no placeholder for a key holding a reference;
   - `substitute` reads one key, cached with #248's invalidation, measured.
3. **A proxy that can swap** (L; waits on previews, apps and agents): apps' egress is `CONNECT`
   tunnels and never sees headers. roll secrets' half: a short-lived interception CA per consumer from
   the project's PKI, name-constrained to the bound hosts. Previews first (the strictest egress).
   roll secrets' half is #517 (gated green):
   - a self-signed CA per consumer, with critical name constraints to the bound hosts only;
   - the key in memory only, with no HTTP route;
   - at most 24 h, revoked when the consumer ends;
   - `covers`/`leaf` helpers and a five-step proxy contract.

   After placeholders lands, it implements `BoundHosts` and ends the CAs with the consumer.
4. **Validity checks without tripping canaries** (M; #487, gated green: AWS accounts read offline, canary
   accounts never tried, live AWS only by operator opt-in; roll tokens all answer alike):
   - AWS: the account read from the key id offline, known canary accounts skipped, a live call only by
     opt-in;
   - roll's own tokens are looked up, never used, and decoys aren't findings.
5. **The first hour's remaining dead ends** (S): the quick start's code used `production` with a
   staging-only token, and step 2 assumed a project the CLI couldn't make (both fixed here). Still to do:
   - the empty states lack the install line (`lib/install-line.ts`);
   - `secrets-page.tsx:63`'s "no such project" has no link anywhere.

   #494 (CI green): the install line, a no-such-project page with ways out (shared with the canvas),
   links from empty states, Try again on load errors. After #430 (one conflict in `ui.tsx`).
6. **Workload identities match nested and list claims** (S; #499, gated green: JSON-pointer keys in
   `claims`, lists match on any element through a pointer only (a plain name stays string-only, so
   nothing made before widens), one shared `roll_oidc::check_identity`):
   `roll-oidc`'s `Jwt::str` reads top-level strings only, so Keycloak's `realm_access.roles` and
   Entra's `groups` can't be matched. Doppler and Vault both take JSON pointers and lists.
7. **Big changes on Postgres** (performance, M): 9,900 keys in one change took 11.7 s against 0.5 s on
   SQLite, about one round trip per row. Batch the rows in `write.rs` and add the case to `bench`.
   #511 (CI green): rows read up front and written in one pipeline of 128-row statements on
   Postgres; 9,900 new keys 11 s → 1.1 s (49,529 → 236 statements), SQLite unchanged.
8. **Test gaps** (S):
   - demotion, not removal, stopping syncs, #407 bindings, placeholders and schedules;
   - #357's "one node rotates" with two nodes: #514 (on #357), no race found on SQLite or Postgres,
     and it catches one when `rotate`'s guard is removed;
   - placeholders' wildcard and alert limits.
9. **Integration:** previews to embed `proxy::filter` first; MCP's `secrets_audit` (#479); apps' #474
   and search's #361 resolve as the runtime until #407 lands (known).

10. **Decoys only to a signed-in session or the operator** (security, S; #513, CI green: no plant
    audit row, reminders treat decoys as any key, decoy notices only to a URL a session set): roll MCP found an admin's agent, or a leaked admin token, could
    read `Secret.decoy` and skip the decoys, which defeats the tripwire for exactly who it's meant to
    catch. Every token caller gets answers identical to a normal key's (list, get, Health, audit), and
    planting a decoy needs a session or the operator.

Batch 5 conflicts with main again after #475 (#357, #465, #277, #432); it's being re-levelled.

**Ranked, 2026-11-19 20:45** (the thirty-second review's list as it stands, plus what today's
work found). First what can be built now:

1. **A key of its own for audit digests** (security, M; on #357 and #281). The issuer's key signs
   cloud federation, apps' instance tokens and #281's hourly digests. One leak forges all three, and
   since #357's emergency rotation, digests signed by a pulled key still verify (their only other
   protection is the chain head's signature under `ROLL_SECRET_KEY`). CloudTrail signs digests with
   keys of their own. #465 (on #357, gated green): an Ed25519 digest key with its own JWKS and rotation;
   pulled keys' digests are refused unless a later good digest covers them; old digests are `legacy`.
2. **Validity checks for found credentials** (M; #303 on main; GitGuardian's validity checks). A
   `secret_unstored` notice or a scan finding says whether the credential is live, checked once,
   bounded and audited. AWS uses `sts:GetCallerIdentity` signed with the found pair, GitHub `GET /user`,
   Stripe `GET /v1/balance` and Slack `auth.test`; each goes through `roll_net::PublicOnly` with no
   redirects, and the request carries nothing but the credential. A live one ranks first and offers
   rotate-on-leak where roll has a rotator for its kind. Opt-in per project, since it contacts the
   provider. Building: `feature/roll-secrets-validity`.
3. **Postgres decoys in the dashboard** (S; #432): the "Add a decoy" dialog offers only `rlt`. #467 (on #432):
   the dialog offers a Postgres login with a database picker. Follow-up: name the endpoint in the
   badge and Health finding (the server's detail).
4. **Members v1** (building, `feature/roll-members` on #445; decisions D1–D9 with the user, (a) for now).
5. **Use without seeing** (building, `feature/roll-secrets-placeholders`): host-bound keys,
   `rlph_` placeholders, `substitute`, and a filter that previews', apps' and the agents sandbox's
   proxies embed.
6. **The audit chain under concurrency, and a sealed-start rolld test** (#475, on main 2026-11-20 as
   56f87f031, alone before batch 5). The measurement found main's #281 chain collapsing under concurrent readers:
   - 5,000–72,000 serialization retries per 10 s;
   - reads 6–10x slower;
   - most secret changes failing;
   - reads past their 40 retries answering *without* an audit row.

   The fix batches one node's reads into one READ COMMITTED transaction (up to 256), and a new
   `code_sql::Conn::claim(key)` queues batches and changes on a per-project advisory lock, so they
   don't retry. It keeps the chain unforked. At 50+ readers it's within 2x of pre-#281. Plus a test
   booting rolld sealed.
7. **`Reference::parse` exact** (#451, gated green; batch 5): the trim bypass.
8. **roll code's secrets out of `Debug`, and its signing secrets sealed** (#455, gated).
9. **A product sync's maker can pause it** (S; #304's rule was admin for every pause; #488, gated green): pausing or resuming needs what running the kind needs (`reveal`
   for a product's kind), and resuming what a cut-off paused stays admin only.

Waiting on another thread or on a merge:
- **Batch 5:** #407, #430, #440; #277, #397, #432; #357, #330 (landed 2026-11-20, 8b20ce290); the strict
  parse joins it.
- **apps:**
  - the git login and `roll://` variables through #407, with the #399 stopgap on main;
  - queue links on #274.
- **search:** #361 on #407 and the strict parse; the cluster password sealed (`fix/search-cluster-password-sealed`).
- **previews:**
  - #436 approved;
  - preview variables kept only in roll secrets (their Next 4);
  - a proxy hook for placeholders.
- **roll MCP:** value-request tools on #277; `secrets_verify_audit` on #281.
- **The user's call:** members D1–D9, R4 (GCP KMS, Azure Key Vault, HSM), and the real-account checks.

**The thirty-second product review** (2026-11-19 morning, against Vault/OpenBao, Infisical, Doppler,
AWS, 1Password, Akeyless, Bitwarden SM and GitGuardian; open PRs and branches checked first). The store
is still level with or ahead of the category. The real gaps are elsewhere: three places where
credentials reach someone who was never checked (two on main, one in our own #357), the missing project
members, the first hour, and the other products' credential inputs. Ranked:

1. **roll apps' Git login by secret name** (on main, the #361 hole live). `PUT …/git-login {secret}`
   is apps `write`. Each fetch resolves that key from production, sensitive keys included, and sends it
   as Basic auth to the source URL's host, which the caller chose. Asked of roll apps (2026-11-19): a
   reference that is reveal-checked when set, a stored maker, and `maker_reaches` before each fetch.
   Stopgap: `deploy` instead of `write`, and no sensitive keys.
2. **#357 before it lands** (out of batch 4; fixing):
   - `ForApps::issue` gave untrusted `pv-…` previews tokens, so a fork's code got a cloud session
     through a `*` trust;
   - an emergency `rotate --now --retire KID`;
   - a separate digest key (planned).
3. **Agents' sandboxes copy an environment without the person's reveal** (on main):
   - `for_owner` only checks owner management and `counts_for_repo`;
   - the fix is to check `Caller::allows(Reveal, env)` and store the maker;
   - S, this side's `for_owner` plus the agents surface.
4. **Project members and roles for people** (R1, L): designed in #445 (`docs/members.md`). A member's
   session becomes a roll-access `session` grant built on every request; there are four built-in roles
   and resources have homes, and v1 covers workspace and secrets. It waits on the user's decisions D1–D9
   (role names, viewers seeing values, developers' defaults, admins and production, invites, homes,
   removed members' tokens, operator projects, admin accounts); the doc is written with option (a) of each.
   v1 is #501 (on #445; option (a) of every decision, each one constant in `roll-auth/src/roles.rs`),
   gated green with the demoted-maker fix. The security review said don't merge yet. Its findings,
   being fixed:
   - critical: a person's wide token could make anyone's resource its project's home and link its
     credentials;
   - high: any member, a viewer too, got previews' full powers, a shell included;
   - medium: a removed maker kept previews' team access, made things such as rotators acted on after
     removal, and homes outlived their resource;
   - low: invites and admin accounts as owners.
5. **Token defaults** (P13, S):
   - the CLI gives every environment, `reveal` and no expiry; the dashboard pre-picks production and 90
     days;
   - proposal: `--for-env` required (or the directory's setup environment, never a protected one
     without the flag), 90 days by default, and `--no-expiry` explicit;
   - the dashboard pre-picks development.
6. **The first hour after #370:**
   - create or pick a project from `setup`;
   - empty states offer `roll login <origin>`;
   - split the Syncs tab (15 panels) into Syncs, Credentials, and Keys and certificates.
7. **One `[secrets]` table, three owners:** #303's `RawSecrets` (deny unknown) must accept #397's
   keys when both land.
8. **Plaintext credentials in other products' stores:**
   - search's cluster password, webhook and review-app secrets, and preview vars;
   - `AiConfig.secret_key` derives `Debug`;
   - seal each, and write `Debug` by hand. Asked of search and previews (2026-11-19); roll code's
     share is building here (`fix/code-no-secret-debug`).
9. **`roll://` where other products take keys,** each with a reveal check when set, a maker and
   `maker_reaches`:
   - previews' variables;
   - apps' drains and on-call keys;
   - registry logins;
   - search's snapshots (#361, fixed that way) and connectors (#318: pause a branch or restore);
   - agents' endpoints, through the daemon's own login;
   - reviewer keys.
10. **Use without seeing:** a proxy that swaps a placeholder for the key, only for the key's bound host
    (Infisical Agent Vault, Akeyless SecretlessAI). L; waits on previews, apps and agents' proxies.
    roll secrets' half (host-bound keys, `rlph_` placeholders, `substitute`, a reference filter) is
    building on `feature/roll-secrets-placeholders`.
11. **Unmeasured:**
    - #281's head row under many concurrent writers on Postgres: measured, and fixed in #475 (on main);
    - #248's "1,000 pods … 0.1 core" is arithmetic, not a load run.
12. **Test gap:** rolld booting sealed (#367): no product or lease before the unseal, and every reader
    gets the key after it.

Found since: `Reference::parse` trimmed its input, so ` roll://qa/KEY` passed previews' checks as
plain text but resolved as a reference (previews' #436 review, fixed there in 7c0b10ab2). The root fix
is building: parse is exact, and a value that's a reference only once trimmed or lowercased is an error
(`fix/secrets-strict-reference`). Apps and search were told to decide "is a reference" only through it.

Doc fixes found: all made (the checks' `secrets =` line, #20's heading, the old open-PR list; #357's
"same check as `resolve`" in #357 itself).

**State, 2026-11-18 13:25.** Built today, each green and mergeable: #248 (the polling path), #262
(Terraform for batch 1's kinds), #264 (doc fixes), #274 (queue links), #276 (a split deployment key),
#277 (secret requests), #279 (queues' default-exchange permission), #281 (a tamper-evident audit log),
#284 (six more import sources), #290 (AWS IAM login), #303 (credentials roll doesn't hold yet); and
on the #132 stack, #165/#166's engines through the gateway, their `rotate-admin`, and Cassandra's
cache waited out. Ranked now:
1. **The thirty-first security review** (2026-11-18 afternoon, today's PRs read cold): one high, three
   medium, six low, all being fixed (#277 4a23eabc2: answers stored literally, a per-request attempt
   count in its row, replays refused; #303 e4be05f1c: block fails closed; #304 3b1757d45; #290 6dd730cf8: a signed
   login used once, a server ID derived from the deployment key; #262 217be1314: the rotator's `url_wo`;
   #274 e1e2a89e7: a user's URL built from parts; #248 2236e55d1: an LRU, not a clear-all; #281 78e1c7c9d: only keys whose sealed half opens under the
   deployment key sign, verify or reach the JWKS, a keyed MAC on the head, hourly digests with a
   `stale` check, `verify --file` needs `--jwks`). All the findings are fixed. High: #281's verify (and the public JWKS) trusted any key a
   database writer added; now only keys whose sealed half opens under the deployment key. Medium:
   #281's head forgeable after a tail cut (a keyed MAC and hourly digests with a staleness check);
   #277's answer `${production.X}` became a reference (stored literally); #303's block mode failed open
   when the lookup errored. Low: #290 replay and a shared default server ID, #262's rotator URL in state,
   #274's ignored URL setters, #281's offline verify, #277's limiter reset, #248's cache clear-all.
   Sound: #276, #279, #284 (one stderr note).
2. **Batch 2 landed** (2026-11-18, one lock turn, main 49a6f1d8c): 24 PRs in four queue rounds, #79–#229,
   #81–#213, #92–#212, #117–#128; main's tree after each round was exactly the simulated one, and
   after the last the exact tree re-gated on the day's main (3feff202b). Batch 3 is ready (2026-11-18
   evening): #132; #136 + #165; #173; #166; #144 in five queue rounds, levelled, each gated, the
   combined tree gated on main c59fca59c with the live outside-databases check (`batch-3.txt`,
   `batch3-round.sh`). **Batch 3 landed** (2026-11-19 02:56–04:07, one lock turn): #132; #136 + #165;
   #173; #166; #144 (main cd65c5fcb = the gated tree plus #375), with roll redis's auth-bypass fix #375
   ahead of it in the same turn. Batch 4 (landed 2026-11-19 18:41; this was its plan at 04:15): today's
   PRs (#248 #262 #264 #274 #276 #277 #279 #281 #284 #290 #303 #304 #329 #330, the issuer work) and
   #86 (`batch-4-notes.txt`).
3. **R2, cut off everything that reaches a folder** (#304): tokens through roll-access's bulk revoke,
   identities disabled, leases ended, shares and exports cut, syncs paused, an optional freeze (423).
   Fixed (3b1757d45): a project admin could revoke the operator's all-projects tokens (a cross-project
   denial of service); now only grants whose every rule names this project are revoked, the rest are
   listed as `skipped` ("the operator revokes it"); the operator and roll admins revoke all.
4. **The dashboard's "Store in roll"** for #303's notices (#330, on #303): roll reads the credential
   from the change's own file and stores it in the project trusted for the repo (a writer on the repo
   with `write` on the folder; never overwrites, never returns the value); it found #303 naming the
   wrong line past blank context (fixed on both). And **R5, `roll://`
   wherever another roll product takes a key** (#329): `Secrets::resolve_for(resource, roll://…)`, the
   project from the resource's trusted canvas (a reference may only name that one), read as that
   project's runtime (an untrusted ephemeral environment never reaches a protected one; dynamic
   credentials refused), audited as the resource; `digest_for` to notice a rotation. Storage's
   replication targets take references first (resolved per client, rebuilt when the digest moves;
   operator-only to set, for now); `ResourceRef::untrusted()` (04b3e7ddb) narrows a branch or copy to
   an untrusted read (no protected environment, no sensitive key; the digest refuses the same).
   Next: let whoever may `reveal` the key set the reference, then
   search's snapshot repositories, the reviewer's provider keys and the gateway's upstreams, each
   with its thread.
4b. **roll's issuer for the other products** (#357, on #281; asked by roll apps): an in-process
   `Secrets::issue(subject, aud, ttl)` for apps' per-instance workload tokens (`roll-apps:<project>:<service>:<env>`,
   Vercel's OIDC federation), scheduled key rotation (publish ahead, retire after the last token
   expires), and secrets' own workload logins trusting roll's issuer for apps' subjects; one issuer,
   never a second one. Built: 90-day rotation (published 2 days ahead, old key kept 2 h after the
   switch, retired keys kept sealed so old audit digests still verify), one audit row per subject,
   audience and hour; `Secrets::issue` refuses an untrusted preview environment and one the project
   doesn't have (#357's fix).
5. **A sealed start for the deployment key** (#367, on #276): `ROLL_SECRET_KEY_SEALED=<split>:<threshold>`
   starts rolld with only `POST /v1/secrets/unseal` up; K shares from the split, checked against the
   store's key id, start every product with the key in memory (all eight readers take it as a value);
   10 shares a minute per address, 5 failed sets an hour per split (durable); `roll secrets unseal
   --node …` for a fleet, each node unsealing itself. The thirty-second security
   review (2026-11-18 evening): one high, two medium, four low, all being fixed. High: #304's freeze
   let roll's own products through (every in-process reader counts as the folder's admin), so a
   frozen folder still fed previews, checks and apps; only people, the operator and roll-access
   grants pass now (#304 14e7e55ab: a private `frozen` flag only the operator's, a person's and a
   token's constructors set; tested for apps' deploys and untrusted checks). Medium: #281's old head replayed after a cut (4b4fe4033: the sweeper signs a
   `gap {after, since}` digest when the chain's newest digest is stale or isn't the head's; verify
   reports it from then on); #277's
   replay refusal beaten by changing the box's `v`. Low: #330 storing an outsider's credential, #290's
   duplicated `Signature=`, #329's source bucket choosing the reference's project (and its refusal
   text an oracle), #248's per-reader eviction. Sound: #303's fixes, #274, #262, #281's key provenance.
5b. **The thirty-first product review** (2026-11-18 night): the store is level with or ahead of the
   category; what's left is the first ten minutes, two test gaps and three products' own credential
   copies. Ranked:
   - P1–P4, **the first ten minutes** (#370: a saved login's URL by default, approvals printed and
     resent with `X-Roll-Approval`, the quick start and dashboard dead ends fixed): the CLI
     keeps 127.0.0.1:7070 after `roll login`; the first `roll run` waits silently on an `ask` approval
     it can't handle; the quick start has no install or login; the dashboard's commands dead-end.
     Then a committed `[secrets]` in `roll.toml` and an interactive `setup` (Doppler's `doppler.yaml`),
     designed (2026-11-19) so a repo suggests and the person approves, as direnv's `allow` (building:
     `feature/roll-secrets-repo-setup`):
     - `roll.toml` may hold `[secrets] project = "shop"`, `environment = "development"`, `path`, and
       `branches = { "release/*" = "staging" }`; never an API URL, a token or a value (the #58 lesson:
       a repository never chooses where a token goes).
     - It's only a suggestion. `roll run` uses it once the person has approved that exact table for
       that directory (`roll secrets setup` with no flags shows it and asks; the approval, with a hash
       of the table, goes in `~/.config/roll/secrets.json`, so a changed table asks again). Until then
       `roll run` says what the repo suggests and how to approve it, and uses nothing.
     - A protected environment is never taken from the file, even approved: `-e`, `ROLL_ENV` or the
       person's own setup only (a cloned repo can't route production values to code it ships).
     - Precedence: flags, then env vars, then the person's own setup, then the approved table, then
       `development` and `/`. `doctor` names which one was used.
     - `setup` in a terminal picks from the projects and environments the login can see.
   - P5 + P14 (#365): CI snippets that work on a fresh runner (the GitHub Action, or the install step
     plus `roll run`; GitLab's `id_tokens`; Azure's install step) and the write-flow component tests
     (a pasted `.env` as one change, a protected edit becoming a change request, a stale edit reloading).
   - P6, MCP tools: import, rollback (destructive), create environment, link and link targets in roll
     MCP's #353 (rollback's `bypass` left out); secret requests and cut-off when batch 4 lands; the
     development preset change (#354) was closed after roll MCP's review: `roll login` is an ordinary
     OAuth client, so a looser preset would loosen every approved agent too. The first run is fixed in
     the CLI instead (the approval link at once, then a remembered "allow from now on"); remembered
     approvals are already per environment and folder and tied to the asking client (revocable on the
     token page); roll MCP adds 7- and 30-day choices next.
   - P7–P8, **tests** (#394, on main 2026-11-19): `roll run --watch`, the six links
     to roll's own products, and a `pnpm test:secrets` gate.
     The links test found **an authentication bypass in roll redis**: `AUTH default <db>_<anything>`
     answered +OK (a `nopass` default user behind the platform-secret check), so any password, and any
     rotated-out one, logged in. Fixed in #375 (a hosted database's default user
     has no password, never `nopass`; a rotated-out secret refused at the next AUTH), landing ahead of
     batch 3 in its own lock turn.
   - P9–P11, apps' variables, search's connectors and previews' variables as `roll://` references
     (each with its thread; search's is building on #329).
   - P12 members (R1, waits); P13 a CI token's defaults (all environments, no expiry: the user's call);
     P14 a dashboard write-flow test; P15 R4 and real-account checks.
   - Inconsistent: `roll://` is also the desktop app's deep-link scheme (`roll://agents/…` parses as a
     reference; proposed: the desktop app moves to a scheme of its own, e.g. `roll-app://`, since
     `roll://ENV/KEY` references are in the CLI, VS Code, the clients and #329 already; the agents
     owner's call), `doctor` suggests `export > .env`, GitLab is listed without a snippet.
6. **R10, decoy keys** (#392, gated green; merges main after batch 4, re-gated, batch 5): a key
   holding a decoy `rlt_` token (#270), minted in process by `Access::mint_decoy`, alerting
   `decoy_used` (audit, notify URL, top health finding; at most hourly) through `Access::on_decoy`. Only
   admins see it's a decoy. A Postgres decoy role needs a login hook in pg-proxy, designed in #392's
   body. IAM users need an account;
   **R4** (GCP KMS, Azure Key Vault, HSM) and confirming #290, #284, #227, #229 on real accounts: the
   user's call.

**The thirtieth product review** (2026-11-18, against Vault/OpenBao, Infisical, Doppler, AWS, 1Password,
Akeyless, GitGuardian and Bitwarden SM, open PRs checked first) ranked these gaps; its four doc
inconsistencies are fixed in #264:
- R1. **Project members and roles for people** (L; every competitor's unit of sharing): v1 built
  (docs/members.md: workspace and secrets; "What v1 leaves out" lists the rest).
- R2. **Cut off everything that reaches a folder** (M): reach lists it but can't act; one action to
  revoke its tokens and identities, end leases and shares, pause syncs and optionally freeze reads.
  roll MCP's #267 (on main) has `Access::revoke_many(ids, why, by)` (atomic, ≤500 ids, one audit row
  per grant). Building: `feature/roll-secrets-cut-off`;
  reach passes its own grant ids, and leases, shares and syncs stay in secrets' transaction.
- R3. **Credentials roll doesn't hold yet** (#303): a landing's added lines run through
  `roll_auth::credentials` with the stored-value check; a match roll doesn't hold is a `secret_unstored`
  notice naming kind, `path:line` and the `roll secrets scan --store` command (never the value), or a
  blocker with `roll.toml` `[secrets] unstored_credentials = "block"` (admin bypass audited);
  placeholders (`example`, `fake`, `test`… parts, runs of one character) and `.roll/secrets-allow`
  (fingerprints, path globs) are skipped. `scan --store` moves one into roll and prints the env read.
  Next: the dashboard's "Store in roll" (roll reads the value from the change's blob).
- R4. **GCP KMS, Azure Key Vault and HSM keys** (M), next to AWS KMS: real accounts (the user's call).
- R5. **`roll://` wherever another roll product takes a key** (M): agent endpoints (they take `op://`),
  the gateway's upstreams, review provider keys, storage replication and search snapshot credentials.
- R6. **roll queues as a link provider** (#274): live host, port and vhost; an AMQP URL with a user
  of its own, managed or dynamic, and only the access asked for (read, write, configure). It found
  roll queues checking a default-exchange publish against `""` rather than `amq.default`, fixed
  for every protocol in one place as RabbitMQ does (#279, checked against real RabbitMQ). roll apps
  wires queue links into `RollResources` once #274 is on main: tell them when it lands.
- R7. **AWS IAM login for EC2/ECS/EKS** (#290): a signed `GetCallerIdentity` the server sends only to
  AWS's own STS hosts (URL rebuilt, body exact, `X-Roll-Server-ID` signed, ±5 min), matched against
  one role, session prefix or user; CLI, Go and Python sign from the standard credential chain
  (IRSA, ECS/Pod Identity, IMDSv2). Unconfirmed against real STS: an account (the user's call).
- R8. **A tamper-evident audit log** (#281): each row hashed onto a per-project chain in its own
  transaction (one head row, a unique seq: no forks, checked with two racing nodes), hourly digests
  signed by roll's issuer key and sent to the sinks, `audit verify` online or offline against an
  export and a saved JWKS; retention cuts only at a signed digest. ~+0.1 ms per audited write on
  SQLite, ~one round trip on Postgres with one writer. Under many readers of one project (a
  500-pod rollout) the chain alone capped reads at ~500 a second with p99s of seconds, left reads
  unaudited and failed most changes; reads are now group-committed per node and queue across nodes
  on a claim, and a change that loses once runs next holding it ("Tamper evidence", "Under load").
  Known: rows cut from the end along with the head, after the
  last delivered digest (≤1 h), aren't detectable without a sink.
- R9. **Secret requests** (#277): `roll secrets request KEY…` or "Request a value…" makes a link;
  the answerer's browser seals each value to the request's own key, roll opens it only in the write
  that stores it (a change, or a change request the requester can't approve), once, rate-limited,
  audited. Screenshots in `~/.local/share/roll-review/secrets/requests/`.
- R10. **Honeytokens** (M): decoy `rlt_` tokens are roll-access's (roll MCP's #270, on main);
  database roles and IAM users that alert on use are this side's, once those land.
- R11. **Moving in from 1Password, Bitwarden SM, Parameter Store, GCP, Azure and Akeyless** (#284):
  each through its own CLI with the user's login, bounded (500 secrets, 16 MiB, 60 s), stopping on an
  unreadable secret or a collision, `--dry-run` for every source, `--sensitive` opt-in for
  SecureStrings. Shapes from vendor docs only (Akeyless least sure): real accounts are the user's call.
- R12. **A split deployment key** (#276): `roll secrets key split/combine`, Shamir N of M in a small
  I/O-free crate (`roll-keyshares`), checksummed `rlks_` shares with a set id and a key check (a
  wrong mix is an error, never a wrong key); backup keys split the same way. Next: a Vault-style
  sealed start (`ROLL_SECRET_KEY_SEALED`, holders unseal over the API), designed for fleets. R13. **A FIPS build** (L).
  R14. **Format-preserving encryption** (M, low). R15. Clearing a copied value from the clipboard: not
  done, since a page can't read the clipboard without a prompt and clearing blindly could wipe
  what was copied since.

0. **Land in batches**: batch 1 landed in one lock turn (2026-11-18): #69, #70, #72, #76, #77, then
   #73, #75, #71 (main's tree afterwards was exactly the gated one), and #147 behind them, gated on
   their tree. One flake on the way (a CI checkout hung past its 2-minute limit; #71 re-queued after a
   re-simulation of the new order). Next: batch 2 the same way, prepared and gated before the next
   lock turn: ready (2026-11-18 07:20): 24 PRs in four queue rounds (`batch-2.txt`, `batch2-round.sh`),
   every one levelled with main and the combined tree gated on every step (Rust on SQLite and
   Postgres, access, CLI, dashboard, and the Go, Terraform, Python, Ansible, Ruby, Java and .NET
   clients); logged in the merge order after search's next turn, waiter `rs79-lock`. Then batch 3:
   the #132 stack in four rounds (`batch-3.txt`; #166 last, after #173), with #144, #86 and #248.
0a. **The thirtieth independent security review** (2026-11-17, tonight's PRs #132, #136, #144,
   #165, #166, #173, #194, #227, #229, read cold): no high finding; ACME's JWS, nonces, EAB and
   tls-alpn-01 verifier, the gateway's allow-list, bounds and revocation, and every PR's network
   guards held. One medium: a gateway's link could be opened with a person's session (a WebSocket
   GET has no CSRF check, and the session was checked once) — now only the gateway's own token or
   login opens it (#166). One low: an Atlas user named `.` or `..` turned the API path into the
   collection's (#173, refused now). One to confirm: ScyllaDB's audit log with a password in a CQL
   statement: answered (#165, d8d21b7c7). ScyllaDB's audit log masks a plain password, but tracing
   keeps the whole statement on both servers, so rolld now sends `HASHED PASSWORD` (bcrypt, made in
   rolld) wherever the server takes it: always on Cassandra, and for a superuser's `CREATE ROLE` on
   ScyllaDB, whose `ALTER ROLE … HASHED PASSWORD` is a server error. Checked live on both.
0c. **Azure DevOps** (#227): a sync to variable groups as secret variables, and Azure Pipelines
   logging in with its own OIDC token (one pipeline or service connection per identity), as Vault,
   Infisical and Doppler integrate. Azure never returns a secret variable's value and its update
   rewrites the whole group, so the sync sends every value on each push and refuses a group holding
   secrets it doesn't manage. Unconfirmed against a real organization (the user's call).
0e. **Four more syncs** (#229): Supabase Edge Function secrets, Cloudflare Pages variables, Azure
   App Configuration and HashiCorp Vault / OpenBao KV v2 (the gradual way off Vault), as Infisical
   syncs to each; a `refuses` hook so a kind can refuse key names (Supabase's reserved prefix).
   Merges clean with #227. Unconfirmed against the real services (the user's call).
0d. **A Ruby client** (#212), the same surface as the TypeScript, Go and Python ones, both sealed-box
   vectors opening in it, standard library only, with Rails notes. Writing it found that Python's
   and Go's `Values` printed every value and their `Client` the token (#213 fixes both).
   Transit is now in the Java (#105, 5570f5b11) and .NET (#110, 7c91476c7) clients too, its errors
   and printing checked to hold no plaintext; next, TOTP for Java, .NET and Ruby once #194 is on main.
0b. **The polling path at scale** (#248): a 304 or an unchanged `digest` at 9,900 keys now takes
   0.1 ms on SQLite and 3 ms on Postgres (was 99 and 169 ms), so 1,000 pods polling every 30 s cost
   ~0.1 core (worked out from single-request latency, not a load run), not ~5.6. A digest reads only the columns it hashes and each folder once, and each node
   keeps it while one read of the database's state (the project's change number, environments,
   imports, required keys, shared folders' projects) says the same, every permission it asked
   answers the same, and live links' generations and fingerprints hold. The first digest after a
   write is computed once for every reader with the same answers (the thirty-fourth review's item 7).
1. **Land the queue**, one at a time when the merge lock is free (the order is in "Landing"):
   four threads share it, so each gate runs before the lock is taken and only main's newest change
   is merged under it.
1a. **Measured after #151** (2026-11-17, 9,900 keys on SQLite, release, five alternating runs each way
   at load 25–68): every read's before and after ranges overlap, an ephemeral environment's values
   included (the path that loads its branch list); #151 adds one lookup per request, not per key.
1b. **A screenshot pass over the dashboard** (2026-11-17, #132, #136, #165, #166, #173): it found
   textareas one line tall (a fixed height beat `rows`; now `textareaField` everywhere, main's CA
   boxes too, on #144), cut-off labels, Active Directory's fields offered for an OpenLDAP directory
   (link targets now ask each resource for its fields), and a gateway's token refused on a server
   without sign-in (the operator now makes it, as `POST /v1/access/tokens` does). All fixed and
   carried up the stacks.
2. **Keeping the queue level with main** (done 2026-11-17 for all fifteen that conflicted, each
   pushed on its own checks; #76 now also marks syncs when a product's own route changes a
   credential, as #74 put that in `source_changed`). **Landing the stacks together where it saves a
   lock cycle** (#128 on #105, #124 on #102, #95 on
   #79, #117 on #121, #136 on #132), and the Spring integration's first CI run (#128).
3. **ACME on project CAs** (#144), as Vault's PKI and step-ca serve it: cert-manager, certbot,
   Caddy and lego get certificates from a project's CA with no roll client (lego and certbot checked
   against a local rolld). Accounts always bind an EAB key (looked up on every request, ended with
   its maker's reach); `dns-01` through public DNS, `http-01` through the guarded client, and
   pre-authorized names for clusters roll can't reach (off by default); `tls-alpn-01` and reused
   authorizations too (Let's Encrypt's behaviour cert-manager expects), and finalize re-checks the
   binding. Next on it: checking pre-authorized names through a gateway (item 6) once that's built.
4. **Active Directory, done against Samba AD** (#132): every step run against a real domain
   controller with a delegated bind account. It found AD's hour-long acceptance of a reset account's
   previous password, now closed by a throwaway reset first; logins also hand out the UPN and
   `sAMAccountName`. Left for a Windows Server: its password policies, AD CS certificates, LDAP
   signing and channel binding, replication between domain controllers (an account of the user's).
4b. **Elasticsearch, OpenSearch, Cassandra and ScyllaDB outside roll** (#165, on #132): Vault's
   database engine has them; two roles taking turns and a login per lease, each checked live
   (Elasticsearch 9.5.5, OpenSearch 3.9.0, Cassandra 5.0.9, ScyllaDB 2026.3.3). Cassandra's
   credentials cache let a dropped role in for ~1.6 s, so a lease's end sets a password nobody knows
   first. All four go through a gateway too (#166, checked against the real servers), and their admin's
   own password rotates (`rotate-admin`, #165: it fell through to Postgres's statement), waiting out
   Cassandra's ~2 s credentials cache; other schemes are refused by name. Managed rotations wait
   the same way (with an app logged in, Cassandra refused the handed-out password 12 of 12 times
   before, 0 after). Known: the cache is per node, and an ended lease's password can still log in
   for up to `credentials_validity` (documented; the servers' setting shortens it).
   Next: OpenSearch's narrower permission set, and Couchbase and Oracle (Vault has both).
4c. **MongoDB Atlas through its Admin API** (#173, on #136): Atlas refuses the user commands on
   every tier and rolls back changes made another way, so the MongoDB engine never could rotate an
   Atlas login (it now refuses Atlas hosts and points here). Users taking turns and a user per
   lease (`deleteAfterDate` as a backstop) through an organization service account, as Vault's
   Atlas plugin. Unconfirmed against a real account (the user's call).
4d. **TOTP codes from a stored seed** (#194), as Vault's TOTP
   engine and 1Password's one-time passwords: a `totp` key hands out its current code (`reveal`,
   audited, never the code in the row) to CI that logs into a 2FA-protected service; the CLI, the
   dashboard (a countdown) and the three clients.
5. **Terraform resources and MCP tools for the new kinds**. #262 (2026-11-18): rotators, audit sinks,
   GitHub Apps, AWS roles and users, cloud accounts, required keys and the KMS key, credentials
   write-only (never in plan or state, checked), and import for every registration. Then, as each lands: SaaS
   rotators, Kubernetes clusters, backup schedules, rotation windows, directories, Snowflake
   accounts, ACME bindings; the tools with roll MCP.
6. **A gateway into private networks** (#166, on #132; Infisical's Gateway, Akeyless'): a small process inside a
   VPC that dials out to roll and carries rotations, dynamic credentials, directory calls and ACME
   challenges to private addresses, so private databases need neither a public address nor
   `allow_private`, and pre-authorized ACME names can be checked after all. Its link is a
   workload identity; it carries only the conversations rolld starts, only to the hosts its own
   local allow-list names (a compromised roll can't scan the network through it), as bytes with TLS
   end to end; on a fleet its link and the calls through it run on one lease holder. Built: Postgres,
   MySQL, SQL Server, Redis, MongoDB and LDAP/AD through it, a real agent over a WebSocket in rolld's
   tests (allow-list, backpressure, revocation) and Postgres end to end; revocation takes effect at
   once (every stream open looks the token and record up, ~0.11 ms); a stream opens in ~1 ms, not
   44 (Nagle: `TCP_NODELAY` on every hop, rolld's API listener and roll-edge's included).
   ClickHouse and RabbitMQ go through it too (a hyper-util client whose connector opens the
   gateway's stream, TLS in rolld; checked by hand against real servers), `mongodb+srv://` is
   resolved by the gateway with the network's DNS and its own allow-list, and a two-node test covers
   the link following its lease, a call from the other node, the internal route's signature, and a
   drain's reconnect; a gateway logs in with a workload identity and renews with no stream lost;
   port ranges in its allow-list. Next: ACME's pre-authorized names checked through a gateway (once
   #144 and #166 are both on main).
7. **Pulumi** (#145): roll's Terraform provider through Pulumi's any-Terraform-provider package
   from a local build (no registry release), with TypeScript, Python and Go SDKs generated and an
   end-to-end `pulumi up`/`destroy` checked. Pulumi keeps every input in its state (encrypted), so
   values can't stay out of it as they do in Terraform. Next on it: a CI job, and publishing to
   the registries (public: the user's call).
8. **Access requests**: roll-access's `ask` rules already are Infisical's and 1Password's
   time-boxed requests for tokens and agents (a `reveal` held for an operator, approved once or
   remembered for 1h/8h/1d), so roll secrets builds no second system. #147 (on roll MCP's #114,
   which added `roll_auth::Approved`) records which approval let a request through, and who
   approved it, in the audit row and the activity timeline. Next: reveals in a protected environment
   needing two approvers, through roll-access's two-person policies (roll MCP is building them),
   matched to change requests' rule that an author never approves their own. People's own requests
   wait on projects having members.
9. *To check against the real services (accounts, the user's call):* #113's STS login
   (`GetWebIdentityToken`) in an AWS account, Entra's answer for removing a secret that's already
   gone (#92), #136's statements on a Snowflake account, and #173's Admin API calls on an Atlas
   project.
10. *With roll MCP:* their credential guard checks what an agent's tool call would write against
   `POST …/leaks` (only values the token may reveal; `sensitive` keys never, so it can't be an
   oracle), marked `X-Roll-Guard: 1` (#169 audits it apart). *Waits on roll MCP:* #114 (`roll-oidc`, approved here), the GitHub Action's `client-id` mode
   (their diff, reviewed here), and their `roll_machine_client` Terraform resource (their diff
   comes here first).
11. *roll search's #134* (inference keys from roll secrets): on main (e7d0389af, 2026-11-19); offered for
    services taking an `api_key` or Entra ID; Bedrock's access keys and Vertex's service account (several
    fields) are a later step. Re-reviewed and approved 2026-11-18 (a
    comment on the PR): product-made syncs need `reveal`, every push re-checks its maker, the pin is
    checked before and after `_update`. Follow-up: show "pushes as <maker>" and warn before it fails. Earlier: approved once its pin covers the endpoint's whole destination
   (`service_settings` but the key, rate limit and allocations; `task_settings` where they route).
11b. *roll search's #361* (snapshot repositories take `roll://` references; on #329): reviewed
    2026-11-19, blocked. A reference resolves as the cluster, and roll never sees who chose it. So an
    Elasticsearch admin credential (a native user or the `elastic` password, outside roll-access, alive after
    its holder leaves) can point an `s3` repository at its own endpoint with a production key as
    `access_key`. That key goes out in the SigV4 header. Asked of search: a per-cluster allowlist
    set through a `/v1/search/clusters/{id}/…` route by a caller that `Caller::allows(Reveal)` on
    each environment, with its maker stored and `maker_reaches` checked before every `resolve_for`.
    Also asked: bind the resolver to the cluster id, not its name; zeroize held values and drop them
    with the repository; give one message for protected and missing keys on branches; add a
    lost-access test through the real `ForSearch`.
12. *Waits on roll apps:* their redeploys holding `wait_digest` once #79 lands; #191's log
   redaction hiding every value that comes from roll secrets unless #198 marks it `plain`: done
   (apps' #201, a41ac171c), a URL's password hidden even in a plain value.
   *Waits on roll previews:* masking secret values in a preview service's logs and its jobs' output
   (#200 and #203, approved here: logs, job output, screenshot text, health bodies, egress names;
   their egress proxy refuses before resolving, and a fork's preview now defaults to Roll-only
   egress with a DNS server that resolves nothing outside).
13. *Waits on other threads:* a live check of OpenSearch links, roll redis and nosql keeping a
   rotated password a while, splitting the CLI's `secrets.rs` once the queue has landed.
14. *Waits on linking chat users to roll accounts:* approving change requests from Slack.
15. *Waits on products serving signed-in people:* a person's project linking their own databases,
   buckets and streams.

### Landing

**To main** (merges are approved from 2026-11-15; one thread at a time behind
`~/.local/share/roll-merge/lock`). About 50 pull requests sit on #20; the plan folds them into a
few reviewable ones and closes what they supersede:

1. **#20 → main**: merged 2026-11-15 as 1ad248420 (through main's merge queue: add a pull
   request with GraphQL's `enqueuePullRequest`, since auto-merge is off), after the full gate
   including `pnpm test:code --docker`.
2. **#62 → main**: retargeted once #20 is in. It is #31–#61 merged in order with every
   conflict resolved and seventeen independent reviews' fixes, so it lands as one squash
   (decided 2026-11-15: splitting it would redo conflict resolutions #62 already settled), and
   #31–#61 close as superseded by it (their branches stay; #35 and #59, which conflict on their
   own, are in it too). #30 closes as superseded by #64.
3. **The follow-ups, one at a time**, each a small pull request of its own: each merges main,
   is retargeted to main and lands in order — #63 (CSI provider), #65 (conditional reads),
   then #74 (live digests, after #65), #66, #67, #68, #69, #70, #71 (#70 and #71 together:
   see the merge note below), #72, #73, #75, #77 (after #75), #76, #79 (after #74 and #76).
   Pairs that touch the same lines and keep both sides: #73 with #75, #66 with #72, #65 with
   #67, #74 with #76 (`source_changed` calls `due_for_source` in its write).
4. **#64** (the landed stack on roll-access): rebuilt on main once #62 is in (#30's change and
   its two follow-up commits), not merged: its branch predates the squashes of #24 and #25.

**Merging main after a squash.** A squash leaves main without the branch's commits, so merging
main into the next branch would conflict wherever it changed the squashed lines. Once main has
squash S of tip T: check `git diff S T` is empty, merge `S^` normally, then `git merge -s ours S`,
then anything after S normally. Before `-s ours` on a follow-up, check that it contains the
squashed tip (`git merge-base --is-ancestor <tip> <follow-up>`); if not, merge the tip normally
first, or `-s ours` would silently drop the commits it lacks. The same goes for #74 after #65. No base branch is deleted
until the pull requests on it are retargeted (deleting one closes them).

Before each lands: origin/main merged, the full gate (`cargo build/test/clippy`, roll-secrets on
Postgres, `pnpm typecheck` and the dashboard's tests, the SDK suites, `pnpm test:code --docker`
where AGENTS.md asks for it), CI green, a squash titled like #18–#27, main's CI green on the
merge commit, a line in the merge log.

Merging the stack in order (2026-11-14) found what no single pull request could:

- **Customer-managed keys (#37) and the providers built beside it.** Databases outside roll (#39),
  CAs (#44) and SSH CAs (#45) opened a project's data key with the deployment key alone, so their
  links failed on a project whose key a customer's KMS wraps. They unwrap first, as rotators do.
- **`roll run`**, which four pull requests changed: the fallback (#35), references in env files
  (#49), masking (#54) and the strict read (#55) now work together. A run started from the
  fallback follows a digest no folder has, so the first answer from rolld reloads it, and leases
  are kept as their renew routes.
- **Relogin (#56) and the fallback (#35)** both changed the API client: a 401 for a workload's
  login logs in again inside `answer`, which the fallback reads through.
- **Lines three pull requests changed alike** kept only the last one's version: health's finding kinds
  lost `rotation_failing`, `revoke_failing` (#48) and `certificate_expiring` (#44), and the
  registries a forgotten project drops lost `cloud_accounts` (#43). Every pull request's added lines
  were checked against the landing branch afterwards; everything else is there or extended.
- Additive everywhere else: routes, catalogue rows, `Options` fields, types and this document.

A nineteenth independent review (2026-11-15, of #62 with main merged, #73–#79) found two
high-severity issues: two admin rotations at once could still lose the only working password
(#75: a rotation in flight holds others off for 5 minutes, a promote that misses keeps whichever
URL logs in, PATCH waits for it, and a rebuilt URL must keep its login and host) and `roll run
--watch` spinning when a long poll failed fast (#79: a failure waits the interval; the server
holds at most 25 s and 32 polls per principal; a stopped watch abandons its poll). Also fixed:
one key's reveal in an untrusted preview could issue a dynamic credential (#62: the rule is the
resolver's now), signing secrets shown to a grant without `reveal` (#62, #77), a rotator moved
while its retired keys still wait to be revoked through it (#77). Merging #25 into #62 needed
`roll_auth::Grant`'s new `user` field in three places.

An eighteenth independent review (2026-11-15, of #62, #73–#76 and the last fixes against the
other threads' heads) found two high-severity issues, both fixed on their pull requests: an
interrupted admin rotation could lose the only working password (#75: whichever URL logs in is
kept, every change a compare-and-swap, an interrupted rotation finished on the next use), and code
nobody vetted could get a GitHub App token with the App's full power (#62: untrusted callers get no
dynamic credential at all; #73: a registration sets the most a token may reach). Also fixed: a
registration moved to another server with logins still on the old one, Redis' shared default user
rotated (#75), a reader renewing another's lease (#66), a failed product read moving every live
digest (#74), the 2-second sync pass scanning everything under steady writes (#76), restart
failures turning off the operator's cheap path (#69), and an early renewal failing a call while
the token still worked (#62). roll previews resolves untrusted previews without dynamic keys
again (#28), and roll apps keeps one `roll completions` command (alias `completion`). Merging
#74 with #76: `Secrets::source_changed` calls `syncs::due_for_source` in its write.

A seventeenth independent review (2026-11-15, of #62 and #63–#72 against the other threads' heads)
found four high-severity issues, each fixed on its pull request:
- roll apps deploying a fork's preview got its unprotected base's sensitive keys: an untrusted
  ephemeral environment kept the runtime flag (#62: it's read as a non-runtime by everyone, and
  `ForApps` reads one as previews do, leaving out dynamic keys and what doesn't resolve);
- a reveal token could end every lease in its environment (#66: leases record who they were
  issued to; a reader ends only its own);
- a 304 could hide a password its product changed itself (#65: never a 304 for a folder that reads
  a live link, until products report their own rotations);
- the Go and Python clients never logged a workload identity in again, so a watch failed after
  15 minutes (#62: they renew before the token ends and on a 401, reading the OIDC token afresh).

Also fixed: every preview converge writing an audit row (#62), the operator stopping at the first
failed restart and letting a RollSecret's author restart anything in its namespace (#69: opt-in
per workload, every target tried, failures retried), a write and an audit scan on every 304 (#65),
the CSI provider logging in on every mount (#63: a login reused per service account token), and
every node checking the same oldest syncs (#62: each claims its own). For the other threads: roll
MCP's secrets tool descriptions have drifted (seven database engines, rotation of any managed
link, health's newer finding kinds), and roll search should merge before #62, since the landing
branch's in-tree search issues admin keys until it echoes `access`.

A sixteenth independent review (2026-11-15, of #63–#72) found three high-severity bugs, all
fixed on their pull requests: a watch on a caching client delivering old values and stopping
(#65: watches read fresh, answers with problems aren't kept, and a 304 records one
`secrets.revalidate` row a day so health and insights still see the reads), the maker chain stopping
at tokens (#64: resolved through roll-access, and reach judges tokens as their requests are), and
Terraform refusing values roll normalises (#72, with encryption keys now protected from destroy). Also
fixed: restarts on unchanged data (#69), leases a read token couldn't end (#66), a decoy check
passed by throttling (#68), the CSI provider's API over plain HTTP or any host (#63), a signing
secret printed for SIEM sinks (#70) and system indices for search sinks (#71). Merging #70 and #71
together: both add `with_project` to rolld's `syncs.rs` (keep one) and change the stream form's
`urlOk` (keep #71's targets with #70's formats).

A fifteenth independent review (2026-11-14, against Doppler, Infisical, Vault, 1Password, AWS
Secrets Manager and Akeyless, and the other threads' heads) found two high-severity issues, both
fixed: roll apps showed sensitive values to whoever could reveal (`ForApps` never set
`sensitive`), and the operator logged every RollSecret in as itself, so creating one in a
namespace could reach the operator's identity or another team's (now a namespace's own service
account outside the operator's namespaces). Also fixed: client addresses taken from the
client-written `X-Forwarded-For` entry (and MCP's calls audited without one), live rotations
the digest didn't see, resolves that couldn't refuse dynamic credentials (previews), search
keys a build made admin, a sync sweep that could outlast its interval, failed pushes audited as
if nothing went out. The rest of its ranking is above; the other threads have its findings on
their side.

A fourteenth independent review (2026-11-14, of the landing merges) found no lost behaviour in the
store, the providers, the values route or the catalogue; it found where features meet: a workload
identity's `roll run --fallback --watch` started through an outage never logging in again (#35 with
#56: the login is now pending until rolld answers, and a failing or timed-out login counts as no
answer), masking concealing an env file's own plain values (#54 with #49: only values from roll),
a strict read failing on a required key that is sensitive or dynamic (#55 with #46), a reference to
a sensitive key reported as unset (#49 with #46), a reference's read with no answer not falling back
(#35 with #49), and docs and comments merged twice or misplaced: all fixed, tested where a test
guards behaviour.

The landing branch passes roll-secrets' tests (SQLite and Postgres), roll-cli's, rolld's auth and
secrets tests, roll-access's, clippy with `-D warnings`, `pnpm typecheck`, the dashboard's 719
tests, `@roll/secrets`' and the VS Code extension's.

The pull requests that followed #20 (history: by 2026-11-19 each one below is on main, closed, or
replaced; the open ones are in the ranked list and "State" above):

- **#63, a Secrets Store CSI Driver provider** (after #62): `sdk/csi-provider`, a pod's volume
  gets an environment's values as files with no Secret in etcd, each pod logged in with its own
  service account; the driver's gRPC served with Go's standard library.

- **#30, one permission system** (after #20 and #25): a project's tokens are roll-access grants,
  identities' logins get `workload` tokens, `rlst_` is gone.
- **#31, roll apps' deploys get their project's secrets** (after #20 and #26): `ForApps`, linked
  keys marked for apps to rewrite, `may_reveal` by secrets' own rules (`Secrets::caller_of`).
  #30 and #31 both change `caller()`: whichever lands second keeps #30's (no `rlst_`) with
  #31's `caller_of` split.
- **#32, Go and Python clients** (after #20): `sdk/go/secrets`, `sdk/python/roll_secrets`.
- **#33, a Terraform provider** (after #20): environments, keys (a value or a link), identities,
  syncs, tokens and values.
- **#34, a GitHub Action** (after #20): a job's environment through its own OIDC token, masked;
  `scan: true` fails a pull request that adds a stored value.
- **#35, `roll run --fallback`** (after #20): an encrypted copy of the last values, used through an
  outage.
- **#36, a Kubernetes operator** (after #32): RollSecrets pulled with the cluster's own identity,
  and its Helm chart.
- **#37, customer-managed keys** (after #20): a project's data key wrapped by the customer's KMS key,
  set from the project's settings.
- **#38, dynamic AWS credentials** (after #20): a session per lease from a role the project registers.
- **#40, AWS IAM users' access keys** (after #38): two key slots take turns, the old key deleted
  after its grace; a user with two keys already is refused, never pruned.
- **#55, pulling into Kubernetes with External Secrets Operator** (after #20): its webhook
  provider (one store for whole environments and single keys) and `values?dynamic=false`.
- **#61, reach** (after #20): who and what can read a folder or key; `GET …/reach`, `roll secrets
  reach`, a key's "Who can read it…".
- **#60, server credentials under browser-bundle names** (after #20): `NEXT_PUBLIC_`, `VITE_`,
  `PUBLIC_`… holding a server-only value; the write warns, health's `public` finding.
- **#59, `roll completions` and direnv** (after #20): completion scripts for every command; a
  documented `.envrc`.
- **#58, roll secrets for VS Code** (after #20): hovers, warnings and completion where code reads
  a variable; `.env` values replaced by `roll://` references or moved into roll first.
- **#57, workload identities pin their issuer's keys** (after #20): `jwks` on an identity, for a
  cluster's own issuer; `identities add --jwks`, the dashboard's Other OIDC form.
- **#56, a workload identity's login renews itself** (after #20): long-running `roll run
  --watch` in a pod logs in again before its login ends, reading the rotated token file.
- **#54, `roll run` masks values in the command's output** (after #20): stdout and stderr that
  aren't a terminal go through roll, which writes `<concealed by roll>` for each value it added
  (as is, by line, JSON- and URL-encoded); `--mask`, `--no-mask`.
- **#53, change policies per folder** (after #20): a folder of an unprotected environment takes
  changes only through approved requests, with its own approvals and approvers.
- **#52, moving in from Vault trees and AWS Secrets Manager** (after #20): `import --from vault
  --recursive` into folders, `--from aws` through the user's aws CLI.
- **#51, search links on OpenSearch clusters** (after #20): an internal user (`user`,
  `password`) where the cluster has no API keys; the wrong field for a flavor is refused when linked.
- **#50, check-and-set writes** (after #20): `if_version` on a set or delete, refused as `stale`
  when the key moved; the dashboard's editor sends it.
- **#49, `roll://` references in files** (after #20): `roll run --env-file` and `render` resolve
  `roll://ENV/KEY` beside plain values, as 1Password's `op://`.
- **#48, failing rotations in health** (after #20): `rotation_failing` and `revoke_failing`
  findings, with the reason, until roll succeeds; a rotations overview of every managed credential.
- **#47, encryption keys** (after #20): encryption as a service, as Vault's Transit: named,
  versioned keys that never leave roll; encrypt, decrypt (audited), rewrap, data keys, Ed25519
  signatures and HMACs; keys brought from outside, Vault's ciphertexts included.
- **#46, sensitive keys** (after #20): a key tagged `sensitive` reaches only roll's runtimes, syncs
  and workload identities, never a person or a token; the tag is for good.
- **#45, SSH certificates** (after #44): a project's SSH CA signs short-lived user certificates
  for the logins it allows, and `roll ssh` uses them for one session; OpenSSH's formats, checked
  with `ssh-keygen` and a real `sshd` login.
- **#44, a project's own certificate authority** (after #20): short-lived TLS certificates for the
  names its name-constrained CA allows, dynamic or managed, and CSRs it signs; revocation and a
  public CRL; checked with a real TLS handshake and `openssl verify -crl_check`.
- **#43, dynamic GCP and Azure credentials** (after #20): roll as an OIDC issuer
  (`/v1/secrets/oidc`); GCP's workload identity federation and Azure's federated credentials trust
  its token for one project, checked with a decoy project at registration.
- **#42, MongoDB outside roll** (after #41): rotation and a user per lease for replica sets (not
  Atlas, which refuses the user commands on every tier: "MongoDB Atlas" uses its Admin API); the primary only among the seeds, SRV targets inside their domain, SCRAM with the server's
  signature checked.
- **#41, Redis outside roll** (after #39): two ACL users take turns (`#<sha256>` passwords), or a
  user per lease copying a template user's rules; the tenant's replies read by a bounded parser.
- **#39, Postgres, MySQL, SQL Server, ClickHouse and RabbitMQ outside roll** (after #20): two login roles take turns (rotation), or a
  login per lease in a group role (dynamic), over verified TLS to a public address rolld resolved
  itself (Postgres gets a SCRAM verifier, never the password).

Done since the last review: roll search's merge (#22: its links checked live, the conflicts additive); signed-in people manage the secrets of their own projects (People,
under Access), with the canvas guards. main (roll MCP #19 and accounts #18) merged in. roll secrets asks the
caller's roll-access Grant (`GrantGrants`): a grant's request is checked inside the product too,
health and leaks filter themselves, search and findings take any grant, ephemeral environments are
roll-access's `preview` class, and a sync made by a grant pushes only while the grant may.
