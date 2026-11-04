# roll code agents — architecture

**Keep the code rolling.** roll code agents is an agent harness on a canvas. You run coding
agents (Codex, Claude Code, ACP agents such as Gemini, Grok or OpenCode) on your own machines
or in roll-hosted sandboxes, and drive all of them from one live canvas in the roll dashboard:
machines, projects, sessions, subagents, approvals and terminals are nodes on it, and everything
that is running pulses.

It works like T3 Code (MIT, github.com/pingdotgg/t3code): the same domain model (threads, runs,
an execution-node tree per run, runtime requests, checkpoints, worktrees, forks, provider
switching), the same providers driven through the same native protocols, and the user's own
provider logins. The differences are that every backend piece is Rust, machines connect *out* to
roll (no tunnel, port or Tailscale), and the UI is a canvas in the roll dashboard instead of a
sidebar app.

## Shape

```
 browser (dashboard /dashboard/agents)          roll agent (daemon, on each machine or sandbox)
   │  REST commands, SSE state stream             │  providers: codex app-server (JSON-RPC),
   │                                              │  claude (stream-json control protocol),
   ▼                                              │  ACP agents (JSON-RPC); worktrees, git
 code-control /v1/code/agents  ◄── WebSocket ─────┘  checkpoints, PTYs, SQLite entity store
   relay: machines, device login, entity mirror,
   owner-wide resumable stream, sandboxes, shares
```

- **`crates/agent-proto`**: the wire types (entities, commands, deltas, frames). No I/O.
  Shared by the daemon and the relay; the dashboard mirrors it in
  `apps/dashboard/src/lib/agents-api.ts`, and both sides test against the JSON fixtures in
  `crates/agent-proto/fixtures/` (regenerate with `ROLL_FIXTURES=write cargo test -p agent-proto`).
- **`crates/agent-host`**: the daemon library. The orchestrator, the SQLite entity store, the
  provider adapters (`provider/{codex,claude,acp,fake}.rs`), git worktrees and checkpoints,
  terminals, and the relay client.
- **`roll agent …`** in `crates/roll-cli`: `login`, `up`, `status`, `logout`.
- **`crates/code-control/src/agents/`**: the relay and the `/v1/code/agents` API, machines and
  machine tokens, device login, sandboxes and shares.
- **`apps/dashboard`**: the agents in the header of every page (running, waiting, "New agent"),
  opening as a popup over the page; the same surface fills the window at `/dashboard/agents`;
  public shares at `/agents/s/<id>` (`src/app/agents/s/[id]`, replayed by `lib/agents-replay.ts`).
- **`apps/desktop`**: the desktop app (Electron): roll in a main window and the canvas in its own
  Agents window, plus this machine's daemon, a tray and notifications (see "Desktop app").

## Why the daemon owns the truth

Like a T3 server, the daemon on each machine is the authority for its own state: it spawns the
providers, owns the files, and keeps an event-sourced SQLite store. The relay mirrors what
daemons send so browsers see the canvas instantly and still see the last known state of an
offline machine, but it never decides anything about a thread. Commands go to the daemon and
fail with `machine_offline` if it isn't connected.

## The domain model

Kept from T3 Code (docs/orchestration-v2 there), trimmed to what the canvas and the providers
need. Every entity is a full record keyed by its id; a change re-sends the whole record (an
**upsert**), never a patch. That makes mirroring, resuming and replay trivial.

```
Machine            a daemon (hostname, os, providers and their auth state, online)
  Project          a directory on that machine, optionally a roll code repo
    Thread         one agent conversation: provider, model, modes, worktree, lineage
      Run          one user turn (prompt → completion), with its checkpoint and usage
        Node       the execution tree of a run: assistant text, reasoning, plan, todos,
                   tool calls, subagents, approval and question requests
      Request      a pending approval or question, answered from the canvas
      Checkpoint   the worktree's state after a run (a hidden git commit)
  Terminal         a PTY on the machine
```

- **Lineage.** A thread may have a parent: `fork` (user forked at a run) or `subagent` (an agent
  delegated work through roll's orchestrator MCP). Provider-native subagents (Claude's Task tool,
  Codex's collab agents) are `subagent` nodes inside the parent's run tree, with their own child
  nodes. The canvas draws both.
- **Runs and steering.** A message to a running thread is queued (`message.send` with
  `mode: queue`) or steers the active run (`mode: steer`): providers that can steer natively get
  the input; others are interrupted and the run restarts with the new input appended. The queue
  drains only after a run completes: an interrupt (or a failure) holds it, as T3's Stop does.
- **Handoffs.** A provider that joins a conversation it hasn't seen (a provider switch, a fork, a
  rollback it can't do natively) gets a portable summary of the runs it missed in front of the
  next prompt (T3's context handoff). Switching back to a provider resumes its native thread and
  hands over only what happened since. Runs record what the user sent, never the summary.
- **Forks.** `thread.fork` copies the files of the fork point's checkpoint and starts nothing
  (T3's lazy fork). The fork's first session settles how it gets the parent's conversation: when
  it runs on the parent's provider, that provider forks natively, and the fork point (the end of
  the chosen run) is in its native conversation, the session starts as a native copy through
  that run (`SessionConfig.fork`; Codex `thread/fork` with `lastTurnId`, Claude
  `--resume --fork-session --resume-session-at`); otherwise, or if the provider refuses, it
  starts fresh with the portable summary. Fork points are the native turn refs adapters report
  per turn (`ProviderEvent::TurnRef`), kept in the parent's private state, so forking at an
  earlier run works after a restart too. A `system` node in the fork's first run says which way
  it went (T3's `resolved_native` / `resolved_portable`).
- **Runtime modes** (T3's): `full_access` (no approvals), `auto_edit` (edits in the workspace
  allowed, commands ask), `ask` (everything asks). **Interaction modes**: `default`, `plan`.
- **Checkpoints.** Before the first run the daemon records the base tree; after every run it
  snapshots the worktree into a commit under `refs/roll/checkpoints/<thread>/<ordinal>` using a
  private index (`GIT_INDEX_FILE`), never touching the user's index or HEAD. The diff of run N is
  checkpoint N-1 → N. `checkpoint.rollback` restores the tree of a checkpoint and marks later
  runs `rolled_back` (and rolls back the provider thread where the provider can). A project in a
  subdirectory of a repository owns only that directory: its diffs, `git.status` and rollbacks
  cover the files under it, and a rollback never touches a file outside it (the user's edits
  elsewhere in the repo stay as they are).
- **Native rollback** (T3's `CheckpointRollbackService`), in one of two ways a provider declares
  (`Provider::rollback_style`). Codex drops the turns in its live thread (`thread/rollback`) and
  keeps it that way. Claude's session file keeps every turn, so the conversation is resumed at
  the newest kept run's turn ref (`SessionConfig.resume_at`, `--resume --resume-session-at`).
  That works from the refs kept in the thread's private state, after a daemon restart too, and
  every session resumes there (the thread's `heads`, T3's `nativeConversationHeadRef`) until a
  turn goes past it. A kept run with no ref moves the cut back to an earlier one, and the runs
  after the cut are handed over at the next run. A native fork rolled back to before its own
  first run copies the parent's conversation at the fork point again (T3 resumes the copy at its
  fork-head message id instead, which it reads from Claude's transcript; copying again yields
  the same conversation without reading provider files). Only with no native way left (no ref,
  a portable fork, a provider without rollback, or a resume that fails) does the provider start
  fresh with a summary of what's left.
- **Worktrees.** A thread may run in its own git worktree
  (`<data>/worktrees/<project>/<thread>`, branch `roll/<slug>`), so parallel threads never touch
  each other's files.

## Sequences, sync and resume

- The daemon stamps every entity upsert with its own monotonically increasing `seq`. On connect
  the relay says the last `seq` it holds for the machine (`welcome.resume_from`) and the daemon
  replays everything after it. Replays are idempotent: the relay keeps the newest `seq` per
  entity.
- The relay stamps each mirrored upsert with an owner-wide `cursor` (one number across all of an
  owner's machines). Browsers load `GET …/state` (a snapshot plus its cursor) and then stream
  `GET …/events` (SSE, `id:` is the cursor, `Last-Event-ID` resumes). Nothing is lost on a
  reconnect, because a snapshot plus "everything with a newer cursor" is complete.
- The state is in two scopes so the canvas stays light: the **shell** (machines, projects,
  threads, runs, pending requests, terminals) is always sent; **detail** (nodes, checkpoints)
  only for the threads a client asks for (`?threads=`).
- **Deltas** are ephemeral and never stored: streaming assistant text, tool output and terminal
  bytes. They are forwarded live as SSE events without an `id:`. The durable node record catches
  up (the daemon re-upserts a streaming node at most every 500 ms and when it completes), so a
  client that missed deltas loses nothing but smoothness.

## The relay protocol (daemon ↔ relay)

One WebSocket per daemon, `GET /v1/code/agents/connect` with `Authorization: Bearer rlm_…`
(a machine token; only valid here). Text frames, one JSON object each
(`agent_proto::{DaemonFrame, RelayFrame}`).

```
daemon → relay   hello {protocol, info}                 first frame
relay  → daemon  welcome {machine, owner, resume_from}
daemon → relay   entities {items: [{seq, entity}]}     batched, ordered by seq
relay  → daemon  ack {seq}                              relay has persisted up to seq
relay  → daemon  notice {message}                       for the user (records over the quota);
                                                        `roll agent up` prints it
daemon → relay   delta {delta}                          ephemeral
relay  → daemon  command {id, by, command, grant?}      by: who sent it (owner / agent name);
                                                        grant: git access for this command
                                                        ("Agents in roll code")
daemon → relay   reply {id, result}                     exactly one per command id
relay ↔ daemon   ping / pong                            every 20 s; 60 s silence drops the link
```

**The mirror's quota.** The relay keeps at most 250,000 records and 512 MB of record JSON per
machine, and 2 GB over all of an owner's machines (`store::QUOTA`; counted from the records
again at every `hello`). A batch that would go past it is still acked, but its new and growing
records aren't kept (updates that don't grow and tombstones always are), and the link sends a
`notice` saying so, at most once a minute. The relay's copy of that machine is then incomplete
until threads are deleted and their records sent again; the daemon's own store is untouched.

A command id is the client's idempotency key, per sender (`by`: two senders may pick the same id
for different commands): a daemon that already ran a command id replies with the stored result
instead of running it again (T3's command receipts). The receipt is written as started before the
command runs, so a command a crash or restart cut short never runs twice: its id answers
`conflict` ("it may have partly run") from then on. A reply comes after
the records its command wrote, so a client that has the reply finds them in `/state`: the daemon
holds it until it has sent every record up to its newest `seq` as of the command's end, also while
a replay is still catching up. A frame the daemon can't read (a newer relay's frame type or
command, or garbage) is skipped and the link stays up; a command among them gets an `invalid`
reply. The relay does the same after `hello`: a daemon frame it can't read is skipped, a reply
among them gives its waiters an `internal` error at once (not the timeout), and an entities batch
keeps the records that parse. The daemon reconnects with jittered backoff (1 s doubling to 30 s). The relay's close codes say what to do:
4401 the token was revoked and 4400 the protocol isn't spoken (stop, with a clear error), 4409
another daemon took the machine's link over (stop), 1012 the owner's agents moved to another
cluster node (reconnect; docs/code/cluster.md, "An owner's agents").

## Providers

Each adapter implements `agent_host::provider::Provider` and turns the provider's native
protocol into `ProviderEvent`s (item started/updated/completed, text deltas, requests, turn
completed, native thread ref). The orchestrator owns ids and turns those events into entities.

- **Codex**: `codex app-server` over stdio JSON-RPC (`thread/start`, `thread/resume`,
  `thread/fork`, `turn/start`, `turn/interrupt`, `turn/steer`; approvals arrive as server
  requests `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, …).
- **Claude**: the `claude` CLI with `--input-format stream-json --output-format stream-json`
  and the SDK control protocol on stdio (`initialize`, `can_use_tool` requests answered with
  allow/deny, `interrupt`, `set_permission_mode`), the same protocol Anthropic's Agent SDKs
  speak. Subagents come from `parent_tool_use_id`; questions from `AskUserQuestion`.
- **ACP**: any Agent Client Protocol agent (`session/new`, `session/prompt`,
  `session/request_permission`, `session/update`), configured in the daemon's config
  (`acp::known_agents()` lists how Gemini CLI, OpenCode, Grok, Kiro, fx and others start).
  `initialize` offers ACP v2 with v1's fields alongside, as T3 does; resuming is `session/load`
  or `session/resume`. ACP has no runtime modes of its own: roll answers permission requests
  itself where the thread's mode allows (reads always, edits inside the workspace in
  `auto_edit`, everything in `full_access`) and asks otherwise. The agent may call roll's
  `fs/*` and `terminal/*`, confined to the thread's directory; writes and terminals need the
  mode or an approval of the same kind in that turn.

**Auth is the user's own.** The daemon runs the provider CLIs installed on the machine with the
logins already there, exactly as T3 Code and Conductor do; roll never reads, copies or relays a
provider credential. API keys and `ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL` can be set per provider
in the daemon config. A provider's status (installed, version, logged in) is reported in
`hello` and refreshed on `provider.refresh`; logging in happens in a terminal on the canvas.

## Delegation (roll's orchestrator MCP)

Agents delegate work to subagents that are ordinary roll threads, so they show up on the canvas
under their parent and can run on any provider the machine has. It is T3 Code's orchestrator MCP
server (`docs/orchestration-v2/orchestrator-mcp-server.md` there), ported in
`crates/agent-host/src/mcp/` (the server and the tools) and
`crates/agent-host/src/orchestrator/tasks.rs` (what the tools do).

- **Server.** `Host::open` starts one Streamable HTTP MCP server on the IPv4 loopback address
  and a free port (`POST /mcp`, JSON responses, no SSE stream). Every provider session gets it
  in `SessionConfig.mcp` as server `roll` with its thread's bearer token (`rlx_…`, memory only).
  The token never goes on a command line, which every user on the machine can read (`ps`):
  Claude Code gets its `--mcp-config` as a path to a 0600 file in a 0700 directory of its own
  under the temp directory, removed when the session closes.
  A token names one thread: it sees and controls only the tasks that thread delegated and their
  descendants (anything else is `task_not_found`), and it stops working when the thread is
  deleted.
- **Tools.** `delegate_task`, `task_status`, `task_wait`, `task_cancel`, `task_send` (a follow-up
  message to the subagent, `queue` or `steer`), `list_tasks` and `orchestrator_capabilities`.
  Refusals are tool results with `isError` and `{error: {code, message}}` (T3's codes:
  `parent_not_active`, `provider_unavailable`, `model_unavailable`,
  `runtime_mode_escalation_denied`, `interaction_mode_escalation_denied`, `task_not_found`, plus
  roll's `depth_exceeded` and `too_many_tasks`).
- **A task** is a child thread with lineage `subagent` (parent = the caller, root, `from_run` =
  the caller's running run) and a `subagent` node in that run with `child` set; the node's id is
  the task id. The child gets only the task as its first message (no history), the caller's
  project and checkout (or its own worktree from the caller's HEAD with `worktree: true`), and
  the caller's provider, model and modes unless the call names others. Modes may only narrow
  (`ask` < `auto_edit` < `full_access`, `plan` < `default`). Delegating needs a running turn.
- **The node follows the child**: `running`, `waiting` while the child waits on an approval or
  question, then `completed`/`failed`/`interrupted` with `result` = the child's final answer (the
  last top-level message of its newest run, or a failed run's error). Child approvals and
  questions are ordinary `Request`s on the child thread, answered on the canvas; they never
  surface on the parent.
- **Sync** (`mode: "sync"`) waits for the result, at most `timeout_ms` (10 min default, 60 max);
  a pending request in the child ends the wait (`pending_request`), a timeout returns
  `wait_timed_out` and leaves the task running, and the wait ends when the caller's run ends.
  **Async** (the default) returns at once. Each child run's result reaches the parent once: as
  the answer of a sync call, `task_wait` or `task_status`, or else as a wake (T3's completion
  delivery). A wake starts a run on an idle parent whose prompt is
  `Delegated task <id> (<title>) [<status>]:\n<result>`; on a busy parent it is a message queued
  behind the active run, and results that finish meanwhile join that message (16 KB of results
  inline, then a pointer to `task_status`). A result the parent reads first is taken out of the
  queued wake. A parent whose queue is held (it was stopped) keeps the wake queued.
- **Stopping.** `task_cancel` interrupts the child and everything it delegated; `run.interrupt`
  on a thread does the same for every task it delegated (T3's Stop). Cancelled tasks report
  `cancelled` and wake nobody; deleting a thread stops its tasks first.
- **Limits** (`[delegation]` in the daemon's `config.toml`): `max_depth` (3; a user's thread is
  depth 0) and `max_children` (6 unfinished tasks per thread). A provider must be installed and
  logged in, and a model must be one it lists.
- Tasks survive as records: the node and the child thread. Who is waiting and which results were
  delivered is memory only; a restart interrupts every run anyway.

## Machines, login and tokens

- `roll agent login` starts a device login: `POST /v1/code/agents/login` (with the machine's
  hostname and OS) returns a `device_code` and a short `user_code`; the user opens
  `/dashboard/agents/connect?code=…` (signed in), which looks the login up and asks "Approve
  ada-mbp (macOS)?"; the CLI's poll then receives a machine token `rlm_…` and stores it in
  `~/.config/roll/agent.json` (0600). Anyone may start a login, so at most 20 wait per client
  address (an IPv6 /64; 429 past that), and when 10,000 wait in all the oldest pending one of the
  address with the most makes room: a flood from one address, or many, never locks out a user
  whose login is the only one from theirs. The dashboard can also mint one directly ("Add machine"),
  which `roll agent login --machine-token rlm_…` (or `ROLL_AGENT_TOKEN`) saves the same way after
  asking roll which machine it is (`GET /v1/code/agents/connect` without an upgrade).
- Installing: `curl -fsSL https://roll.sh/install.sh | sh` (`apps/dashboard/public/install.sh`)
  installs the `roll` CLI from the GitHub releases of roll-sh/roll into `~/.local/bin`, checking
  its SHA-256. Releases are built by `.github/workflows/release-cli.yml` on a `cli-v*` tag (Linux
  and macOS, x86_64 and aarch64).
- `ROLL_AGENT_FAKE=1 roll agent up` is for tests only: the daemon offers the scripted providers
  of `agent_host::provider::fake` instead of the real ones (`pnpm test:agents`).
- Machine tokens are stored as sha256 only, looked up on every connect (never cached), and only
  open `/v1/code/agents/connect` (`GET` the link; `DELETE` revokes the machine itself, for
  `roll agent logout`). Revoking a machine closes its link at once.
- Commands need `Code::owns` on the owner, and an agent token (`agent:<name>`) only drives
  threads, so agents can orchestrate agents without being able to reach past a thread's work
  (`code-control/src/agents/policy.rs`, one function for the routes and the MCP tools that
  replay them):
  - Agents may start threads (`ask` or `auto_edit` only, never `full_access`), configure them
    (modes only go down: `ask` < `auto_edit` < `full_access`, `plan` < `default`, against the
    thread as the relay mirrors it), fork, message, interrupt, archive and publish them, and read
    their state, diffs, status and files inside a project.
  - Terminals, files outside a project (`fs.list` without one), answering a thread's approvals
    and questions (an agent could approve itself), adding or removing projects (also `POST
    …/projects`), deleting threads, rollbacks and `provider.refresh` need `Code::manages`, as
    do adding, renaming or revoking machines, sandboxes and shares.
  - Why the relay and not the daemon: agent tokens are the relay's idea (the daemon sees only a
    `by`), and the relay is the one place every command passes. Delegated subagents (the
    daemon's orchestrator MCP) already only narrow modes.

## Shares

The owner can share a thread as a read-only link, `https://roll.sh/agents/s/<id>`: "the agent IDE
people will share". Sharing freezes a copy of the thread's records from the relay's mirror (and,
optionally, of the subagent threads it delegated to), so the link shows the run as it was, not
what happens after. Terminals, attachments' bytes, queued messages and a project's repo and git
state (`Project.repo`, `Project.git`: a private repo's name, a remote's credentials) are left
out. In every string the machine's home directory (from its `hello`), and anything that looks
like `/home/<user>`, `/Users/<user>` or `/root`, is written `~` wherever it appears, and
credentials that look like what they are become `<prefix>[redacted]`: roll's `rlm_`, `rlg_`,
`rlc_`, `rlx_`, `rla_` and `rld_` tokens, GitHub's (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`,
`github_pat_`), `sk-` keys, Stripe's `sk_live_`/`sk_test_`/`rk_live_`, Slack's `xox?-`, AWS
`AKIA`/`ASIA` key ids, whatever follows `Bearer` or `Basic`, and a URL's userinfo
(`share.rs`, `secrets` and `tilde`). It is a best effort for what a share shows, not a promise
that no secret can be in it: the owner chooses what to share. The page is the canvas in read-only mode with **replay**: a timeline
rebuilt from the records' timestamps (threads appear, nodes start, text streams, the accent pulse
while a run is going), a play button and speed, and the final frame at once with reduced motion.
Revoking a share removes it; nothing else does. Routes are in docs/code/http-api.md ("Shares").

## Agents in roll code

The canvas and the code live in the same place: a project can be a checkout of one of the
owner's roll code repos, a thread's work is published as a roll code change, and what happens to
that change (checks, reviews, the merge queue, landing) shows on the thread. The relay side is
`crates/code-control/src/agents/code.rs`.

- **Machine identity for code: per-command grants.** A machine token (`rlm_…`) only opens the
  relay link; it never opens git or the API. When a command needs a repo (`project.add` with
  `clone`, `thread.publish` in a roll code project), the relay mints a **grant** and sends it in
  that command's frame (`RelayFrame::Command.grant`, `agent_proto::CodeGrant`): a token
  (`rlg_…`, kept as sha256 in `agent_grants`) for that one repository and that machine, valid ten
  minutes. It opens git smart HTTP for that repo only, acts as `agent:<machine name>` (the pusher
  of the push), and is refused by every API route (401), even
  on an open daemon, like review app tokens. It is looked up on every git request (never cached),
  so revoking the machine ends it at once. The daemon passes it to git in the environment of that
  one process (`GIT_CONFIG_*` `http.extraHeader`), never in a remote URL, a config file or the
  command line, and forgets it after. That process runs no hooks (`core.hooksPath=/dev/null`: not
  the repo's, the user's or a clone template's) and no fsmonitor, so nothing it would start can
  read the grant.
  - What it may push is one ref. A clone's grant only fetches. A publish's grant pushes
    `refs/heads/<the thread's worktree branch>` (as the relay mirrors the thread), create or
    update, nothing else: not the default branch, another branch or a tag, and never a delete.
    That one ref is all roll code needs: pushing a branch opens or revises its change. The git
    adapter checks every ref of a push against the grant (`git.rs`, `receive`; an atomic push
    with a ref outside it pushes nothing). A thread without a worktree gets no grant, and one
    whose branch is the repo's default branch is refused (400).
  - The repo must be the machine's owner's. A grant never exceeds the command's sender: a clone
    needs read access, a publish needs `check_write` (`owns`), and nothing in it ever needs
    `manages`. A machine of an owner can't reach another owner's repos, public or not.
  - Why not let machine tokens open git as `agent:<machine name>` on all the owner's repos? A
    machine token is long-lived and sits on disk on a laptop; a grant is scoped to the repo the
    user just asked about, lives minutes, and is only in memory. The relay already sees every
    command, so it is the one place that knows which repo is needed and who asked.
- **Cloning.** `project.add {path, clone: {owner, repo}}` clones into `path` (absolute), into
  `<clone dir>/<owner>/<repo>` when `path` is empty, or under the clone dir when it is relative
  (`..` refused). The clone dir is `[code] clone_dir` in the daemon's config, else `~/roll`. An
  existing complete checkout of the same repo at the path is kept; any other non-empty directory
  is refused. The clone is made in a temporary directory next to the path and moved into place
  once complete, so a failed or killed clone leaves nothing there. It goes on as long as git
  reports progress (two minutes without any give up). The project records `repo`. The command's
  30 s reply wait may run out on a big clone; the daemon finishes anyway and a retry with the
  same id gets its receipt. A clone or push that fails on the way (the network, the server, no
  progress) is `internal` and leaves no receipt, so a retry with the same id runs again; a
  refusal (the path is taken, no grant) is that id's answer for good.
- Projects report `git.remote` without credentials, a query or a fragment
  (`https://ada:ghp_…@github.com/ada/app.git` is `https://github.com/ada/app.git`).
- **Starting a repo from the canvas.** `POST /v1/code/agents/{owner}/machines/{machine}/projects
  {id, repo: {name, description?, visibility?}, path?, name?}` creates the repo through
  `POST /v1/code/repos` on the caller's behalf (a 409 means a retry, or a repo of that name: it is
  cloned as it is), lands a first commit (a README) when the repo has no history, so worktrees and
  publishing work at once, and sends `project.add` with `clone`, answering its `Reply`. MCP:
  `create_agent_project`.
- **Publishing.** `thread.publish` commits the thread's worktree on its branch. In a roll code
  project the daemon pushes the branch to the grant's URL: roll code's git adapter opens the
  branch's change (or revises it), exactly as for any `git push`. The relay then reads that change
  (`change_by_branch`) and fills `Published.change` and `url`. With `land: true` the relay calls
  `POST /v1/code/changes/{id}/land` with the sender's own token: on a `queue` repo that enqueues it
  (the queue lands it once tested), otherwise it lands through `Code::blockers`. Nothing goes
  around the rules; the answer is `Published.landing {status: landed | queued | blocked |
  conflict | failed, message, blockers}`, a result, never an error. A project that isn't a roll
  code repo pushes to `origin` as before (and refuses `land`).
- **The change on the thread.** `Thread.change {repo, number, status, url, detail}` and
  `Thread.work {id, repo, number, title}` are the relay's, not the daemon's: they are kept in
  `agent_links` and merged over every thread record the daemon sends (daemons leave them unset),
  so the canvas sees roll code's view even while the machine is offline. `status` is one of
  `checking`, `failing`, `blocked`, `ready`, `queued`, `landed`, `conflict`, `closed`.
  - **Following.** The node serving an owner's agents re-reads every unsettled linked change from
    the changes table, the durable record that checks, statuses, reviews, the merge queue and
    landings all write: every second, sooner when the event feed mentions a change, check, queue,
    review or landing (only a hint; a dropped event loses nothing), and before every `/state`
    snapshot. A changed status re-stamps the thread record under a new cursor, so it reaches
    streams like any other change. Landed and closed changes stop being followed.
- **Work items.** `POST /v1/code/agents/{owner}/threads/{thread}/work {work? | repo, labels?,
  lease_s?, agent?}`: the thread takes `work` (`wi_…` or `owner/repo#N`, read through the API),
  or claims the next open item of `repo` through `POST …/work/claim` (as `agent`, default the
  caller's agent or `agent:<machine name>`); 204 when there's none. The item must be in one of the
  owner's repos. `DELETE` lets go of it (the claim stays the claimant's to release). MCP:
  `claim_agent_work`.
  - **The claim stays alive while the thread works.** The node serving the owner's agents renews
    the claim the item had when the thread took it (by its lease length, once half of it is left)
    while the thread works: busy, or idle for under ten minutes by the relay's clock, not
    archived, and its machine connected. A renewal only extends a claim its claimant still holds
    (one conditional update), so it never revives a lapsed, released or taken-over claim. When
    the thread stops working the lease runs out like any other, and the item goes back to `open`.
  - **Publishing does the item.** `thread.publish` links the branch's change to the thread's item
    (`PATCH /v1/code/changes/{id} {work_item}` as the sender), as if the change had been opened
    with it: the item moves to `review`, and landing the change marks it `done`.

## Sandboxes

A sandbox is a machine roll hosts: a container running `roll agent up` with a machine token, so it
appears on the canvas like any other machine (`Machine.sandbox`: backend, size, status, price). It's
the product for people who don't want to connect their own machine: 3¢ per running hour for an
always-on 2 vCPU / 4 GB (`standard`; `large` 6¢, `xlarge` 10¢; sizes are priced in config).
Prices are integer US cents per running hour (`hourly_cents`), and that is what's charged. Code: `code-control/src/agents/sandbox/`;
routes: docs/code/http-api.md, "Sandboxes".

- **Backends** implement `SandboxBackend` (create, find by roll's id, status, start, stop, delete;
  all idempotent):
  - **agent37** (the hosted product): agent37 instances (gVisor) on their Sandbox API, created from a
    workspace template made from our image (`ROLL_AGENT37_TEMPLATE`), `auto_sleep` off, the env in
    the instance's write-only `env`, `metadata.roll_sandbox` = roll's id. The key
    (`ROLL_AGENT37_API_KEY`) is sent only to agent37 and never logged or returned. Every call has
    timeouts, follows no redirects and reads a bounded body. Tests use a mock agent37. Finding
    roll's instances reads `GET /v1/instances` once: it has no pagination or filters and returns
    every instance of the workspace, and a workspace holds at most 200 (agent37's instance
    limits), so one read (capped at 16 MiB) is complete.
  - **docker** (self-hosted and development): the `docker` CLI; no new privileges, every capability
    dropped, memory (no swap), CPU and process limits, an init, a named volume for `/home/roll`, no
    Docker socket, `runsc` (gVisor) when Docker has it (`ROLL_SANDBOX_RUNTIME`). The env goes in
    through a 0600 env file removed after `docker run`.
    - Network: a bridge of their own, `roll-sandboxes`, made on first use with traffic between
      its containers off (`enable_icc=false`), so sandboxes can't reach each other; or the
      operator's `ROLL_SANDBOX_DOCKER_NETWORK` (never `host` or another container's).
      `host.docker.internal` only with `ROLL_SANDBOX_DOCKER_HOST_GATEWAY=1`. A bridge still routes
      to the host's addresses and to cloud metadata (`169.254.169.254`), and only a firewall rule
      stops that; roll never changes the host's firewall, so the operator adds it, e.g.
      `iptables -I DOCKER-USER -s <the network's subnet> -d 169.254.169.254 -j DROP` and the same
      for the host's own addresses (allow the roll API's port first if it runs on that host).
    - Disk: Docker's local volume driver can't cap the home volume, so `disk_gb` isn't enforced
      there. `ROLL_SANDBOX_DOCKER_DISK_LIMIT=1` caps each container's writable layer
      (`--storage-opt size=<disk_gb>G`) on storage drivers that support it (overlay2 on xfs with
      `pquota`, btrfs, zfs). agent37 enforces the size's disk itself.
- **The image** is `deploy/agent/Dockerfile`: the non-root user `roll`, the `roll` CLI built from
  this repository, git, node, ripgrep, and the codex and claude CLIs at pinned versions,
  `IS_SANDBOX=1`. Its command is `roll agent up`, which, with no login file, takes the machine token
  from `ROLL_AGENT_TOKEN` (and `ROLL_API_URL`, `ROLL_AGENT_MACHINE`, `ROLL_AGENT_OWNER`,
  `ROLL_AGENT_NAME`) and starts again without those variables, so neither the providers and
  terminals it starts nor its own `/proc/<pid>/environ` (which unsetting a variable doesn't change,
  and which the user's other processes can read) hold the token: it writes what it took to a pipe,
  execs itself (same pid) with `ROLL_AGENT_LOGIN_FD` naming the pipe, and reads the login back
  from it. The token is never written to disk. With a login file the file wins, and the variables
  are dropped the same way. (Providers and terminals run as the same user, so a sandbox is one
  trust domain: the token only opens that sandbox's own link.)
- **Creating** makes the machine and its token at once and records the sandbox as `creating`; the
  token and the owner's env (provider keys such as `ANTHROPIC_API_KEY`) are sealed with
  `ROLL_SECRET_KEY`, bound to the sandbox, until the backend has them, then dropped. They go to the
  backend and nowhere else.
- **The reconciler** (one per owner, on the node holding `agents:<owner id>`; kicked by the
  owner's requests, started by taking the lease, on a single node by `Code::start_sandboxes`)
  looks every 30 s: it creates what isn't created yet (retrying capacity errors, adopting a create
  whose answer was lost by roll's id, failing one the backend refuses or that takes over 15
  minutes), starts an always-on sandbox that stopped on its own, records the backend's status into
  the sandbox and its machine (a `machine` event when it changes), and deletes. A sandbox's machine
  being removed deletes the sandbox; deleting a sandbox revokes its machine once the backend has
  deleted it. The lease isn't let go as idle while the owner has sandboxes.
- **The sweep** (`Code::sweep_sandboxes`, every 10 minutes on the node holding the
  `sandbox-sweep` singleton) lists what the backend holds tagged with roll's ids and deletes
  strays, so a create whose answer was lost never leaves a paid instance running with a token in
  it: one whose sandbox was deleted (a delete that came before the create showed up; its machine,
  and so its token, was revoked then), one whose sandbox failed before it was made, and a second
  one of a sandbox that has its instance. One tagged with a sandbox this server doesn't know is
  logged and left alone: another deployment may share the account (roll never removes sandbox
  rows, so its own are always known).
- **Metering**: at each look, running time is added to `agent_sandbox_usage` per sandbox and UTC
  month (split at month boundaries), with the size's hourly price at creation; a row costs
  hours × `hourly_cents` / 100 dollars. `GET …/sandboxes/usage` shows it; there's no payment processor.
- **Limits** (config): sandboxes per owner (`ROLL_SANDBOX_MAX_PER_OWNER`, 5), sizes
  (`ROLL_SANDBOX_SIZES`), the URL sandboxes connect to (`ROLL_SANDBOX_API_URL`, default the API URL).

**Logging providers in.** The owner logs the providers in from a terminal on the canvas
(`terminal.open` with `command: "claude /login"` or `"codex login --device-auth"`): both are
browserless flows that print a URL and take a code, and the login stays in the sandbox's home
volume. Provider API keys through the sandbox's env are the alternative. roll never reads, copies or
pools these credentials. A subscription login on a machine roll hosts is still the owner's own login
on a machine they rent, but it isn't their own computer: whether Anthropic's (and OpenAI's)
consumer terms allow a subscription to be used from a hosted sandbox is an open policy question to
settle with them before this is marketed; API keys have no such question.

## Canvas

Agents have no page in the dashboard nav. The header of every page shows how many are running
and waiting, and "New agent"; either opens the agents as a popup over the page (⌘J toggles it;
`#agents`, `#agents/new` and `#agents/<thread>` open it from a link, and the popup keeps the URL
in step). In the desktop app's main window the same control opens the Agents window instead,
which shows `/dashboard/agents`: the same surface, filling the window. `lib/agents-surface.ts`
holds what's open, so the header, the popup, the window and deep links agree.

The surface has three views:

- **The canvas.** Machines are anchors; projects hang off machines; threads off projects; forks
  and delegated subagents off their parent thread; provider-native subagents and pending requests
  are satellites of their thread. Edges carry a calm accent pulse while work flows through them
  (`--accent`, 1.6 s loop, off with reduced motion). Cards stay small: a thread's card is its
  status, title, what it's doing and what it cost; actions on machine and project cards show on
  hover (always on touch screens). Approvals are answered right on their card; questions say what
  they ask and open the conversation. Positions are laid out automatically and remembered when
  dragged. One canvas engine (canvas 2D + `requestAnimationFrame`, paused offscreen) draws edges,
  pulses and the dot field; cards are DOM, positioned by the same world transform. Navigation is
  Figma's: scrolling pans (two fingers on a trackpad, any direction; shift turns a mouse wheel
  sideways), pinching zooms about the fingers (ctrl+wheel in Chromium, Firefox and Electron,
  gesture events in Safari, two touches on a touch screen), ⌘/ctrl+wheel zooms with a mouse,
  dragging the background or space-drag pans, and + − 0 1 and the arrows work from the keyboard.
- **A conversation.** Clicking a thread opens it on its own, filling the surface: the transcript
  in a centred column, the composer under it, the diff (of the thread or one run) or the thread's
  terminals beside it. Everything is managed from there: approvals in the transcript, fork, roll
  back, publish, share, rename, archive, delete, take a work item. The title is a switcher to the
  other conversations (the ones waiting on you first), so there's no need to go back to the
  canvas; Escape (or ←) goes back. The canvas stays mounted underneath, so going back is instant.
- **A new agent.** Pick the machine and project (the last used by default; adding a project is
  right there), say what to do, and it opens as a conversation. Recent conversations are listed
  under the composer.

- **Getting started.** An owner with no machines sees the globe (`RollMark`) and the two ways in
  side by side, the same as "Add machine" on the canvas: connect your computer (install,
  `roll agent login`, `roll agent up`) or rent a sandbox (sizes and prices from
  `GET …/sandboxes`, optional provider keys). A new sandbox is on the canvas at once and pulses
  while it boots. A sandbox's card shows its size, status, price and this month's usage, with
  start, stop and delete, and puts logging a provider in first (a terminal on the canvas running
  the device login). A machine with no projects offers adding one (a folder, a clone of one of the
  owner's roll code repos, or a new repo); a project with no threads offers starting one. Offline
  machines are dimmed with when they were last seen.
- **roll code.** Thread cards and sessions show `Thread.change` (a chip linking to the change)
  and `Thread.work`. Publishing shows what landing did as a result, never an error; a thread
  outside a worktree is offered a fork into one first, and new threads in a roll code project
  start in a worktree.
- **Phones.** Below 640 px the popup is the whole screen; a conversation's actions fold into its
  menu and the diff or a terminal takes the transcript's place; the oldest waiting request sits at
  the bottom of the canvas, answerable with one tap; fitting the canvas keeps cards readable (at
  least 55 %) and starts at the machines.

## Desktop app

`apps/desktop` (`@roll/desktop`) is a thin Electron shell, like T3 Code's desktop app around its
web app: the backend stays the Rust daemon, and the UI stays the dashboard. It does three things:

- **Two windows.** The main window shows roll: it loads `https://roll.sh/dashboard` (the `url`
  in `settings.json` in the app's data directory, or `ROLL_DESKTOP_URL` for development against
  a local dashboard). The Agents window shows the canvas at `<origin>/dashboard/agents`. There
  is one of it, opened from the dashboard header's agents control (`openAgents`), `roll://`
  links, notifications, the tray and the menu ("Open agents", "Open roll"). The dock icon shows
  the main window. Closing the Agents window only closes it; the agents keep running. Closing
  the main window keeps roll in the tray. On macOS the Agents window has no title bar (inset
  traffic lights at 16,16), so the canvas draws its own 48 px bar. The page pads it and makes it
  a drag region when `rollDesktop.window` is `"agents"`. Each window remembers its own bounds. In
  both windows a trackpad pinch reaches the page as ctrl+wheel, so the canvas zooms itself and
  the page doesn't. Both renderers are sandboxed and context-isolated, with no Node and no
  webviews. Only the app's origin (plus `extraOrigins`, for a sign-in provider) loads in them.
  Other web links open in the system browser, new windows are never opened by a page, and every
  web permission is denied except notifications, clipboard writes and fullscreen for the app's
  own top frame.
- **This machine.** It ships the `roll` CLI (`<resources>/bin/roll`; in development
  `target/release/roll` or `ROLL_BIN`) and supervises `roll agent up`, which takes the API and
  token from `agent.json`. If the daemon exits, the app restarts it after 1 s, doubling up to
  30 s, and the delay resets after a minute of uptime. Output goes to `agent.log` in the app's
  logs directory, rotated at 10 MiB. On quit the app sends SIGTERM and then SIGKILL after 5 s. The
  child gets the login shell's PATH, so `codex`, `claude` and `git` are found when the app is
  started from a launcher.
  **First run:** with no `agent.json` (where the CLI keeps it: `ROLL_AGENT_CONFIG`, else
  `~/.config/roll/agent.json`), the app runs `roll agent login --json --api <api>`. It reads the
  `verification_uri` (`/dashboard/agents/connect?code=…`) from the output and opens it in the
  main window, where the user is already signed in, so approving is one click. When the CLI exits
  with the token saved, the daemon starts. The API is `api` in settings (`ROLL_DESKTOP_API`).
  Unset, it is api.roll.sh for roll.sh and the web origin anywhere else. If `agent.json`
  disappears (`roll agent logout`, a revoked machine), the app stops retrying and offers
  "Connect this machine…" again. "Pause agents" stops the daemon and stays paused across
  restarts. With `daemon: false` in settings, the app leaves the daemon to something else.
- **Tray, badge and notifications.** The tray shows the machine's state: connected, connecting,
  offline and reconnecting, "offline · retrying in N s" while the daemon is down, paused, not
  signed in, or waiting for approval (with the code). The daemon's own "connected to roll as …"
  and "…; retrying in N s" lines tell it whether the link is up. The tray also shows how many
  agents are running and how many requests are waiting, with "Open agents", "Open roll", "Pause
  agents" and "Quit". The dock or taskbar badge counts the waiting requests. Each new approval or
  question posts one native notification, except while the Agents window is focused. Requests
  already waiting at launch don't notify. Clicking a notification opens the thread in the Agents
  window.

**The bridge.** The preload exposes `window.rollDesktop` in both windows. The main process only
accepts its messages from the app's own top frame in one of its windows, so the bridge does
nothing on any other origin. The page reports its state (from either window; the latest wins);
the shell never talks to the relay itself:

```ts
rollDesktop.window                    // "main" | "agents": the window this page is in
rollDesktop.setStatus({ running, pending: [{ thread, title, request, kind, summary? }], machines? })
rollDesktop.notify({ title, body?, thread? })
rollDesktop.onDeepLink((link) => …)   // { kind: "agents" | "thread" | "new" | "connect", … }; returns unsubscribe
rollDesktop.openAgents(target)        // { kind: "canvas" } | { kind: "thread", thread } | { kind: "new" }
```

`apps/dashboard/src/lib/desktop.ts` has the same types (a test keeps them identical). Its
`useDesktop(data, focus)` is the canvas's one call: it sends the status from the agents store
whenever the status changes, and opens thread links with `focus("t:<id>")`. Its
`openAgentsWindow(target)` opens the Agents window from the main window and returns true; in a
browser, in the Agents window or in an older app it returns false, so the header control shows
its in-page popup instead. In a browser every helper does nothing.

**Deep links.** `roll://agents`, `roll://agents/thread/<id>`, `roll://agents/new` and
`roll://agents/connect?code=<user code>` are registered at install (macOS `Info.plist`, Linux
`x-scheme-handler/roll`, Windows NSIS) and at runtime. They reach the running instance (one
instance at a time). Agents, thread and new links (and `openAgents`) go to the Agents window. If
it is closed, it opens on `/dashboard/agents`, `?focus=t:<id>` or `?new`. If it is open, it comes
to the front and its page gets the link through `onDeepLink` without a reload. While the page is
still loading, the link waits until the page listens. A page that never listens (one that isn't
the canvas) gets the link's URL loaded 3 s after it finishes loading. Connect links load the
connect page in the main window.

**Packaging.** electron-builder (`apps/desktop/electron-builder.yml`) builds a dmg and zip for
macOS, NSIS for Windows, and an AppImage and deb for Linux, with the CLI as an extra resource.
`pnpm --filter @roll/desktop build` bundles main and preload with esbuild and renders the icons
from `@roll/brand` (`horizonSvg`) to PNG, ICNS and ICO. `pnpm --filter @roll/desktop dist`
packages for the host platform; it stages `ROLL_BIN` or `target/release/roll` and fails without
one. Auto-update (electron-updater, generic feed) is off unless `autoUpdate.enabled` and
`autoUpdate.url` are set. There is no update server yet.
