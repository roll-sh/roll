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
  Queued messages are edited, reordered and sent now (`message.edit`, `message.reorder`,
  `message.promote`); "Turns" below has the rest.
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
  turn goes past it. Pi rolls back the same way, but its session can't resume part-way: the
  session resumes, then `fork`s at the first user message after the kept turn's leaf, which
  copies everything before it into a new session file (keeping entry ids), and that file is the
  conversation's native thread from then on. (T3 forks in the live process; roll's refs survive
  restarts, which T3's `thread_rollback_after_stop` scenario needs.) Kept runs' refs still name
  the old file, so a later native fork or rollback to one of them falls back to a summary. A
  kept run with no ref moves the cut back to an earlier one, and the runs
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
  - **Google Antigravity** (`acp:antigravity`, `acp/antigravity.rs`, ported from T3's
    `provider/acp/Antigravity*.ts`, `AntigravityAcpSupport.ts`, `antigravityAuthSupport.ts`,
    `AntigravityInstallation.ts`, `Drivers/AntigravityDriver.ts` and `AntigravityAdapterV2.ts`).
    Google's ACP server `agy_acp_server.par` is found on `PATH` with its `localharness_external`
    beside the real binary. It starts with `--uid=` on Linux, `ANTIGRAVITY_HARNESS_PATH` and
    `PYTHONUNBUFFERED=1`. Missing, the detail says to get it from the ACP registry
    (`antigravity-acp`). Its status never starts it, because it unpacks about 1 GB per launch:
    installed means the binary and the harness are there. It is known as Antigravity by its name
    (`antigravity`, or a binary named `agy_acp_server…`) before `initialize`, and by
    `agentInfo.name == "antigravity-acp"` after.
    - It's offered reads but no writes or terminals: advertising writes turns its diff edits into
      whole-file writes. roll names itself; T3 claims to be Zed, which the agent's allowlist
      rewards with Claude and GPT models, so roll's threads see the Gemini models only.
    - `authenticate` goes out eagerly, before `session/new`, with `oauth-personal` (or the first
      method offered). Sending it reads nothing: the agent uses its own stored login.
    - Not signed in, the agent prints `Open the following link to authenticate the ACP server: …`
      on stdout (not JSON). roll ends the start as a sign-in failure, never repeats the link, and
      the provider reports `auth: missing` until a session starts. ACP's -32000 does the same, for
      every ACP agent. To sign in, run it once in a terminal on the machine and send it
      `authenticate` (`(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}'
      '{"jsonrpc":"2.0","id":2,"method":"authenticate","params":{"methodId":"oauth-personal"}}'; sleep 600) | agy_acp_server.par --uid=`
      with `ANTIGRAVITY_HARNESS_PATH` set), then open the link it prints. Google's redirect goes to
      `127.0.0.1` on that machine, so the browser must run there.
    - A resume is `session/resume`, never a `session/load` that replays every tool call. 1.4.2 to
      1.4.5 get `_meta.antigravity.disabledTools: ["run_workflow"]`, because otherwise it writes a
      skill into the workspace. 1.4.5 gets a thread's instructions as
      `_meta.antigravity.systemPrompt.append`; older ones get them in front of the first prompt.
    - The runtime mode is its `mode` option: `yolo`, `auto_edit` or `default`. It has no plan mode
      (its own `/plan` is a message), and it reports subagents.
    - `interaction_*` permission requests are its questions. Each is a single choice of its own
      options, checked before roll's policy, so full access still asks. The answer is the option's
      label, kept unique.
    - The `agy.security.warning` on "allow always" is the approval's reason.
    - Tools are bounded as T3 bounds them: 8,000 characters kept from the end, inline images and
      `formatted_output` dropped. Its names are read: `CommandLine`, `command_line` and
      `commandLine`; `combinedOutput`; `exitCode` and `exit_code`. A command without a kind is
      `execute`. Its titles are tool names (`Running client_view_file`), which become the tool's
      name, with the path shown.
    - `Running start_subagent` and `Run invoke_subagent?` (unless `_meta.is_mcp_tool_call`) are a
      subagent batch, with the role and prompt from its permission prompt's `rawInput.Subagents`.
      It runs until the turn ends, because no child session is reported.
    - A `client_view_file` read that roll served is completed even when the agent later fails it
      with an unrelated error.
    - `stopReason: "refusal"` fails the turn as the quota.
    - Usage is Gemini's: input plus cached reads, output plus thoughts.
    - Models come from its grouped `model` option, and thinking from the `thought_level` option.

    Not ported, by the credential rule or for cost:
    - T3's own profile and credential store (`GEMINI_HOME`, `AGY_ACP_FORCE_FILE_STORAGE`, its
      `acp_token.json`).
    - The OAuth redirect relay (`antigravityCallback.ts`) and the API-key and Vertex sign-in
      methods it stores.
    - The quota reader (`antigravityUsageLimits.ts`), so there are no usage limits.
    - The SQLite read of the agent's conversation database. Turns its log ends on an API error
      stay completed.
    - Edit hunks rebuilt from the file before an edit.
    - Attachments other than images (audio, PDFs, video).
    - Reading subagents' work.
- **OpenCode** (`opencode.rs`, a port of T3's v2 OpenCode driver): OpenCode 2's own server,
  `opencode serve`, over HTTP and server-sent events (`GET /api/event`). One server per provider
  serves every session (each request names its directory); roll starts it with a password it keeps
  in memory and stops it 30 s after its last session. A session is created with the permission
  rules of the thread's modes (`full_access` allows everything; otherwise shell and edits ask,
  edits are allowed in `auto_edit`, work outside the project asks; plan mode is OpenCode's `plan`
  agent, which denies edits), changed between turns. A turn is a prompt whose id roll picks
  (`msg_roll_turn_…`): it is the turn's ref, and rollback (`revert` at the first dropped turn's
  prompt, history only) and fork (`fork` before the next turn's prompt) cut there. Steering is
  native (`delivery: "steer"`), `/compact` is `session.compact`, and `/name` runs a workspace
  command by that name. Permission asks are approvals (accept for session saves OpenCode's own
  `save` patterns as session rules; decline steers T3's note in first, so the model doesn't
  retry), forms with text and choice fields are questions, and `subagent` calls are subagents with
  their child sessions' work beneath them. OpenCode answers a background subagent at once and later
  delivers its report in an execution it starts itself; roll folds that into the turn that started
  it, so the turn ends once the report is delivered. roll's MCP servers are registered per session
  (`PUT /api/experimental/mcp/{name}`) and the session's rules allow only its own; instructions are
  the `roll` instructions entry. The catalog is `GET /api/model` (variants as the reasoning option,
  T3's defaults); a model's `settings` and `headers` (keys, base URLs) are never read. Status: an
  empty catalog means no login (run `opencode auth login` in a terminal); OpenCode 1.x reports not
  installed here and runs as the ACP agent `acp:opencode`. T3 reads OpenCode's `auth.json` and
  database for usage limits; roll never reads OpenCode's private files, so OpenCode shows none.
  Not done: T3's reconnect and backfill (a broken event stream ends the session; the next message
  resumes it) and skills (`$name` mentions).
- **Pi** (`pi.rs`, T3's Pi driver): `pi --mode rpc` with JSON lines on stdio, one process per
  session in its own process group (Pi 0.80.5 or newer). The native thread is the session file
  (`get_state`'s `sessionFile`); a resume is `switch_session` to it. A turn is a `prompt` (Pi
  acks it without an id); steering is the same `prompt` with `streamingBehavior: "steer"`; an
  interrupt is `clear_queue` then `abort`; `/compact [instructions]` is Pi's `compact`, and
  prompts wait while Pi compacts (it refuses them). A turn ends when `get_state` shows Pi idle
  after `agent_settled`; its ref is the session leaf it left (`get_entries`). Pi has no
  approvals and no MCP client, so roll writes its own extension (`pi/extension.ts`, loaded with
  `--extension`): a `tool_call` hook keeps the runtime modes (reads never ask, `auto_edit` lets
  `edit` and `write` through, anything else asks as an `extension_ui_request` confirm), it
  speaks streamable HTTP to roll's MCP servers, and it joins roll's instructions to the system
  prompt. Servers, headers, mode and instructions reach it in the environment only. Changing
  modes restarts the session (the hook keeps one mode per process). Models come from
  `get_available_models` (`provider/id`, plus "Pi default", which keeps Pi's own), with T3's
  thinking levels as the reasoning option (`set_thinking_level`). Pi lists only models it has
  credentials for: none means "log in", with `pi auth check` explaining an unusable default.
- **Muse Code** (`muse.rs`, a port of T3's Muse driver): `muse serve` speaking MSP (JSON-RPC 2.0,
  schema version 1) on stdio, one process per session: `--trust-workspace`, plus
  `--disable-sandbox` in `full_access`. Muse gets the daemon's environment without
  `META_API_KEY` (so it uses its own login, unless `[providers.muse]` sets one) and with
  `MUSE_NO_AUTO_UPDATE=1`, in the workspace's real path. `initialize` asks for session MCP
  servers (an older Muse that can't take them is refused); `session/start` takes a session id
  roll mints (UUIDv7, like every command id), the model, and the approval mode and permission
  profile (`allowAll`/`:unrestricted` in `full_access`, otherwise `promptUnmatched`/`:ask-me`:
  Muse has no accept-edits mode); resuming is `session/resume` then `session/setApprovalMode`,
  and a session Muse lost fails the start so the orchestrator starts fresh and hands the thread
  over. A turn is `turn/start` with a turn id roll mints, `ifBusy: "queue"`, the effort, and
  roll's instructions as the first text part (Muse has no system prompt); a model change is
  `session/setModel` before the next turn. Steering is `turn/steer`, Stop is `turn/interrupt`
  for the turn and any turn joined to it, and `/compact` is `session/compact` (a turn that ends
  with its compaction item; `noop` fails it). Approvals (`approval/requested`, after the server
  request is answered `{}`) offer the narrowest choice per decision (Muse's `abort` is the
  decline for shell); questions are answered with labels and free text (`userInput/answer`).
  Items show as T3's `itemPresentation.ts` does (shell, edits, web by tool name; the user's
  message, reminder children and the todo/question/workflow tool calls hidden); subagents are
  subagent nodes with their child session's items beneath them, a workflow's agents are
  subagents under the workflow, and in `full_access` a child's approval is approved for it (Muse
  doesn't pass `allowAll` to children). A child's approval that comes between turns is asked at
  the start of the next turn. Usage is summed per turn from `session/tokenUsage`, the context
  window is `session/contextUsage`, and `usage/changed` gives the account's windows (between
  turns too); a quota failure ends the turn naming when the limit resets. The catalog is
  `model/list` on a read-only host (`--disable-shell --disable-write --no-session-log`) with T3's
  reasoning tiers; MSP has no login signal, so the login is always unknown and an empty catalog
  says to run `muse login`. Fork, rollback, plan mode and switching modes in a session aren't
  offered. Known gap: roll's orchestrator has no turns a provider starts by itself, so a turn
  Muse starts while idle (its wake after a background workflow finishes) isn't adopted; its
  events are dropped, and a background workflow's agents show only while one of our turns runs.
  A turn Muse starts while one of ours runs (or that still runs when ours starts) is joined to
  ours. roll never reads Muse's home directory, session logs or binary, which T3 reads for usage
  history and updates.

**Models and their options** follow T3 Code's model picker (`apps/web/src/components/chat/
ProviderModelPicker.tsx`, `ModelPickerContent.tsx`, `ComposerModelEffort.tsx`) and its option
descriptors (`ProviderOptionDescriptor`, `packages/contracts/src/model.ts`). Each `ModelInfo` in a
provider's info carries `options`: a `select` (choices with labels, descriptions and a default) or
a `boolean` (with a default), each with an optional `role`: `reasoning`, `speed` or
`context_window`. The role says where a client shows an option, so a client never needs a
provider's own ids. The reasoning option's value is a thread's `effort`. Every other picked option
is in the thread's `options` (`{id: string | bool}`, at most 16, checked by
`agent_proto::check_options`), and so are `thread.create`, `thread.configure`, `ThreadDefaults`,
automations' `agent.start` and `delegate_task`. Nothing picked means the model's default. When
`thread.configure` switches to a new model, the thread keeps the options that model takes
(`ModelInfo::accepted`). A switch of provider clears them, as it clears the effort. A change of
options, like a change of effort, starts the next turn in a new session.

- **Claude** (`claude/models.rs`) follows T3's model manifest:
  - Current and legacy models (`legacy: true`; the picker folds them under "Show more"), each
    with a minimum Claude Code version. A current model the installed CLI is too old for goes in
    `update_required` ("Update Claude Code to v2.1.293 or newer to use …").
  - The profiles' reasoning levels and per-model defaults (Ultracode, Ultrathink), Fast Mode, the
    200k/1M context window and the Haiku 4.5 thinking switch.
  - A selection compiles as `compileClaudeModelSelection` does: the profile's `effortMap`, then
    `--effort`; `fastMode` and `alwaysThinkingEnabled` in `--settings`; `[1m]` on the model for
    the 1M window.
  - A custom slug of a known family gets that family's options.
- **Codex** (`codex.rs`, T3's `mapCodexModelCapabilities`) reads options from `model/list`:
  - reasoning levels with the model's default;
  - service tiers after "Standard" (`serviceTiers`, else the older `additionalSpeedTiers`);
  - text-only models (`inputModalities`).

  A picked tier goes on `turn/start` as `serviceTier`. A custom slug borrows the first model's
  options.
- **ACP agents** apply a thread's options to their own config options of the same id
  (`acp/policy.rs`), when the option offers the value. An agent names its models and options only
  in a session (`session/new`'s `configOptions`, or v1's session models). T3's probe opens one in
  its own directory to find out; roll's probe stops at `initialize`, because a probe session lands
  in the agent's history and may need a login, so it never opens a session or signs in as a side
  effect. Instead the first session the user opens tells the daemon (T3's live configuration,
  `applyAcpRegistryLiveConfiguration`): the session sends `ProviderEvent::Catalog`
  (`acp/catalog.rs`, T3's `acpProviderOptionDescriptors`: the model option's choices as models, the
  current one the default; a `thought_level` as the reasoning levels; every other select or switch
  as an option of the agent's own id; modes, the model and Kiro's Autopilot stay roll's controls;
  16 options, 64 choices, 256-byte texts), and the daemon keeps it per agent and version in
  `agent.db` (`provider_catalogs`), sends `hello` again, and offers it whenever a probe of that
  version lists no models, after restarts too. Until then the picker says "Gemini CLI lists its
  models once a session starts: start a thread with its default model, and they show here
  after."

**Several models at once** (T3's multi-model fan-out, `useMultipleModelSend.ts`): in a new
thread's composer on a git project, Shift-click (or Shift+Enter) in the picker adds a model to the
ones a prompt starts, and again removes it; a plain pick, or one left, is one model again. The
trigger names the first two and how many more ("Opus 5.5, GPT-5.5 Codex, 1 more"; its tooltip all
of them), the picked rows are checked, and the picker's foot says "⇧-click or ⇧ Enter to add a
model". The worktree switch is on and locked ("Each model starts in its own worktree."), and the
send button reads "Start 3 threads": one `thread.create` per model with the same prompt,
permissions and plan mode, each in a worktree of its own. When all started, the first opens and
the rest are on the canvas beside it, siblings under the project; models that couldn't start stay
picked with the prompt, and the composer says which and why ("Started 2 of 3 threads. Couldn't
start …"). Outside a git project it isn't offered (T3 refuses it there too). The threads aren't
linked to each other, as in T3.

**Provider logos.** The picker draws T3's provider logos (`components/agents/provider-logos.tsx`,
generated from T3's path data) in the text's colour, like every mark on the agents surface. roll
never uses orange, so Claude's mark doesn't use its brand colour. A provider without a mark shows
its initial.

**The same picker everywhere.** One `ModelPicker` serves:
- the composer (a thread, and the launcher's new thread), with ⌘⇧M to open it and ⌘⇧E to step
  the reasoning level;
- the canvas's prompt bar (the orchestrator's goal, and "One agent");
- automations' "Start an agent" step;
- machine and project new-thread defaults.

The orchestrator MCP's `orchestrator_capabilities` lists each model's `efforts` and `options`.

**Context usage** comes from each protocol's own report, as in T3's adapters: Codex's
`thread/tokenUsage/updated` (`last.totalTokens` against `modelContextWindow`, `total.totalTokens`
processed); Claude's main-agent message usage (input, cache writes and reads, plus output, from
`message_start`, `message_delta` and the assistant snapshot; `<synthetic>` messages and reports
without input are skipped), with the window from T3's model manifest (`claude/models.rs`) and
`compact_metadata.post_tokens` after a compaction; ACP's `usage_update` (`used`, and `size`
unless it is 0); Pi's `get_session_stats` at each turn's end (`contextUsage.tokens` against its
`contextWindow`, the compaction's estimate while Pi has no count) and the running message's
`usage.totalTokens` while it streams.

**Usage limits** (T3's `ServerProviderUsageLimits` and its usage readers; `provider/usage.rs`)
are `ProviderInfo.usage_limits`: windows (session, weekly, monthly, other), each with its percent
used, when it resets and its length, plus the plan. They come only from what a provider's own CLI or
protocol tells its client:
- Codex: `account/rateLimits/read` in the probe and `account/rateLimits/updated` in sessions;
- Claude Code: `rate_limit_event` (a rejection without overage still fails the turn as a usage
  limit);
- Muse: `usage/changed`.

A session reports them as `ProviderEvent::UsageLimits`, during a turn or between turns. The daemon
merges each window into the provider's info by id, keeping a reset time or length an update leaves
out, and says `hello` again when something changed. A probe that finds none keeps what sessions
reported. T3's readers for Cursor, OpenCode, Grok and Antigravity, and Claude's reset credits, read
the CLI's stored login or keychain and call the vendor's API with it. roll never reads a
credential, so those providers show no limits. The dashboard shows the windows as bars on the
provider's card in machine settings, with "N% left", "resets in …" and T3's even-pace marker. The
picker's provider tooltip gives the tightest window, and a model of a provider whose limit ran out
says "Limit reached".

**Auth is the user's own.** The daemon runs the provider CLIs installed on the machine with the
logins already there, exactly as T3 Code and Conductor do; roll never reads, copies, pools or
relays a provider credential in the clear. A provider can instead use an endpoint (a base URL and
a key: CLIProxyAPI, a gateway), set in the daemon config or from roll, where the key is sealed to
the daemon in the browser so roll only ever carries ciphertext ("Endpoints"). A provider's status
(installed, version, logged in, or "via <endpoint>") is reported in `hello` and refreshed on
`provider.refresh`; logging in happens in a terminal on the canvas.

**Probes never hold up the daemon** (T3's provider registry, which starts from its status cache,
`apps/server/src/provider/providerStatusCache.ts`, and refreshes each provider on its own). A probe
can take a while: an ACP agent gets 20 s to answer `initialize`, and one that never does (a broken
or slow CLI on `PATH`) would otherwise hold up every start. So `roll agent up` says `hello` at once,
with each provider on offer as `ProviderInfo.checking`, showing what its last probe found (kept in
`agent.db`, the `settings` table's `provider_probes`) or, with no last probe, not installed with its
login unknown. Every provider is probed on its own in the background, and each probe that answers
sends `hello` again (the frame that already carries `MachineInfo`; a reconnect's `hello` is the newest
info, and probes still running follow on the new link). `machine.configure` probes again without
waiting. `provider.refresh` says `checking` at once and replies once its probes have answered. A
newer probe supersedes an older one still running. A command that reads a provider's status
(delegation: `orchestrator_capabilities`, `delegate_task`) waits for that probe, at most 45 s, and
then goes on with what's known. Starting a session never waits: the provider's own start says
whether it works. An ACP agent on the PATH that doesn't answer `initialize` within 20 s reports
not installed, with the reason as its detail, so nothing delegates to a hung agent until a refresh
finds it answering. The dashboard shows a provider being checked as a quiet shimmering "Checking…",
on the machine card and in machine settings. Code: `agent-host/src/orchestrator/probes.rs`.

### The ACP registry

T3 Code adds agents from the ACP registry (`cdn.agentclientprotocol.com/registry/v1/latest/registry.json`)
as provider instances from its settings (`packages/provider-acp-registry`, `AcpRegistrySupport.ts`;
`AddProviderInstanceDialog.tsx`, `AcpRegistrySearchStep.tsx`). roll does it per machine: a
machine's settings have "Add agent" beside "Check again", and the daemon does all the work. Code:
`agent-host/src/registry.rs` (`registry/{index,archive,packages}.rs`),
`agent-host/src/orchestrator/registry.rs`, `components/agents/add-agent.tsx`, `lib/agents-registry.ts`.

- **Fetching is the daemon's**, never the relay's (it only forwards the commands, which are the
  owner's). The URL is fixed; only the daemon's own `config.toml` can point it elsewhere
  (`[registry] url`; plain HTTP is accepted there, and then for that origin's archives too, for a
  local copy and the tests). The index is read with a 30 s timeout, at most 1 MiB, redirects only on
  the same host, and parsed with T3's bounds: at most 512 agents, ids `^[a-z0-9][a-z0-9._-]*$`,
  versions that can't be `.` or `..` (they name folders), HTTPS URLs without credentials, packages
  pinned to an exact version. An entry that breaks a bound is dropped; the rest stay.
- **`registry.list`** (a query) answers `Reply::Registry { agents, fetched_at, error }`. It reuses a
  fetch for 5 minutes (T3's search); the daemon keeps the last copy at `<home>/acp/registry.json`
  (written to a temporary file, then renamed) and falls back to it when the registry can't be
  reached, with `error` saying why. Each `RegistryAgent` says which distributions suit this machine
  (`kinds`: a binary only for its OS and architecture, Windows on ARM taking x86-64 like T3; `npx`;
  `uvx`), whether it's `installable` (a binary, or npm/uv on the PATH), what vouches for it
  (`integrity`: `sha256` listed, `registry` for an exact package version, `none`), the version the
  daemon `installed`, and the provider that is it already (`offered`): `acp:<id>`, a known agent's
  own name (`github-copilot-cli` → `acp:copilot`, `factory-droid` → `acp:droid`, `grok-build` →
  `acp:grok`, `qwen-code` → `acp:qwen`, `amp-acp` → `acp:amp`, `antigravity-acp` →
  `acp:antigravity`) or a native driver (`opencode`, `opencode-v2` → `opencode`; `pi-acp` → `pi`)
  that is installed. The dashboard shows those as "On this machine" and never installs them twice.
- **Never at probe time.** Only `registry.install` installs; a probe stays `initialize` only (T3's
  registry probe opens a throwaway session; roll's doesn't, as for every ACP agent).
- **`registry.install {agent}`** fetches the registry (an install always does; the kept copy when
  it can't), then uses the first of: the binary for this machine, npx with the machine's own `npm`,
  uvx with its own `uv` (T3's order). One install per agent at a time.
  - A binary is streamed to a temporary folder beside its destination with its SHA-256, at most
    1 GiB and 20 minutes; a listed hash must match. It's unpacked in Rust (no system `tar` or
    `unzip`) by its suffix: `.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`, `.zip` (stored or deflated), else
    it is the executable. Only regular files and folders: a symlink, hardlink, device or FIFO, an
    absolute path, a `..`, or an entry listed twice refuses the archive; at most 20,000 entries and
    2 GiB unpacked; files are created new. `cmd` is a relative path (no drive, no `..`), must be a
    regular file inside the folder, and is made executable. The folder is renamed into
    `<home>/acp/tools/<id>/<version>/<target>/` only when all of that holds.
  - A package goes into `<home>/acp/tools/<id>/<version>/npm` (`npm install --global` with
    `npm_config_prefix` there) or `…/python` (`uv tool install` with `UV_TOOL_DIR`/`UV_TOOL_BIN_DIR`
    there): never global. The command is found as T3 finds it (the package's `bin`, the agent's id,
    the package's name).
  - A failure (a hash that doesn't match, a download that fails, an unsafe archive) leaves nothing:
    no files, no entry, no provider. Other versions of the agent are removed after a successful one.
  - `<home>/acp/installed.json` records it (id, the registry's name and version, the command, its
    arguments and the registry's variables, when), and the provider `acp:<id>` is added to the
    running daemon (`Host::add_provider`: configured, probed in the background, `hello` at once with
    `checking`). It replies once all of that is done. The next start offers it too
    (`registry::acp_providers`: `config.toml`'s `[[acp]]` first, then installed agents, then known
    agents on the PATH; a configured `[[acp]]` of the same name wins). `MachineConfig.registry`
    lists the installed agents, so their cards in the settings get "Remove" (unset: a daemon that
    can't install, and the panel is read-only).
  - The relay waits 30 s for a reply; a longer install answers `timeout`, and the dashboard sends
    the same command id again: the daemon runs a command id once, so the retry waits for the same
    install and gets its reply.
- **`registry.remove {agent}`** is refused while a thread on it runs; its idle sessions close, the
  provider goes, then its entry and its folder.
- **Credentials: never.** The registry's variables are configuration (`AUGMENT_DISABLE_AUTO_UPDATE`);
  any name `is_credential_env` flags is dropped when the index is parsed and when `installed.json`
  is read. An installed agent signs in as any ACP agent does, in a terminal or through an endpoint.
- **The dashboard** (`add-agent.tsx`): a popover from "Add agent" (glass, scaling from its trigger)
  with T3's search and rows: the registry's icon (only from `https://cdn.agentclientprotocol.com/`,
  recoloured to the text's colour with T3's SVG filter; the registry's mark otherwise), name, one
  line of description, version, "About", and Install / Installing… / Installed / Update / On this
  machine / Needs npm / Needs uv / Not for this machine. Errors show under their row. The demo's
  daemons list a dozen real agents and install one after a short "Installing…".

## Turns: queue, steer, compact, subagents, plans

How a conversation moves while an agent works, as T3 Code does it (its orchestration-v2
`CommandPolicy.ts`, `Orchestrator.ts` queue handling, the adapters' compaction and subagent
mapping, and the web's `QueuedRunsControl.tsx`, `ContextWindowMeter.tsx`, `V2LifecycleRow.tsx`,
`ProviderSubagentBar.tsx`, `ProposedPlanCard.tsx`, `proposedPlan.ts`).

- **Queue and steer.** While a run goes, Enter steers it and ⌘/Ctrl+Enter queues (the owner's
  "While a run is going" swaps the two, T3's `followUpBehavior`; with ⌘Enter to send, ⌘⇧Enter
  is the opposite for one message). Queued messages sit on top of the composer (`queue.tsx`):
  three, then "Show all N queued"; each can be sent now (`message.promote`: into the running turn,
  T3's promote-to-steer; on a stopped thread, as the next run, after which the rest drains, T3's
  `queued-run.start`), edited in the composer (`message.edit`; the row says "Editing in the
  composer", the button "Update queued message", Escape gives the draft back; ⌥↑ edits the last
  one), removed, or moved (drag a row, or ⌥↑/⌥↓ on it: `message.reorder`; T3 has reorder only in
  its MCP tools). ⌘⇧Enter with nothing typed sends the first queued message into the turn. A
  stopped or failed run holds the queue, and a restart keeps it held (the thread's `queued`
  persists): the panel says "Queue paused" with Resume, the empty send button is "Resume thread",
  and a new message starts at once with the queue resuming after it. All of this was checked
  against T3's rules on a real stack (rolld and `roll agent up` with the fake provider: queue,
  edit, reorder, remove, refusals, a daemon restart with a parked run, drain, Stop holding the
  queue, Send now both ways, steer); the one difference is on failure, where roll holds the
  queue whatever failed and T3 lets a validation error or another provider's run through.
- **Messages.** A sent message has T3's "Edit from here" (`useTimelineRewind.ts`): after asking,
  the thread goes back to the checkpoint before it (`checkpoint.rollback`: the files, and the
  conversation where the provider can) and the message comes back in the composer, appended to
  what's typed, images too (`lib/agents-compose.ts`); not while the thread runs, and not without a
  checkpoint (a project outside git). Copy, and Fork on a finished run, are T3's; T3 has no retry
  either (Resume continues a stopped thread). T3's rollback without restoring files isn't
  offered: a checkpoint is the files.
- **Compaction.** Claude Code and Codex compact on their own as the context fills; both also
  compact on request (`Capabilities.compact`). A message of exactly `/compact` runs as a
  compaction, a run of its own (T3's `compactThread`): Codex `thread/compact/start` (the turn it
  starts is adopted as ours), Claude `/compact` as the turn's input (its result text is no reply),
  Pi its `compact` command (`/compact <instructions>` passes them on).
  A compaction can't be steered: a steer sent to it queues, and Send now refuses. Every compaction
  is a `NodeBody::Compaction` row (T3's `compaction` item: Claude's `status: compacting`,
  `compact_result`, `compact_boundary` with `pre_tokens`/`post_tokens`; Codex's
  `contextCompaction` item; ACP's `compaction_update`), with the context before and after: the
  provider's counts where it gives them, else the thread's context when it started and the next
  report after it. The transcript shows it as T3's separator ("Compacting context", "Context
  compacted 182k → 41k tokens", "Context compaction failed · why"); the context meter drops and,
  in the composer, opens on hover with "Compact context" and "Context compacts automatically when
  needed"; `/compact` is in the slash menu.
- **Auto-compact and idle compaction** (T3's Claude instance settings and `IdleCompaction*`;
  machine settings, "Machine settings"). Codex has neither knob in T3, so neither does roll.
  - *Auto-compact after* (Claude's `autoCompactWindow`, 100,000 to 1,000,000 tokens, empty for
    Claude's default) goes into the CLI's `--settings`. Claude compacts 33,000 tokens short of it
    and caps it at the model's window, so the context report carries `auto_compact_at` only below
    the window, and the meter says "Compacts automatically at about 267,000 tokens."
  - The context report also carries the prompt cache's lifetime, `cache_ttl_ms`, from the
    request's `cache_creation` (an hour for 1-hour entries, as on subscriptions; five minutes for
    5-minute ones, as on API keys; unknown when the request was served whole from the cache).
  - *Compact idle threads*: on the machine (default "When you return"), and per provider over it
    ("Use default"), for conversations over 200,000 tokens. **When you return** (the default):
    once a conversation has been idle since its last run for longer than its cache lives (Claude
    without a reported lifetime: 70 minutes), the composer's button reads "Compact and send"
    ("Summarize 312k tokens of history, then send"): it sends `/compact`, then the message, queued
    behind it. **Before the cache expires** (beta): the daemon (`orchestrator/idle.rs`) keeps a
    timer per thread from each completed run and, two minutes before the cache expires, compacts
    the thread by itself if nothing changed (the same newest run, idle, nothing waiting or queued,
    not settled, snoozed or archived, still the run's provider, which compacts); the run is marked
    `idle_compaction`, shows no message of the person's, and its row reads "Context automatically
    compacted before cache expiry". A compaction is never compacted again. Timers live in memory,
    as in T3: a restart drops them. **Never** does neither.
- **Subagents.** Claude's Agent (or Task) tool, Codex's collab agents and roll's delegated threads
  are `subagent` nodes; Claude's `task_progress` fills `progress` while one runs, and the Agent
  call's (Codex's collab call's) `model` is kept. In the transcript each is T3's row: an avatar
  with a status dot, what it was asked, what it's doing now or its answer, a live timer and a
  chevron; several started together sit under "N subagents · k running". A provider's subagent
  opens its own transcript in place of the conversation (what it was asked, its steps and words,
  its answer), with T3's bar instead of the composer (the agent and model, "Working 1m 5s" or
  "Completed in 34s", "Runs on its own", Open parent; Escape goes back). A delegated thread opens
  as the thread it is, and its composer says who delegated it with Open parent. Pinned cards,
  which have no subagent view, open a provider's subagent inline under its row.
- **Plans.** `interactionMode: plan` reaches Claude as `--permission-mode plan` and Codex as its
  collaboration mode `plan` (ACP: an agent's `plan`/`architect` mode). The plan is T3's card:
  "Plan", its title (the first heading), a menu to copy it, download it or save it to the
  workspace, long ones folded; it stays out of the "Worked for" fold. "Save to workspace" is T3's:
  a path relative to the thread's folder (its worktree, else the project's), the title as the file
  name by default, written with `fs.write` (folders made, never out of the workspace or through a
  symlink, an existing file replaced, 1 MiB at most; the owner's only). A plan is ready (`lib/agents-plan.ts` `readyPlan`) when the
  newest run ended with one in plan mode (Codex, ACP) or Claude asks to leave plan mode with it
  (its ExitPlanMode is an approval with `action: plan`; T3 denies that tool and captures the plan
  instead, roll keeps it pending so Claude goes on in the same turn when accepted). Then the
  composer says "Plan ready · title", the header pill says so, and the button is T3's split pill:
  Implement (Claude: accept; otherwise switch the thread to build mode and send T3's
  `PLEASE IMPLEMENT THIS PLAN:` prompt), "Implement in a new thread" (a thread in build mode on
  the same project, provider, model and permissions, with its own worktree if this one has one,
  started with that prompt and opened; a pending approval is cancelled first), or, once something
  is typed, Refine (Claude: decline with the words; otherwise send them in plan mode). Shift+Tab,
  the Build/Plan chip, and `/plan` and `/default` switch the mode. Todo lists stay quiet rows
  ("Tasks 2/3 · current"). Not done: T3's "Save to workspace".

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
- **Tested end to end** without a model: `ROLL_AGENT_FAKE=1 roll agent up` also offers
  `orchestrate` (`provider::fake::Script::Orchestrate`), a fake orchestrator model that calls this
  server over HTTP as Codex or Claude would (`orchestrator_capabilities`, then an async
  `delegate_task` per part), ends its turn, and sums up once both parts' results have woken it.
  `scripts/agents-e2e.mjs` drives it through rolld and the relay.

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
  - Terminals, files outside a project (`fs.list` without one), writing files (`fs.write`),
    answering a thread's approvals
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
- **Cleanup** (`cleanup`): when the worktrees of its settled and archived threads go ("Settling").
- Shares leave a project's settings and last pull out (a command can hold anything).

## Identity

Projects, machines and sandboxes are recognisable at a glance, the same everywhere they show: canvas
cards and edges, the threads sidebar, ⌘K rows, the launcher, the conversation's breadcrumb, settings
and the automations list. T3 Code's project icons (`ProjectFavicon.tsx`, `ProjectMonogram.tsx`,
`projectIdentity.ts`, `ProjectFaviconResolver.ts`, `ProjectIcon*` in `packages/contracts/src/project.ts`
there), with roll's own palette. Types: `agent-proto/src/identity.rs`; the dashboard's are
`lib/agents-identity.ts` and `components/agents/identity.tsx`; the look is in agents-design.md,
"Identity".

- **A colour each** (`IdentityColor`: slate, violet, purple, pink, rose, teal, cyan, green). T3's
  palette has orange, amber and yellow, which roll never uses, and blue, which on roll means busy:
  none of those is in it. Unset, a project's colour is T3's hash of its repo (`owner/repo`), else
  its name, so the same repo checked out on a laptop and in a sandbox (two projects) looks like one
  on every device; a machine's is the hash of its id, so a rename keeps it.
- **A project's icon** (`ProjectSettings.icon`): `auto` (its own logo, else its monogram on its
  colour), `monogram` (with letters of its own: 1 to 3), `emoji`, or `file` (a picture in the
  project, T3's `faviconPath`). `ProjectIcon::check` holds the rules (mirrored in
  `checkProjectIcon`); `project.update` refuses what it refuses. A colour and an icon are project
  settings, kept by the daemon, so every device agrees.
- **A project's logo** (`Project.logo {path, url}`) is found by its daemon (`agent-host/src/logo.rs`,
  T3's resolver): the `file` icon's picture, else the first of T3's well-known favicon, app icon
  and logo paths (less `.icns`, which browsers can't show), else the icon `index.html` or a root
  route links to. It rides in the project record as a `data:` URL rather than behind a command:
  logos are small once bounded, every client and share needs them, and a record already syncs to
  every device and survives the machine going offline. Bounds: each candidate is resolved inside
  the project (a symlink out of it is never read) and must be a regular file; at most 1 MiB of a
  source file and 2 MiB of a picture are read; an SVG goes as found, anything else is decoded with
  limits and re-encoded as a PNG of at most 64 px; a URL over 48 KiB is skipped for the next
  candidate. AVIF isn't decoded. It's looked for when the project is added, when its icon setting
  changes, after a pull that moved it, and on each auto-pull round (hourly when auto-pull is off),
  with a cache by path, size and mtime; the record is rewritten only when the logo changed.
  Clients show it only in an `<img>` (an SVG's scripts never run) and accept only
  `data:image/png` or `data:image/svg+xml` URLs.
- **A machine's colour and kind** (`Machine.appearance {color?, kind?}`; laptop, desktop, server,
  cloud, container) belong to the relay with its name (`PATCH …/machines/{machine}` `{name?, color?,
  kind?}`, `null` clears one), not to its daemon: an offline laptop or a sleeping sandbox has no
  daemon to ask, and the canvas shows it anyway. Unset, its kind comes from its OS (a sandbox is
  cloud, a Mac a laptop, Windows a desktop, the rest servers). They're in the machine's settings
  ("Machine": Colour, Kind) and change while it's offline.
- **Never state.** Colour tells things apart; the accent still means busy, green done, red failed.
  A card's border keeps its state; identity is the icon tile, a flat tint behind a machine's or
  project's header, the project's tile and left edge on a thread's card, and the edge into a card
  (busy edges stay accent).

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
  provider session, as T3 detaches it, and the thread's terminals that have finished (T3's
  `closeIdle`; a running one, a dev server, stays); settling a settled thread keeps its time.
  Archiving closes all of the thread's terminals, as T3 does. `thread.unsettle`
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
  the thread later. `auto_settle.keep_changes` (T3's `sidebarAutoSettleKeepPullRequests`, off by
  default): inactivity never settles a thread whose change is open. The daemon knows that from its
  own publish (a successful `thread.publish` to a roll code repo, kept in the thread's private
  state) until `thread.change_ended`; a plain git project's push opens nothing roll can follow, so
  it doesn't count.
- **Cleanup** (T3's `storageCleanup.ts` worktree rules, `ProjectSettings.cleanup`, off by default
  as in T3; code: `agent-host/src/orchestrator/cleanup.rs`). With `worktree_after_days` set (0 to
  3650), the minute sweep removes the worktree of a thread settled or archived that long ago (and
  at least a minute since its record last changed, T3's 60 s). Every check runs again on the live
  record just before, and any of them keeps the worktree (`KeptReason`): the thread is busy, waits
  on its user or has queued messages; a running terminal is in it; another thread works in it (a
  fork without a worktree of its own); it isn't one roll made (outside the worktrees folder, a
  symlink, the checkout itself, not the top of its own work tree); it's on another branch than the
  thread's; or it has local files (`keep`: `uncommitted_changes`, T3's default, is changes to
  tracked files and untracked files git doesn't ignore; `any_local_files` counts ignored files,
  `.env` and build output, too). T3's `tracked-changes` option isn't offered: it would remove
  untracked files. Removal is `git worktree remove` without `--force`, so git refuses a dirty tree
  as well. The branch (with every commit, pushed or not) and the checkpoints always stay, and
  `Worktree.removed_at` says it's gone. Diffs and `git.status` keep answering from the project's
  repository (worktrees share its objects and refs). The thread's next run, rollback, terminal,
  publish or a fork working in its folder puts it back first, in the same place, on its branch
  (T3's turn start and checkpoint restore do the same); if the branch was deleted, a new one from
  the worktree's base gets the newest checkpoint's files. `project.cleanup` does it now for every
  settled or archived thread of the project, whatever their age, and answers `Reply::Cleaned`
  (what went, what stayed and why), T3's "Run now". T3's other rules (inactive, merged, unchanged
  worktrees, browser artifacts, rotated logs) aren't ported: an inactive thread settles first, and
  the daemon keeps no such files.
- **Idle sessions.** A provider session nobody has used for 30 minutes is closed (T3's
  `idle_timeout`); the next message resumes the native conversation in a new one.
- **Delete** removes the worktree only when it has no uncommitted changes; otherwise the folder
  stays (T3 never removes one without asking), and the branch always does.
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
- **The shelf** (`components/agents/project-shelf.tsx`, `lib/agents-shelf.ts`). What left the
  canvas stays off it: no project card carries a list. "Shelf" in the canvas toolbar, beside
  Display, counts what's settled, snoozed and archived (the focused project's while one fills the
  canvas; no control when nothing is) and opens a popover with every one of them, grouped by
  project, a tab per shelf, 40 rows and then 60 more per "Show more". The search matches every word
  in the title or the project's name; ↑/↓ walk the rows, Enter opens the conversation in the side
  panel with its transcript, diffs and checkpoints as they were, and ⌘Enter (or a row's button)
  restores it (un-settle, wake or unarchive). A project's ⋯ menu ("Settled and archived (12)"),
  ⌘K "Settled and archived in…" and a project's header in the popover open it narrowed to that
  project, with a chip whose × widens it again (`?shelf` or `?shelf=<project>` in the URL opens it,
  the demo's way in). Linear keeps its archive behind one control, not
  on each team; T3 has the Settled shelf in its sidebar, which roll's sidebar keeps too. The
  conversation itself says where it is above the composer, T3's "Settled 2d ago" line with
  Un-settle (Wake, Unarchive), and that its worktree was cleaned up and comes back with the next
  message.
- **Archive** is T3's: a thread leaves the inbox and the canvas, its session and terminals close;
  unarchive brings it back as it was. T3 lists archived threads in settings; roll keeps them on an
  "Archived" shelf per project in the sidebar and on the toolbar's shelf, and ⌘K finds them.
  Delete asks first (the owner's "Confirm before deleting a thread", T3's text plus what roll
  keeps: the branch, and a worktree with uncommitted changes).
- **Undo** (T3's `showThreadUndoNotice.ts`): settling, snoozing, archiving and restoring show one
  notice at a time for 5 seconds, from wherever they were done (the sidebar, the shelf, the
  conversation's menu, ⌘K); Undo or ⌘Z (outside a text field) puts the thread back.

## Machine settings

What lives on a machine is set from roll and kept by its daemon (T3 Code's server settings,
`apps/server/src/serverSettings.ts` and the provider instances of `packages/contracts/src/settings.ts`
there). `machine.configure {settings}` replaces the machine's `MachineSettings` whole; the daemon
checks them, stores them in `agent.db` (the `settings` table, never mirrored as a record), applies
them, sends `hello` again and then replies, so a client that has the reply finds them on the relay.
They ride in `MachineInfo.settings` (`MachineConfig`): what was set (`set`), what applies where it
says nothing (`base`: `config.toml`, else roll's defaults), every provider the daemon has, on or off,
with what `config.toml` gives it (its binary, how many arguments, the *names* of its variables), and
the path of `config.toml`. Riding in `hello` rather than an entity keeps them with the machine record
the relay already persists and streams (`machine` events, the last word of an offline machine), and
next to `providers`, which they change; a lagged link sends `hello` again, so nothing is lost. Code:
`agent-host/src/orchestrator/settings.rs`; the panel is `components/agents/machine-settings.tsx`.

- **Over `config.toml`, never rewriting it.** `config.toml` stays the machine's own file. Every
  setting overrides it: the binary instead of its binary, arguments after its arguments, variables
  over its variables of the same name; unset falls back to it.
- **Never a credential in the clear.** `MachineSettings::check` (agent-proto, mirrored in
  `lib/machine-settings.ts`) refuses any variable whose name holds `KEY`, `TOKEN`, `SECRET`, `PASS`,
  `CREDENTIAL`, `AUTH`, `COOKIE`, `SESSION`, `PRIVATE`, `URL`, `ENDPOINT`, `PROXY` or `HOST`
  (`is_credential_env`): keys, tokens and anything that points a provider somewhere else, because
  settings travel and are reported in the clear. The dashboard refuses them before sending, the
  relay before forwarding (400) and the daemon again. Base URLs and keys go in an endpoint instead
  ("Endpoints", sealed to the daemon) or in `config.toml` on the machine; the panel shows
  `config.toml`'s variables by name only: their values never leave the machine.
- **A provider turned off** (`enabled: false`) leaves `MachineInfo.providers` (never probed), so
  nothing offers it; new threads, switching to it and delegating to it are refused (`provider`), and
  a thread already on it takes no new turn until it is turned on or switches provider. Rolling back
  its records still works.
- **Binary, arguments, environment** reach the adapters for their next process (`Provider::configure`,
  `LiveConfig`); a running session keeps what it started with.
- **Custom models** are added to the provider's `models` (`custom: true`), after its own, so every
  picker lists them and delegation accepts them. The panel holds T3's rules (a slug, at most 256
  characters, not built in, not twice). Each one's chip opens T3's custom model editor ("Edit name
  and options", `custom-model-editor.tsx`, logic in `lib/custom-models.ts`): a display name and the
  options the composer shows for it (`CustomModel.options`, T3's custom `optionDescriptors`), copied
  from one of the provider's models ("Copy from…", without Claude's context window), added from the
  provider's presets (Reasoning, Fast Mode, Thinking on Claude; Reasoning and Speed, `serviceTier`,
  on Codex; Reasoning elsewhere) or the owner's own (an id the provider takes, a label, choices or a
  switch, a default). The reasoning levels are always a select with the id `effort` (the thread's
  effort).
  None: the provider's own options for a slug like it. The daemon checks them with T3's messages,
  numbered as the editor shows them ("Option 2: id "a" is used twice."; `check_custom_options`), at
  most 16 options and 64 choices, one line each. The editor closes on Save, Cancel or Escape, never
  on focus leaving (Safari doesn't focus a clicked button).
- **"New" models.** A current model is badged `new` for a week from its release day
  (`ModelInfo.badge`, T3's `MODEL_NEW_BADGE_DURATION_MS` and `releasedAt` in
  `model-manifest.json`; Claude Haiku 5.5 from 2026-10-07), and the picker shows a small New chip
  after its name.
- **Machine limits** (agent-proto `settings_limits`): 64 variables, 64 arguments, 100 custom models
  per provider; paths absolute or `~/…`; a branch prefix git takes; auto-pull at least daily;
  subagents at most 8 deep, 1 to 32 tasks per thread; a git name and email both or neither.
- Changing them is the owner's (`Code::manages`); agent tokens can't (agents/policy.rs).
- **Agents from the ACP registry** are added and removed from the Providers section ("Add agent",
  and "Remove" on an installed agent's card): `registry.list`, `registry.install` and
  `registry.remove`, under "The ACP registry" in "Providers".

## Endpoints

A provider can talk to an endpoint instead of (or besides) its own login: CLIProxyAPI
(github.com/router-for-me/CLIProxyAPI), any Anthropic- or OpenAI-compatible gateway, or a key in
the provider's own variables (Gemini's `GEMINI_API_KEY`). The owner sets them from roll, for any
machine (laptops, servers and sandboxes), at any time. Wire: `agent-proto/src/endpoint.rs`; daemon:
`agent-host/src/endpoints.rs` and `orchestrator/endpoints.rs`; browser: `lib/agents-seal.ts`.

**The rule** (it replaced "API keys and base URLs come only from `config.toml` or a sandbox's
sealed env"): the owner may set endpoints and keys from roll, and a key is sealed end to end to the
daemon that uses it, so roll's relay only ever holds ciphertext, in memory, while forwarding it;
it never stores, logs or returns a key. Nothing is pooled or shared across owners: an endpoint is
the owner's own, set on the owner's own machines, and a sandbox is one owner's machine.

- **An endpoint** (`EndpointSpec`): an id (the client's, `[a-z0-9_-]`), a name, a kind, a base URL
  (`http(s)://`, no credentials in it), the providers that use it, and for `custom` the variables
  its URL and key go in. A provider uses at most one endpoint on a machine: setting one takes its
  providers from the machine's others. At most 32 per machine.
- **Kinds and what a provider's process gets** (`Provider::endpoint_use`, so every adapter, including
  new ones, gets the generic default and overrides it only when its CLI is configured otherwise):
  - `anthropic`: `ANTHROPIC_BASE_URL`, and the key as `ANTHROPIC_AUTH_TOKEN` (a bearer token, what
    gateways and CLIProxyAPI take; `ANTHROPIC_API_KEY` would make Claude Code ask before using it).
  - `openai`: the base URL includes `/v1`; `OPENAI_BASE_URL` and `OPENAI_API_KEY`.
  - `cliproxy`: one server with the Anthropic, OpenAI and Gemini routes at its root. Claude Code gets
    the Anthropic variables; Codex gets a model provider of roll's (below); other agents get all
    three families (`OPENAI_BASE_URL` = `<base>/v1`, `GOOGLE_GEMINI_BASE_URL`, `GEMINI_API_KEY`).
  - `custom`: the base URL in `url_env` and the key in `key_env`, whichever are named.
  - Codex: `-c model_provider="roll_<id>"` and `-c model_providers.roll_<id>.{name,base_url,
    wire_api="responses",requires_openai_auth=false,env_key="ROLL_ENDPOINT_KEY"}` before
    `app-server`, the key in `ROLL_ENDPOINT_KEY` (never in an argument: arguments show in `ps`).
  - A provider that can't speak the kind is refused (`invalid`): Claude Code with `openai`, Codex
    with `anthropic`.
  An endpoint's variables are laid over the provider's config (`config.toml`, then the machine's
  settings) for its next process, and reach nothing else: not terminals, records, `hello` or
  shares. A running session keeps what it started with.
- **Sealing** (`roll-seal-v1`): the daemon makes an X25519 key pair once (`agent.db`'s `settings`,
  `seal_key`) and says the public half in `hello` (`MachineInfo.seal_key`). The browser seals the
  key with WebCrypto: a fresh X25519 pair, HKDF-SHA256 over the shared secret (salt `epk ‖
  seal_key`, info `roll-seal-v1`) to an AES-256-GCM key, a random nonce, and `roll-endpoint:<id>`
  as associated data, so a box opens only on that daemon and only for that endpoint (`SealedSecret`).
  `endpoint.set {endpoint, key}` carries it (`key`: `keep`, `clear`, or `set {sealed}`). The relay
  checks the box's shape (`SealedSecret::check`) and the spec, and forwards the command; commands are
  never stored on the relay. The daemon opens the box (an all-zero shared secret is refused), keeps
  the key in `agent.db`, which it keeps at mode 0600, and replies; its receipt holds only the reply.
  Debug output of a sealed box or a key never shows it.
- **1Password** (T3's secret providers): instead of a sealed key, an endpoint may name a reference
  (`key_ref`, `op://vault/item/field`). The daemon reads it with the `op` CLI on the machine
  (30 s, its value only in memory) at start, when the endpoint is set and on each check. A reference
  isn't a secret, so it travels in the clear.
- **Reported, never the key**: `MachineConfig.endpoints` (`EndpointInfo`) has the spec, where the
  key comes from (`none`, `sealed`, `reference`), the models it listed and the last check's error.
  The UI shows "Key set (never shown)" with Replace and Remove; it never shows any part of a key.
- **Models**: after `endpoint.set`, at start for endpoints never listed, and on `endpoint.check`
  (which answers `endpoint_checked {ok, models, ms, error}`), the daemon asks the endpoint for its
  models: `<base>/v1/models` (anthropic, cliproxy) or `<base>/models` (openai), with the key as a
  bearer token (and `x-api-key`), 10 s, at most 2 MiB, no redirects, 500 models. Each provider that
  uses the endpoint offers them after its own, with `ModelInfo.via` = the endpoint's name (the picker
  shows it), and its status says "via <name>". A private or tailnet address is fine: the daemon calls
  it; roll's relay never calls an endpoint. Errors name the failure, never a key.
- **Profiles** (`EndpointProfile`, `code-control/src/agents/endpoints.rs`, `GET`/`PUT`/`DELETE
  /v1/code/agents/{owner}/endpoints[/{id}]`): the owner's endpoints kept on the relay without a key
  (one sent with a profile is dropped by the parser), at most 32, one of them the default for new
  sandboxes. Applying a profile to machines sends `endpoint.set` with the profile's spec (same id) to
  each, the key typed once and sealed to each machine separately in the browser; a machine shows which
  profile its endpoint came from by the id. A new sandbox has no daemon yet: the dialog applies the
  chosen profile once the sandbox's `hello` brings its seal key.
- **Who**: the owner (`Code::manages`); agent tokens can't set, remove or check endpoints, or change
  profiles (`agents/policy.rs`, `agents/endpoints.rs`); they can read profiles.

**In the dashboard** (`components/agents/endpoint-settings.tsx`): machine settings have an
Endpoints section (`MachineEndpoints`: each endpoint's kind, URL, providers, key state, models and
last check, with Check, Edit, Replace or Remove the key, and Remove after asking) and an Endpoint
row in each provider's card (`ProviderEndpointRow`: its own login, a compatible endpoint, or New
endpoint…; switching sends `endpoint.set` with `key: keep`). The editor (`EndpointEditor`) checks
the spec with `checkEndpoint` before sending and shows the daemon's `invalid`; a provider that can't
speak the kind is disabled with the reason; the key field is a password field, swapped for an
`op://` reference by "From 1Password", and emptied once sent. An old daemon (no seal key) or an
offline computer is read-only; an asleep sandbox stays editable (a command wakes it). Owner settings
list the profiles (`EndpointProfilesSection`: the editor without a key, "Default for new
sandboxes", and Apply to machines…, which takes the key once and seals it to each picked machine).
The new-sandbox dialog (`add-machine.tsx`) offers the profiles, the default picked, waits up to
three minutes for the sandbox's seal key ("Setting up CLIProxyAPI…", `applyWhenReady`), then
applies it; closed first, a quiet notice says it wasn't applied. The model picker tags endpoint
models "via <name>".

## Owner settings

What follows the user to every device (T3's client settings that roll has a use for) is one
document per owner, kept by roll beside the canvas's (`GET`/`PUT /v1/code/agents/{owner}/settings`,
`agents/docs.rs`): opaque JSON of at most 32 KiB, saved over a revision, and announced on the event
stream as `settings`, so open devices load it again. `lib/owner-settings.ts` is its schema (keys it
doesn't know are kept). Agent tokens read it but never change it. It opens beside the canvas
(`#agents/settings/owner`) from the agents header's gear and ⌘, (T3's desktop menu shortcut; on
every page), and links to each machine's settings.

**Every T3 setting, and where it lives in roll.** Machine: in the daemon (above). Owner: in the
owner's document. Project: `Project.settings` ("Project settings"). Not applicable: what roll's
dashboard or desktop app already decides, or features roll doesn't have.

| T3 setting | roll | Where |
|---|---|---|
| Providers: enabled, display name, binary path, home path (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`), launch args, environment | on/off, binary, extra arguments, environment (home paths are variables); names are the daemon's | machine |
| Provider secrets (API keys, `secretProviders`, sensitive env) | endpoints: keys sealed to the daemon from roll, or a 1Password reference read on the machine ("Endpoints"); `config.toml` stays the machine's own | machine (sealed) |
| Custom models (`customModels`), hidden models, model order | custom models; hiding and ordering: not applicable (the picker sorts favourites first) | machine |
| Claude's "Auto-compact after" (`autoCompactWindow`), idle compaction (`idleCompaction`, per instance over Providers > Advanced) | `auto_compact_window`, `idle_compaction` on the provider (Claude), over the machine's `idle_compaction` | machine |
| Favourite models (`favorites`) and roll's recent models | `favorite_models`, `recent_models` (localStorage is the cache; `agents-models.ts`) | owner |
| Default model (`defaultModelSelection`), permissions (`defaultRuntimeMode`), workspace (`defaultThreadEnvMode`), plan mode | new-thread defaults: project, then machine, then owner (`newThreadDefaults`) | project, machine, owner |
| Worktrees: `worktreesDirectory`, `branchNamePrefix` | worktree folder, branch prefix | machine |
| `branchNamingMode`, `newWorktreesStartFromOrigin`, `worktreeSubmodules` | not applicable (branches are `<prefix>/<title>`, from the checkout's HEAD) | — |
| `worktreeCleanup`, `storageCleanup.worktreeSettledAfterDays`, `worktreeKeepWhen` | `ProjectSettings.cleanup` (`worktree_after_days`, `keep`: uncommitted changes or any local files), "Clean up now" (`project.cleanup`) ("Settling") | project |
| `storageCleanup.worktreeAfterDays`, `worktreeOnMerge`, `worktreeUnchanged`, `worktreeOnDelete`, browser artifacts, rotated logs | not applicable (an inactive or merged thread settles and then its settled rule applies; delete removes a clean worktree; no browser artifacts or rotated logs) | — |
| Git identity for commits | `git_name`, `git_email` for publishing | machine |
| `defaultAutoPull`, `automaticGitFetchInterval` | auto-pull per project; how often it fetches (`pull_every`) | project, machine |
| Add project starts in (`addProjectBaseDirectory`) | the clone folder (`clone_dir`); folders start at the machine's home | machine |
| Delegation (T3's orchestrator limits) | subagent depth, tasks per thread | machine |
| `notificationMode`, `inAppNotificationsEnabled` | notifications: off, notifications, sound, both (the browser; the desktop app notifies itself) | owner |
| `confirmThreadDelete` | confirm before deleting a thread | owner |
| `confirmThreadArchive`, `confirmThreadUnpin`, `confirmQuit` | not applicable (archive is undoable from its toast; no pins; the desktop app quits from its tray) | — |
| `sendShortcut` | send with Enter or ⌘/Ctrl+Enter | owner |
| `followUpBehavior` | while a run is going: queue or steer | owner |
| Keybindings | the shortcuts, listed (not rebindable) | owner (read-only) |
| Theme, contrast, glass, fonts, chat width, word wrap, timestamps, effort display, math | not applicable: the dashboard's theme and type | — |
| Auto-settle after days, on merge, keep PRs (`sidebarAutoSettleAfterDays`, `sidebarAutoSettleOnMerge`, `sidebarAutoSettleKeepPullRequests`) | `ProjectSettings.auto_settle` (`after_days`, `on_land`, `keep_changes`) ("Settling") | project |
| Snooze presets, sidebar grouping and order, working shelf | T3's snooze presets as they are; the threads sidebar groups by project, newest first | — |
| Text generation model, thread title instructions, response streaming | not applicable (titles come from the first message; streaming is always on) | — |
| Diffs & PRs (layout, whitespace, colours, open in app), source control writing style, review guides | not applicable yet (roll code's review is its own) | — |
| Browser, devices, SnapShots, scheduled tasks, environments, sharing, labs, maintenance, update checks | not applicable (no browser or device tools; sharing is `shares`; the CLI updates itself) | — |
| Rename, remove, sandbox size/start/stop/delete, provider logins | in the machine's panel (still on its card) | machine |
| Project icon (`projectIcon`: lucide, emoji, monogram) and `faviconPath` | colour, and icon: logo, monogram, emoji or a picture in the project ("Identity"); no lucide icons (the logo or monogram reads better at 14px) | project |
| — (roll's own) | a machine's colour and kind ("Identity") | machine (on the relay) |

**Search** is T3's `settingsSearch.ts` (`lib/settings-search.ts`): every word of the query in a
row's title, section or search terms (accents and case ignored), the title ranked first; matching
rows stay in their sections, a provider's card opens on a match inside it, and `/` or ⌘F focus it.

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
volume. An endpoint set on the sandbox ("Endpoints"), sealed to its daemon like any machine's, or
provider API keys through the sandbox's env at creation, are the alternatives. roll never reads,
copies or pools these credentials. A subscription login on a machine roll hosts is still the owner's own login
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
  on you, keep still. Cards stay small: a thread's card is T3's thread row as a card
  (`thread-status.tsx`): the project's icon and name and the machine it runs on ("Identity"), T3's status
  pill (pending approval, awaiting input, plan ready, working, connecting, failed, completed; the
  card loads no runs, so "plan ready" for a plan a run ended with, Codex's or ACP's, comes from the
  thread's `plan_ready`, T3's `hasActionableProposedPlan`, set when a run completes with a plan
  and cleared by the next run or a rollback), the
  title and what it's doing, the PR and work item it's attached to, its worktree and running
  terminals, and what it cost; a run that finished since it was last opened turns its border
  green (T3's completed tone) until it's opened; a project's card sums it up like T3's project view (threads, running and
  waiting, the PRs its threads made, the agents working in it); actions on machine and project
  cards show on hover (always on touch screens). The end of a thread card's footer and the
  conversation's composer footer show its context window as a small ring (T3's
  `ContextWindowMeter`; "62% of 200k" on hover, tokens only when the window is unknown),
  monochrome until it's more than 90 % full, then accent. Every request is answered right on its
  card, without opening the conversation (`pending-request.tsx`): the thread's name (it opens the
  conversation), the last three steps that led to it in the work log's rows (the earlier ones a
  click away), then the request itself: an approval with Approve and Decline (a note back to the
  agent) and the rest under "More options", a plan with the plan itself (folded to its first lines, "Show the whole plan") above Implement
  and Refine, questions one at a
  time as T3's user-input panel (radio or check rows, 1–9 pick, one choice moves on after 200 ms,
  your own words in a line, Previous / Next question / Submit). A phone's canvas shows the oldest
  the same way. The canvas grows one of two ways, chosen per view
  (`lib/agents-layout.ts`), and cards never overlap either way:
  - **Left to right** (the default on a wide screen): a tidy tree in columns, one per depth, as
    wide as its widest card; machines on the left, then projects, threads and their satellites.
  - **Top-down** (the default on a phone): a board, like Linear's. The machines sit in a row on
    top, each spreading its projects side by side under it in a row as tall as its tallest card;
    everything in a project is a list down that project's column, indented, each thread's
    requests, subagents and forks under it before the next thread. A plain top-down tree would put
    a project's threads side by side, and a busy project would run off a phone's screen sideways;
    as a column a narrow screen reads it top to bottom, and a wide one reads a column per project.
    Edges route for it: a machine's into the top of each project, a listed card's down a trunk
    inside its parent's left edge and into its left side (`canvas-engine.ts`, `EdgeRoute`).
  The toolbar's direction button, ⇧D and ⌘K ("Lay out top-down", "Lay out left to right") switch
  it. It's kept with the view in the owner's canvas document (`display`), a phone's choice apart
  from a wide screen's, so turning the laptop's canvas around doesn't turn the phone's. Dragging a
  card moves it among its siblings along the axis they're laid out on (they make room as it
  passes; ⌥↑/⌥↓ do the same from the keyboard, ⌥←/⌥→ for siblings side by side); that order is
  remembered per owner and synced through roll, new work keeps its default place (newest first),
  and the view is remembered per device.

  **Working from the keyboard** (`nextCard` in `lib/agents-layout.ts`). The canvas is where an
  owner triages many agents, and Linear and T3 both move by keyboard faster than by pointer, so
  every card can be reached and answered without the mouse. A card takes focus from Tab or a
  click; then the arrows go to the nearest card that way, in line first and the first in reading
  order of those as near. That follows the tree either way it grows: left to right, ← is the
  parent, → a child and ↑/↓ the next card in the column (across parents); top-down, ↑/↓ walk a
  project's column and ←/→ go to the next column. Enter opens what's focused (a thread's
  conversation, a machine's or project's settings; on a request or a pinned card it goes into the
  card). Space goes to the request waiting on that card, else the next one waiting anywhere in
  reading order, with its answer focused (a question's options, or Approve / Implement), so a run
  of approvals is Space, Enter, Space, Enter, without opening a conversation. With nothing focused
  the arrows still pan.

  **Focusing a project** (`focusOn` in `lib/agents-views.ts`). With several projects the canvas is
  busy, and most of the time one project is what's being worked on. F on a card (or "Focus" in a
  project card's menu, or ⌘K "Focus a project…") fills the canvas with that card's project: the
  project, its machine above it and everything under it, framed again. A chip in the views bar
  names it; its × or Escape (before Escape closes anything else) shows every project again. It's
  per device and on top of the view, so it never changes what a view picks.

  **Display** (Linear's display options; `DisplayMenu` in `views.tsx`, `applyDisplay` in
  `lib/agents-views.ts`). Each view has its own, kept with it in the canvas document, in a
  popover beside the filters:
  - **Layout:** across or down (this screen's; a phone keeps its own), as above.
  - **Density:** comfortable, or compact. Compact folds each card to its name, status and what
    it's doing (the rest is `data-detail` in `nodes.tsx`) and roughly halves the gaps, so about
    twice as much work fits a screen when there's a lot running.
  - **Grouping:** by machine (machines, then their projects), or by project (the projects are the
    anchors, by name, and the machine cards go; the threads already say which machine they're on).
    One machine is the common case, and then its card is a level that carries nothing.
  - **Show:** idle threads (not working, not waiting, nothing unseen; an idle thread stays while
    a fork under it works, or while a card of it is pinned) and terminals. Leaving idle threads
    off makes the canvas the work in flight, the thing an owner checks most. **Projects without
    threads** (off by default) gives every project a card again.
  - "Reset to default" when anything differs (projects pinned onto the view stay pinned).

  **Projects without a card** (`collapseUnused` in `lib/agents-graph.ts`, `machine-projects.tsx`).
  A project with nothing on the canvas in this view (no thread, terminal or pin, after the
  filters and the display) isn't drawn. Its machine says so in one line, "3 more projects" (or
  "3 projects" when none of its projects has a card) with their tiles stacked, which opens on
  hover (250 ms) or a click into a list beside the card, scaling from the line: a row starts an
  agent there, its pin keeps the project's card on this view (`Display.keep`, unpinned from the
  card's ⋯ menu), its gear opens the project's settings, and "Add a project" is at the foot. A
  project gets its card back with its first thread; projects focused or named by the view's
  filter always keep theirs, and so do projects grouped as the anchors. Focusing a folded project
  (a deep link, a palette pick) frames its machine. Linear folds empty groups the same way; T3's
  sidebar shows every project but only those with threads take room.

  One canvas engine (canvas 2D + `requestAnimationFrame`, paused offscreen) draws edges,
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
- **A conversation.** Clicking a thread opens it in a panel docked beside the canvas (resizable, remembered; on a phone it covers the canvas), its card selected on the canvas. A machine's or project's settings open in the same panel (`#agents/settings/<key>`, from its card), and so do the owner's (`#agents/settings/owner`, the header's gear or ⌘,; "Machine settings", "Owner settings"). Finished subagents (a done provider subagent, a delegated thread that's done) leave the canvas; their results stay in the parent's transcript. The conversation is T3's chat
  view (`session.tsx`, `transcript.tsx`, `work-log.tsx`, `composer.tsx`; the grouping is
  `lib/agents-worklog.ts`): a header that is a breadcrumb (the project, the title, T3's status
  pill) with Diff, Terminal and Share and the rest under More; the transcript in a 46 rem column
  (the user's turn a bubble on the right, the agent's words bare, its steps a calm work log: a
  group of tool calls and thinking reads as one sentence, "Ran 3 commands and read 2 files", a
  live one shows the step it's on, and a finished run folds into "Worked for 2m 13s" above its
  answer, with the files it changed in a card and copy, fork and roll back under it); approvals as
  rows in the work log; the composer under it (T3's: a message to a running agent steers it,
  ⌘/Ctrl queues it, the send button says which; the agent's questions sit on top of the composer
  and what's typed is your own answer; queued messages, a ready plan, compaction and subagents
  as in "Turns") with the machine, branch and cost in a strip below; the
  thread's terminals in a resizable drawer under it all (while it's open they're only there, not
  cards on the canvas too); the diff (of the thread or one run)
  beside the chat when the panel is wide enough, else sliding over it. Everything is managed from
  there: fork, roll
  back, publish, share, rename, archive, delete, take a work item. The title is a switcher to the
  other conversations (the ones waiting on you first; type to find one, ↑↓ and Enter), so there's no need to go back to the
  canvas; Escape (or ←) goes back, after closing what's open inside it first (the diff or a
  terminal beside it, a confirm, a form; the newest first). The canvas stays mounted underneath, so going back is instant.
- **A new agent: the launcher** (`new-agent.tsx`). A small composer pops over the canvas (the
  canvas stays live under it): the machine and project as pickers (the asked-for one, else the
  last used; adding a project is right there), the model picker, permissions, plan and worktree
  from the project's defaults. Enter starts it: the popup closes and the new thread opens in the
  side panel, selected on the canvas. It opens from "New agent" (the surface's header and the
  header pill), ⌘N or ⌘⇧O (T3's `chat.new` bindings; not in a terminal), `#agents/new`, the + on
  a machine card (preset to that machine) or a project card (preset to that project). **Drag to
  launch:** the same + is a handle: dragged out of the card it draws a calm dashed accent line
  to the pointer ("New agent here"), and dropped on empty canvas it opens the launcher there for
  that project or machine (on a card nothing starts; Escape cancels). A thread card's fork
  handle does the same with `thread.fork` (T3's fork from the latest point): click or drop, and
  the fork opens beside the canvas. The canvas keeps its own card drag (reordering siblings);
  launch handles are buttons, so the two never mix. The line has no animation, so reduced
  motion changes nothing.
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
  `Select`, `Menu`, `Popover`, `Combobox`, `Switch` and `Segmented`. The small action menus (`menu.tsx`: a
  view tab's, More, ⋯, cards') are that `Menu` too, portalled, so nothing clips them. Popups float in a
  translucent panel with a soft shadow, grow out of their trigger (scale 0.98 and a fade, 150 ms)
  and honour reduced motion; highlights are a neutral fill and only an "on" state is accent.
- **The command palette** (T3's CommandPalette; `components/agents/command-palette.tsx`, its
  logic in `command-palette.logic.ts`, T3's ranking in `lib/palette-search.ts`). ⌘K (Ctrl+K)
  while the agents are showing, in the popup, at `/dashboard/agents` and in the desktop app's
  Agents window, or the search button in their bar. It comes before the dashboard's own ⌘K ("Ask
  roll"), which stays on every page while the agents are closed (⌘J opens them, then ⌘K); a
  terminal keeps Ctrl+K. Before typing: actions ("Orchestrate…", new agent, new agent in a
  project, what's waiting, stop, fork or archive the open conversation, add a project right in
  the palette, add a machine, project and machine settings, archived conversations, fit the
  canvas) and recent conversations. Typing searches all of that, every submenu's options ("New
  agent in web"), conversations, projects and machines, ranked like T3 (word starts, joined
  words, typos only when nothing matches, recent use and what was picked for a query, kept per
  device); `>` searches actions only. A conversation opens beside the canvas; a project or
  machine is shown on the canvas. ↑↓, Enter, Backspace to go back, Esc.
- **Prompting from outside.** Anything typed in the palette is also a goal: "Orchestrate: …"
  starts an orchestrator (docs above, "Delegation") that plans it and spins up agents, which
  appear on the canvas under it as they start and leave it as they finish; "New agent: …" starts
  one agent with it as its prompt. Both go to the last project used (on a machine that can take
  it; "Somewhere else…" picks another), with its new-thread defaults and full access unless they
  say otherwise, and the conversation opens beside the canvas. They lead the list when the query
  names nothing (no row has every word typed at a word start: a sentence, a typo), else they come
  last. The same goal can be typed in the prompt bar at the bottom of the canvas ("What should
  the agents get done?", Orchestrate or One agent).
- **The prompt bar** (`prompt-bar.tsx`) and the launcher (`new-agent.tsx`) share one set of rules
  (`new-thread.ts`): the default project is the last one a thread was started in, else the one
  with the newest thread, preferring machines that can take one; the model and modes are the
  project's, then the machine's, then the owner's new-thread defaults. Once something is drafted
  the bar pins its project: thread updates never move it, only its project chip (a picker of
  every project, "Add a project…" last) does, and an offline machine says so instead of the draft
  moving elsewhere. The draft (text, Orchestrate or One agent, project, model) is kept per owner
  in this browser, so it survives the conversation that opens on Enter, a reload and the phone's
  panels; after a start the text goes and the kind and model stay (T3's sticky model). A start's
  command id is kept with the draft: sending again after a timeout replays instead of starting a
  second one, while a refusal (shown whole, above the toolbar) gets a new id. Starting… can be
  cancelled (Esc). The bar stays while a conversation is open beside the canvas: centred over a
  wide canvas, across a narrow one above the zoom controls, and on a phone under "Waiting for you".
  Fitting, focusing and revealing cards keep them above it. Keyboard: Enter starts, Shift+Enter is
  a new line, arrows switch Orchestrate and One agent, Esc leaves. It opens on focus or a press
  and closes on a press outside it and its popups, Esc, or focus moving to something else, never
  on a blur alone (Safari and Firefox on macOS don't focus a clicked button, so using its pickers
  there used to close it). The pill and the open bar are one shape: the text stays put while the
  toolbar grows in under it (docs/code/agents-design.md, "Motion").

- **Getting started.** An owner with no machines sees the globe (`RollMark`) and the two ways in
  side by side, the same as "Add machine" on the canvas: connect your computer (install,
  `roll agent login`, `roll agent up`) or rent a sandbox (sizes and prices from
  `GET …/sandboxes`, optional provider keys). A new sandbox is on the canvas at once and pulses
  while it boots. A sandbox's card shows its size, status, price and this month's usage, with
  start, stop and delete, and puts logging a provider in first (a terminal on the canvas running
  the device login). The daemon probes its providers again whenever a terminal exits while one is
  logged out, so a finished login moves the card on by itself ("Check again" is there for a login
  done elsewhere). A machine with no projects offers adding one (a folder, a clone of one of the
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
  start in a worktree (a repository with no commits yet has nothing to branch from: its threads work
  in the project folder until there's a commit).
- **Phones.** Below 640 px the popup is the whole screen; a conversation's actions fold into its
  menu and the diff or a terminal takes the transcript's place; the oldest waiting request sits at
  the bottom of the canvas, answerable with one tap (it folds to one line; the zoom bar moves to the
  top while it shows); a tapped card shows its tools at a finger's size; fitting the canvas keeps cards readable (at
  least 55 %) and starts on the first machine (centred on it when the top left wouldn't show it);
  the canvas grows top-down unless the owner turned it on a phone.
- **Remembered on every device.** How the owner arranged the canvas is kept by roll
  (`GET`/`PUT /v1/code/agents/{owner}/canvas`; `agents/docs.rs`), so the laptop, the phone and
  the desktop app show the same arrangement. The document is the canvas's own (at most 64 KiB of
  JSON) and opaque to roll; the dashboard reaches it through `AgentsServices.canvas`. It is
  `{ order, pins?, display?, views? }`: `order`, `pins` and `display` are All's, `views` the
  owner's views (`{ id, name, filter, order?, pins?, display? }`, at most 16). `display` is how a
  view is shown (`direction`, `phoneDirection`, `density`, `group`, `hide`; absent means the
  default). A document from before
  views (`{ order }`) still parses and shows the built-in views, one without `display` the
  defaults; `views: []` means the owner deleted them all. Saves
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
