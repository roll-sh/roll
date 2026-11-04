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
  Project          a directory on that machine, optionally a roll code repo; its settings
                   (auto-pull, new-thread defaults, scripts) and last pull
    Thread         one agent conversation: provider, model, modes, worktree, lineage,
                   its context window
      Run          one user turn (prompt → completion), with its checkpoint, usage and
                   the context window it left
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
- **Context window** (T3's `ThreadTokenUsageSnapshot`, shown like its `ContextWindowMeter`).
  `Thread.context` and `Run.context` are a `ContextUsage`: `used_tokens` (what the next request
  carries), `max_tokens` (the model's window, unset when unknown: the dashboard then shows tokens
  and never a percentage) and `total_processed_tokens` where the provider reports it. Adapters
  send `ProviderEvent::Context` as the provider reports (each replaces the last); the run keeps
  the newest one of its own, the thread the newest one of its conversation. After a compaction
  `used_tokens` is what's left, so it drops. The thread's context belongs to its provider's
  native conversation (T3 keeps it on the provider thread): a provider switch shows the new
  provider's newest kept run's (none if that provider has no conversation yet), a rollback the
  newest kept run's, and a fork its parent's at the fork point until its first session, which
  keeps it when the fork is native and clears it when it starts fresh with a summary.
- **Runtime modes** (T3's): `full_access` (no approvals), `auto_edit` (edits in the workspace
  allowed, commands ask), `ask` (everything asks). New threads get `full_access` unless their
  project's defaults say otherwise (T3's default; `RuntimeMode::default()`, the composer's and the
  demo's). **Interaction modes**: `default`, `plan`.
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
- **Requests across a restart** (a sandbox going to sleep with an approval pending, `docker stop`,
  a crash). A run that was only waiting on its user when the daemon stopped is *parked*: the next
  start keeps it `waiting`, with its requests pending and its queue (the thread's `queued`) intact;
  any other unfinished run is `interrupted` ("the daemon restarted") and its requests `cancelled`.
  The provider turn that asked is gone, so answering a parked request resolves it and sends the
  answer to the agent as a new turn on its resumed native conversation (sessions resume by native
  id), in the same run: the decision or the answers as a message ("You asked for approval: …; I
  approved it. … Go ahead and do it now."), with a `system` node in the run saying the machine
  stopped meanwhile. The turn's other pending requests are withdrawn (the agent asks again if it
  still needs them); an accepted approval also answers the resumed turn's first ask for the same
  thing (the same action, command, tool and files) at once, so the user isn't asked twice. A
  conversation that can't be resumed starts fresh, with the thread handed over and the run's own
  prompt in front of the answer. `cancel` ends the run, steering a parked run goes on with the
  steering input the same way, and an interrupt ends it. T3 Code does this for questions after a
  restart (`ProviderRuntimeRecoveryService.ts`: a question becomes answerable by message, delivered
  as a user message on the provider thread) and expires approvals there (`not_resumable`); roll
  answers approvals the same way as questions, because a sandbox sleeps with approvals pending as a
  matter of course.

## Sequences, sync and resume

- The daemon stamps every entity upsert with its own monotonically increasing `seq`. On connect
  the relay says the last `seq` it holds for the machine (`welcome.resume_from`) and the daemon
  replays everything after it. Replays are idempotent: the relay keeps the newest `seq` per
  entity.
- The relay stamps each mirrored upsert with an owner-wide `cursor` (one number across all of an
  owner's machines). Browsers load `GET …/state` (a snapshot plus its cursor) and then stream
  `GET …/events` (SSE, `id:` is the cursor, `Last-Event-ID` resumes). Nothing is lost on a
  reconnect, because a snapshot plus "everything with a newer cursor" is complete.
- A save of the owner's canvas arrangement (`…/canvas`, "Canvas") takes a cursor of the same
  sequence, in the same transaction, so the stream carries it as `canvas` and a reconnect that
  missed it gets the canvas's last save from the table.
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
keeps the records that parse. The daemon reconnects with jittered backoff (1 s doubling to 30 s). A daemon that is stopping
(SIGTERM: a sandbox going to sleep, `docker stop`) sends the records its shutdown wrote, runs no new
command (they stay waiting on the relay, which sends them again on the next link), and closes the
link once the relay has acknowledged every record up to its newest `seq` (3 s at most). The relay's close codes say what to do:
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

**Context usage** comes from each protocol's own report, as in T3's adapters: Codex's
`thread/tokenUsage/updated` (`last.totalTokens` against `modelContextWindow`, `total.totalTokens`
processed); Claude's main-agent message usage (input, cache writes and reads, plus output, from
`message_start`, `message_delta` and the assistant snapshot; `<synthetic>` messages and reports
without input are skipped), with the window from T3's model manifest (`claude/models.rs`) and
`compact_metadata.post_tokens` after a compaction; ACP's `usage_update` (`used`, and `size`
unless it is 0).

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
- **Instructions** (T3's `orchestrationInstructions.ts`; `orchestrator/instructions.rs`). Every
  session is told how delegation works (Claude: `--append-system-prompt`; Codex:
  `additionalContext`; ACP: in front of its first prompt): when to use `delegate_task` rather than
  its own subagent tool, `orchestrator_capabilities` first, end the turn instead of polling. A
  delegated thread is also told it is a subagent: do the work itself and report back. The text only
  depends on what the thread is, so it never breaks a provider's prompt cache.
- **Orchestrators** ("Orchestrate" in the command palette and the canvas's prompt bar).
  `thread.create {orchestrator: true}` starts a thread whose prompt is a goal for agents
  (`Thread.orchestrator`; a fork of one is one too). Its sessions are told to look at the project,
  plan, tell the user the plan, hand each part to a subagent (`worktree: true` for parts that edit
  files), end the turn, check each result as it wakes it, and sum up at the end. Nothing else
  differs: it is an ordinary thread with the same MCP, limits and modes, its run records only the
  user's goal, and its tasks are ordinary subagents on the canvas under it. T3 has no such role
  (its threads delegate when the user asks them to); roll adds it so a goal typed outside any
  conversation becomes agents on the canvas.

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
    and questions (an agent could approve itself), adding, changing (`project.update`,
    `project.pull`) or removing projects (also `POST …/projects`), deleting threads, rollbacks and
    `provider.refresh` need `Code::manages`, as do adding, renaming or revoking machines,
    sandboxes and shares.
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

## Project settings

A project keeps settings of its own (`Project.settings`), T3 Code's per-project settings
(`packages/contracts/src/settings.ts`, `ProjectStore.ts`, migration
`045_ProjectionProjectsAutoPull.ts` there). `project.update {project, name?, settings?}` renames
it or replaces its settings whole, and replies once the record is stored. Code:
`agent-host/src/orchestrator/projects.rs`, `git.rs` (`fast_forward`),
`code-control/src/agents/pull.rs`; the panel is `components/agents/project-settings.tsx`.

- **Auto-pull** (`auto_pull`, off by default as in T3): keep the checkout's default branch current
  when a change lands or a PR merges. It follows T3's `maybeAutoPull` (`VcsStatusBroadcaster.ts`):
  the checkout must be on the remote's default branch (`refs/remotes/<remote>/HEAD`, else `main` or
  `master`) with an upstream; after a fetch it must be behind and not ahead, and the work tree clean
  (untracked files count); then `git merge --ff-only` of the fetched branch. Anything else is a
  reason (`not_git`, `detached`, `other_branch`, `no_upstream`, `local_changes`, `diverged`, and
  roll's own `busy`: a thread is running in the checkout itself, and `no_access`), and nothing is
  touched: the user's files and index move only in that one fast-forward (`status` runs without
  refreshing the index, `GIT_OPTIONAL_LOCKS=0`). `project.pull` does the same at once, whether or
  not auto-pull is on, and answers `Reply::Pulled`.
- **The last result** is kept on the project (`Project.pull {at, outcome}`; T3 only logs it), so
  the canvas says what happened ("Pulled 3 commits into main", "Didn't pull: the checkout has
  uncommitted changes"). The same outcome again keeps its first time and writes nothing, so a
  project that stays up to date sends no records.
- **What triggers it.** Both, because a roll code checkout can't fetch on its own:
  - A plain git project fetches with the user's own git setup (credential helpers, ssh in batch
    mode), a round at startup and every `[projects] pull_every` seconds (60; 0 never), as T3 pulls
    every project at startup and polls while a client watches.
  - A roll code project's remote has no credentials, so the relay asks: `project.pull` with a
    fetch-only grant for the repo whenever the repo's default-branch head isn't the project's
    mirrored `git.head`, looked at every 30 s and as soon as the store says something landed (a
    hint; a dropped one loses nothing). Once per head, again after 5 minutes while the checkout
    still isn't there (it may have had local changes). Turning auto-pull on (`project.update` with
    it, granted the same way) pulls at once.
- **Defaults for new threads** (`defaults`: provider, model, effort, runtime and interaction mode,
  worktree or the checkout), T3's project defaults. The composer starts a new thread from them;
  `thread.create` itself always says everything, so the daemon doesn't apply them.
- **Scripts** (`scripts`: id, name, command, icon), T3's project scripts. `terminal.open {script}`
  runs one in a terminal, in the thread's worktree or the project's checkout, with
  `ROLL_PROJECT_ROOT` (and `ROLL_WORKTREE_PATH`) set. The first with `on_worktree` is the setup
  script: it runs in each new worktree (new threads, forks and delegated tasks with one) beside the
  agent, as T3's `ProjectSetupScriptRunner` does for `async` scripts (roll doesn't make the agent
  wait). A script that can't start never fails the thread.
- **Auto-settle** (`auto_settle`): when the project's threads settle by themselves ("Settling").
- Shares leave a project's settings and last pull out (a command can hold anything).

## Settling

Threads settle like T3 Code's (`ThreadSettlementService.ts`, the settle and snooze commands of
`Orchestrator.ts`, `threadSettled.ts`, `Sidebar.logic.ts` there): finished work leaves the canvas
for the settled shelf, by hand or by itself, and comes back the moment anything happens in it. The
state is `Thread.inbox` on the daemon's record, so every device agrees; the relay only mirrors it.
Code: `agent-host/src/orchestrator/settle.rs`; the threads sidebar is
`components/agents/threads-sidebar.tsx`, its rules `lib/agents-inbox.ts`.

- **The record** (`Inbox`): `settle` is unset (the daemon decides), `settled {at, by}` (`by`:
  `user`, `inactivity`, `landed`, `closed`) or `active {at}` (un-settled by hand: auto-settle leaves
  it alone until it is active again), T3's `settledOverride` with its times. `snooze {until, at}`,
  `woke_at` (a snooze that ended by itself: the thread reads "Woke" until it is visited after
  that), `visited_at` (`thread.visit`, when the conversation was last opened; runs that ended after
  it are unread) and `manual` (auto-settle off for the thread, T3's `autoSettleDisabledAt`).
- **By hand.** `thread.settle` is refused (`conflict`) while the thread runs, waits on its user or
  has queued messages, and for an archived thread (T3's checks and reasons). It closes the idle
  provider session, as T3 detaches it; settling a settled thread keeps its time. `thread.unsettle`
  sets `active`. `thread.auto_settle {enabled}` turns auto-settle off or on for one thread.
- **What brings a thread back.** Every new run (a message, a queued message draining, a delegated
  task) clears `settle`, `active` included, as T3 clears the override on every dispatch. A message
  also ends a snooze. A request or a failed run ends a snooze early and records `woke_at` (T3's
  `isThreadSnoozeWokenEarly`); a completed run doesn't. A settled thread never has a request: it
  can't settle while one waits, and it has to run to get one.
- **Snooze.** `thread.snooze {until}` hides a thread until then (in the future, at most a year
  ahead), refused while it waits on its user or has queued messages; a settled thread snoozed comes
  back to the inbox when it wakes. `thread.unsnooze` wakes it now (no "Woke"). T3's clients work
  out when a snooze is over; roll's daemon does it in its sweep and records `woke_at`, so every
  device shows the same thing at the same time.
- **Automatically** (T3's rules, per project: `ProjectSettings.auto_settle {after_days, on_land}`,
  T3's `sidebarAutoSettleAfterDays` and `sidebarAutoSettleOnMerge`, default 3 days and on). The
  daemon sweeps every minute: due snoozes wake, and a thread settles (`by: inactivity`) once its
  last activity is `after_days` old (1 to 90; `null` never). Activity is a run being asked for,
  starting or ending, the thread's creation, or a snooze waking; renaming, visiting or settling
  isn't, so opening a thread neither delays nor blocks it. The run's outcome doesn't matter
  (completed, failed or interrupted), as in T3. Only a thread with no `settle`, auto-settle on, not
  archived, not snoozed, not busy, waiting on nobody and with nothing queued settles by itself; it
  is dated at its last activity, so the shelf reads in the order things happened.
- **Changes.** When a thread's roll code change lands or closes, the relay's follower (which
  stops following it then) sends `thread.change_ended {landed}` once, and the daemon settles the
  thread by the same rules (`by: landed` when `on_land`, `closed` always), T3's settle on a merged
  or closed PR. It's best effort: a machine that is offline then misses it, and inactivity settles
  the thread later. T3's `keepPullRequests` isn't kept: the daemon doesn't know a thread's change.
- **Seen.** The dashboard sends `thread.visit` when a conversation opens and when a run of the open
  one ends; it isn't activity and leaves `updated_at` alone. Agent tokens may settle, snooze and
  visit (they're thread work, like archive); `thread.change_ended` is the owner's.
- **The dashboard.** Settled and snoozed threads leave the canvas (their requests can't exist;
  a thread brought back by new activity comes back). The threads sidebar, left of the canvas and
  toggled with its button or ⌘B (remembered; a sheet on a phone), lists each project's threads as
  T3's sidebar does: active first (newest activity first) with their status (working, waiting for
  you, failed, woke, done and unread), then a collapsed "Snoozed" shelf with each thread's time
  left, then a collapsed "Settled" shelf, newest first, ten at a time and then 25 more per "Show
  more" (the older settled). A shelf's open state is remembered; a collapsed shelf still shows the
  open thread. Clicking a row opens the conversation in the side panel. A row's menu and the
  conversation's menu settle, un-settle, snooze (T3's presets: in 1 hour, in 3 hours, this evening,
  tomorrow, next week, or a custom time or duration) and wake. ⌘⇧S settles the open thread and
  opens the next one in the sidebar's order (wrapping, skipping settled and snoozed ones; the
  canvas when there's none), T3's settle and advance; on a settled thread it un-settles it.

## Sandboxes

A sandbox is a machine roll hosts: a container running `roll agent up` with a machine token, so it
appears on the canvas like any other machine (`Machine.sandbox`: backend, size, status, price). It's
the product for people who don't want to connect their own machine: 3¢ per running hour for a
2 vCPU / 4 GB (`standard`; `large` 6¢, `xlarge` 10¢; sizes are priced in config), and it runs only
while its agents work: it sleeps when idle and wakes when an agent needs it ("Sleep and wake" below).
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
  minutes), starts a sandbox that should run but stopped on its own, puts an idle one to sleep,
  records the backend's status into
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
  Metering ends at the look that sees it stopped, asleep or not.
- **Limits** (config): sandboxes per owner (`ROLL_SANDBOX_MAX_PER_OWNER`, 5), sizes
  (`ROLL_SANDBOX_SIZES`), the URL sandboxes connect to (`ROLL_SANDBOX_API_URL`, default the API URL).

**Sleep and wake** (`sandbox/wake.rs`). Billing is per running hour, so a sandbox runs only while
its agents work: it stops when it's idle or only waiting on its user, and everything about it stays
on the server meanwhile.

- **Idle** means no thread of the machine is `starting` or `running` in the mirror (and the machine
  is connected: offline, the mirror's statuses are the daemon's last word, not work going on). A
  thread only `waiting` on its user is idle: it was asked to wait. A delegating parent isn't idle
  while its children run, because the children are threads of the machine too. Activity the relay
  sees go by also counts: a command for the machine, and terminal output (provider logins happen in a
  terminal). It's noted in memory and folded into the sandbox's durable `last_active_at` at each
  look; a busy look, a wake and an owner's start set it too.
- **Sleeping**: the owner's reconciler stops a running sandbox once nothing happened for
  `ROLL_SANDBOX_IDLE_STOP` (5 min; `0` turns sleeping off). Its desired state becomes `asleep`
  (`Sandbox.status` `stopped`, `stopped_reason: idle`), apart from the owner's stop (`stopped`,
  `stopped_reason: owner`); both are in the API and in `SandboxInfo` on the machine. agent37's own
  auto-sleep isn't used (it counts only inbound HTTP, so it would put a working daemon to sleep);
  roll drives stop and start itself. A reconciler that just started on a node (a restart, or the
  owner's lease moving) never puts a sandbox to sleep sooner than one window after it started:
  what the previous one saw since the last durable write is lost.
- **Waking**: a command to a sandbox machine that's asleep, stopped by its owner or on its way down
  (`message.send`, `thread.create`, `request.respond`, `terminal.open`, `run.interrupt`, `diff.get`,
  `fs.*`, anything through `/commands`, the MCP tools that call it, and new projects) sets it running
  (`starting` on the stream, so every open canvas pulses it), kicks the reconciler, and holds the
  request until the daemon's link is up: a link newer than the one the stop left behind, at most
  `ROLL_SANDBOX_WAKE_TIMEOUT` (180 s). Then the command goes out once, with the usual idempotency:
  the daemon replies only after it has sent every record up to its newest `seq`, its replay
  included, so a client that has the reply finds the woken machine's records. A wake that runs out
  answers `machine_offline` ("still waking up"; the sandbox keeps waking and the same id is safe to
  send again). Clients just wait for the HTTP request (the dashboard's commands have no timeout of
  their own), rather than retrying a "waking" answer. A deleted or deleting (or failed) sandbox
  never wakes.
- **The race**: a command notes its activity before it reads the desired state, and the reconciler
  puts a sandbox to sleep only if `last_active_at` hasn't moved, then reads the notes again (and
  undoes it if one came in): either the command sees the sandbox asleep and wakes it, or the
  reconciler sees the command. A wake takes the sandbox's lock, so a stop in progress finishes first.
- **Nothing is lost while it sleeps**. The mirror keeps every record, so the machine's threads,
  transcripts, requests and terminals still show, with their statuses. `docker stop`'s SIGTERM and
  agent37's stop let `roll agent up` stop cleanly (`Host::shutdown`, 5 s): a run waiting on its user
  is parked as it is (still `waiting`, its requests pending and answerable, its queue kept), any
  other run is interrupted, every provider session is closed (providers write their conversations
  out), and terminals end (`exited`). Then the link sends the records it wrote and closes once the
  relay has acknowledged them (`relay::DRAIN`, 3 s); commands that come meanwhile aren't run (the
  relay sends them again on the next link). A request answered after the wake resumes the
  conversation ("Requests across a restart", in the domain model).

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
  are satellites of their thread. Only what is running right now moves: a running thread or
  subagent (and the edge into it) and a booting sandbox carry a calm accent pulse (`--accent`,
  1.6 s loop, off with reduced motion); the machine and project above them, and anything waiting
  on you, keep still. Cards stay small: a thread's card is its status, title, what it's doing and
  what it cost; a project's card sums it up like T3's project view (threads, running and
  waiting, the PRs its threads made, the agents working in it); actions on machine and project
  cards show on hover (always on touch screens). The end of a thread card's footer and the
  conversation's composer footer show its context window as a small ring (T3's
  `ContextWindowMeter`; "62% of 200k" on hover, tokens only when the window is unknown),
  monochrome until it's more than 90 % full, then accent. Approvals are answered right on their card;
  questions say what they ask and open the conversation. The layout is a tidy tree in columns
  (one per depth, as wide as its widest card), so cards never overlap. Dragging a card moves it
  among its siblings (they make room as it passes; ⌥↑/⌥↓ do the same from the keyboard); that
  order is remembered per owner and synced through roll, new work keeps its default place
  (newest first), and the view is remembered per device. One canvas engine (canvas 2D + `requestAnimationFrame`, paused offscreen) draws edges,
  pulses and the dot field; cards are DOM, positioned by the same world transform. Navigation is
  Figma's: scrolling pans (two fingers on a trackpad, any direction; shift turns a mouse wheel
  sideways), pinching zooms about the fingers (ctrl+wheel in Chromium, Firefox and Electron,
  gesture events in Safari, two touches on a touch screen), ⌘/ctrl+wheel zooms with a mouse,
  dragging the background or space-drag pans, and + − 0 1 and the arrows work from the keyboard.
- **Views.** The canvas is a workspace the owner composes (`lib/agents-views.ts`,
  `components/agents/views.tsx`). A slim bar over it holds "All" (the whole canvas) and the
  owner's views as tabs, each with how many threads it picks. A view is a named filter (machines,
  projects, thread status: running, waiting on me, errors, recent; providers; a text match on the
  title, project and machine) with its own sibling order and its own frame (pan and zoom, per
  device). It starts with "Waiting on me" and "Running"; views are created with + (or from All's
  filter: type in the filter field or open the filter editor, then "Save view" or ⌥S), renamed
  (double-click or the tab's menu), reordered (drag a tab, or "Move left/right") and deleted. A
  filter that picks threads keeps them, what they hang off and their satellites; one on machines
  or projects alone keeps them whole. Switching eases the cards to their new places and the frame
  to the view's (or fits it); ⌥1…⌥9 jump to a view (1 is All), `[` and `]` step, `/` filters
  (the canvas's own + − 0 1 and arrows stay). Which view shows is per device.
- **Pinned cards.** A thread's conversation or diff, or a terminal, can be pinned onto a view
  (the pin buttons over a card, or the pin in the side panel's title bar) as a big live card
  beside its thread: a pinned conversation shows its newest runs as they stream, answers its
  requests in place (they leave the canvas beside the thread) and has its composer, so a view can
  show several agents working side by side without the side panel. Pinned cards are children in
  the tidy layout (a conversation and diff first under their thread, a terminal in its own card's
  place), so they never overlap; the header drags them among their siblings, the corner resizes
  them (arrows on the focused corner too). They always show on their view whatever its filter,
  at most 12 per view, sizes remembered per view.
- **A conversation.** Clicking a thread opens it in a panel docked beside the canvas (resizable, remembered; on a phone it covers the canvas), its card selected on the canvas. A machine's or project's settings open in the same panel (`#agents/settings/<key>`). Finished subagents (a done provider subagent, a delegated thread that's done) leave the canvas; their results stay in the parent's transcript. The conversation shows the transcript
  in a centred column, the composer under it, the diff (of the thread or one run) or the thread's
  terminals beside it. Everything is managed from there: approvals in the transcript, fork, roll
  back, publish, share, rename, archive, delete, take a work item. The title is a switcher to the
  other conversations (the ones waiting on you first), so there's no need to go back to the
  canvas; Escape (or ←) goes back. The canvas stays mounted underneath, so going back is instant.
- **A new agent.** Pick the machine and project (the last used by default; adding a project is
  right there), say what to do, and it opens as a conversation. Recent conversations are listed
  under the composer.
- **The model picker** (`model-picker.tsx`, T3's `ProviderModelPicker`, `ModelPickerContent`,
  `ModelListRow`, `ModelPickerSidebar` and the docked effort column of `ComposerModelEffort`).
  The trigger names the provider, the model and its effort. The popup has a sidebar (Favorites,
  Recent, then one button per installed provider of the machine), a search across every provider
  (T3's ranking: exact name, prefix, word start, substring, fuzzy from three letters; favourites
  boosted), the list (favourites first in a provider; a star on each row, a check on the picked
  one) and the picked model's effort levels in a column beside it. It opens on Favorites when
  there are some, else on the provider in use. Keyboard as T3's: type to search, arrows and Enter,
  ← or Shift+Tab to the providers, → to the effort levels, ⌘/Ctrl+Shift+↑/↓ to switch providers.
  Picking a model with effort levels moves to them; picking a level closes it. Permissions are a
  menu beside it (T3's `PermissionsMenu`). Favourites and the last five picked models are kept in
  this browser (`lib/agents-models.ts`, `modelPrefs`); T3 keeps favourites in its server
  settings, and moving them to an owner-level record is the one seam to change.
- **Controls.** The agents surface uses T3's own controls, never native `<select>`s or
  checkboxes: `components/agents/ui/` ports T3's `components/ui` (Base UI) to roll's colours —
  `Select`, `Menu`, `Popover`, `Combobox`, `Switch` and `Segmented`. Popups float in a
  translucent panel with a soft shadow, grow out of their trigger (scale 0.98 and a fade, 150 ms)
  and honour reduced motion; highlights are a neutral fill and only an "on" state is accent.

- **Getting started.** An owner with no machines sees the globe (`RollMark`) and the two ways in
  side by side, the same as "Add machine" on the canvas: connect your computer (install,
  `roll agent login`, `roll agent up`) or rent a sandbox (sizes and prices from
  `GET …/sandboxes`, optional provider keys). A new sandbox is on the canvas at once and pulses
  while it boots. A sandbox's card shows its size, status, price and this month's usage, with
  start, stop and delete, and puts logging a provider in first (a terminal on the canvas running
  the device login). A machine with no projects offers adding one (a folder, a clone of one of the
  owner's roll code repos, or a new repo, with auto-pull offered right there: on for a clone, off
  for a folder); a project's card has "Settings" in its menu (in the side panel beside the canvas: auto-pull and
  its last result, new-thread defaults, scripts); a project with no threads offers starting one. Offline
  machines are dimmed with when they were last seen. A sandbox that isn't connected is never
  "offline": it's asleep ("Asleep · wakes when an agent needs it", no Stop), stopped, or waking
  (pulsing, on every open canvas), and its threads' composers, approvals and actions stay usable
  (what's sent wakes it; `machineState` in `agents-api.ts`).
- **roll code.** Thread cards and sessions show `Thread.change` (a chip linking to the change)
  and `Thread.work`. Publishing shows what landing did as a result, never an error; a thread
  outside a worktree is offered a fork into one first, and new threads in a roll code project
  start in a worktree.
- **Phones.** Below 640 px the popup is the whole screen; a conversation's actions fold into its
  menu and the diff or a terminal takes the transcript's place; the oldest waiting request sits at
  the bottom of the canvas, answerable with one tap; fitting the canvas keeps cards readable (at
  least 55 %) and starts at the machines.
- **Remembered on every device.** How the owner arranged the canvas is kept by roll
  (`GET`/`PUT /v1/code/agents/{owner}/canvas`; `agents/canvas.rs`), so the laptop, the phone and
  the desktop app show the same arrangement. The document is the canvas's own (at most 64 KiB of
  JSON) and opaque to roll; the dashboard reaches it through `AgentsServices.canvas`. It is
  `{ order, pins?, views? }`: `order` and `pins` are All's, `views` the owner's views
  (`{ id, name, filter, order?, pins? }`, at most 16). A document from before views (`{ order }`)
  still parses and shows the built-in views; `views: []` means the owner deleted them all. Saves
  wait 350 ms (typing a filter, resizing a card is one save) and stay under the limit by
  dropping the least recently made arrangements first. A save names
  the revision it was made over: over a stale one it's refused with what is there now (409,
  `CanvasConflict`), for the canvas to merge and save again. Each save goes out on the event stream
  as `canvas` with its revision, and an open device showing an older one loads it again
  (`AgentsData.canvas`). Agent tokens read it but never arrange it. The demo keeps it in memory
  for as long as the page is open.

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
