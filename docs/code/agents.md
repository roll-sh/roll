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
- **`apps/dashboard`**: the canvas at `/dashboard/agents`, the "rolling" popover in the header,
  public shares at `/agents/s/<id>`.

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
- **Runtime modes** (T3's): `full_access` (no approvals), `auto_edit` (edits in the workspace
  allowed, commands ask), `ask` (everything asks). **Interaction modes**: `default`, `plan`.
- **Checkpoints.** Before the first run the daemon records the base tree; after every run it
  snapshots the worktree into a commit under `refs/roll/checkpoints/<thread>/<ordinal>` using a
  private index (`GIT_INDEX_FILE`), never touching the user's index or HEAD. The diff of run N is
  checkpoint N-1 → N. `checkpoint.rollback` restores the tree of a checkpoint and marks later
  runs `rolled_back` (and rolls back the provider thread where the provider can).
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
daemon → relay   delta {delta}                          ephemeral
relay  → daemon  command {id, by, command}              by: who sent it (owner / agent name)
daemon → relay   reply {id, result}                     exactly one per command id
relay ↔ daemon   ping / pong                            every 20 s; 60 s silence drops the link
```

A command id is the client's idempotency key: a daemon that already ran a command id replies
with the stored result instead of running it again (T3's command receipts). The daemon
reconnects with jittered backoff (1 s doubling to 30 s).

## Providers

Each adapter implements `agent_host::provider::Provider` and turns the provider's native
protocol into `ProviderEvent`s (item started/updated/completed, text deltas, requests, turn
completed, native thread ref). The orchestrator owns ids and turns those events into entities.

- **Codex**: `codex app-server` over stdio JSON-RPC (`thread/start`, `thread/resume`,
  `turn/start`, `turn/interrupt`, `turn/steer`; approvals arrive as server requests
  `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, …).
- **Claude**: the `claude` CLI with `--input-format stream-json --output-format stream-json`
  and the SDK control protocol on stdio (`initialize`, `can_use_tool` requests answered with
  allow/deny, `interrupt`, `set_permission_mode`), the same protocol Anthropic's Agent SDKs
  speak. Subagents come from `parent_tool_use_id`; questions from `AskUserQuestion`.
- **ACP**: any Agent Client Protocol agent (`session/new`, `session/prompt`,
  `session/request_permission`, `session/update`), configured in the daemon's config.

**Auth is the user's own.** The daemon runs the provider CLIs installed on the machine with the
logins already there, exactly as T3 Code and Conductor do; roll never reads, copies or relays a
provider credential. API keys and `ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL` can be set per provider
in the daemon config. A provider's status (installed, version, logged in) is reported in
`hello` and refreshed on `provider.refresh`; logging in happens in a terminal on the canvas.

## Machines, login and tokens

- `roll agent login` starts a device login: `POST /v1/code/agents/login` returns a `device_code`
  and a short `user_code`; the user opens `/dashboard/agents/connect?code=…` (signed in) and
  approves; the CLI's poll then receives a machine token `rlm_…` and stores it in
  `~/.config/roll/agent.json` (0600). The dashboard can also mint one directly ("Add machine").
- Machine tokens are stored as sha256 only, looked up on every connect (never cached), and only
  open `/v1/code/agents/connect`. Revoking a machine closes its link at once.
- Commands need `Code::owns` on the owner (agent tokens may drive threads, so agents can
  orchestrate agents); adding, renaming or revoking machines, sandboxes and shares need
  `Code::manages`.

## Sandboxes

A sandbox is a machine roll hosts: a container running `roll agent up` with a machine token,
so it appears on the canvas like any other machine. Backends implement `SandboxBackend`:
**agent37** (gVisor sandboxes through its API; the hosted product) and **docker** (local and
self-hosted; hardened: no new privileges, all capabilities dropped, pids/memory/cpu limits, a
writable home volume). The image is `deploy/agent/Dockerfile` (roll CLI, git, node, codex,
claude).

## Canvas

`/dashboard/agents`. Machines are anchors; projects hang off machines; threads off projects;
forks and delegated subagents off their parent thread; provider-native subagents and pending
requests are satellites of their thread. Edges carry a calm accent pulse while work flows
through them (`--accent`, 1.6 s loop, off with reduced motion). A thread node opens in place
into the full session (transcript, composer, diff, terminal); approvals are answered right on
the node. Positions are laid out automatically and remembered when dragged. One canvas engine
(canvas 2D + `requestAnimationFrame`, paused offscreen) draws edges, pulses and the dot field;
node cards are DOM, positioned by the same world transform.
