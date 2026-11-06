# roll code automations

Automations run the owner's agents (and talk to the world) when something happens: a schedule, a
webhook, a change landing, an agent finishing, a Linear issue, a Slack message, a button. An
automation is a **trigger**, **filters** on the event it brings, and **steps**. The user's words:
"I want everything to be possible": start an agent on a specific machine, in its own sandbox, on any
machine that has the project; wait for it, answer it, publish its work and queue it to land; call
anything over HTTP; tell the owner.

Code: `crates/code-control/src/agents/automations/`. Wire types: `agent_proto::automation`
(`crates/agent-proto/src/automation.rs`), mirrored in `apps/dashboard/src/lib/automations-api.ts`; both
parse `crates/agent-proto/fixtures/automation*.json`. Routes: docs/code/http-api.md, "Automations".
The dashboard's Automations view (list, builder, runs, notices): docs/code/automations-ui.md.

## Shape

```
 trigger sources                   inbox                    engine                       steps
 manual / schedule / interval  ─┐                       ┌─ match: trigger, filters ─┐
 webhook (signed URL)           ├─► automation_events ──┤  one run per automation   ├─► agent.start / wait /
 roll code (outbox, land log)   │   (key: unique)       │  and event, same write    │   message / respond /
 agents (the relay's mirror)    │                       └─ runs ─► steps, retries ──┘   publish / script /
 integrations (registry)       ─┘                          claims, backoff, resume      settle, webhook, notify,
                                                                                        delay, integrations'
```

## Durability

The rules are code-control's (AGENTS.md): a read-then-write is one `db.write` closure that only
touches the database; nothing that must not miss an event is driven by a broadcast channel.

- **Inbox.** Every event a trigger sees is recorded in `automation_events` with an idempotency key,
  `<source>:<the event's own id>`, unique per owner, *before* anything runs: in its own transaction, or
  in the transaction of the state change that caused it. A redelivered event (the same key) is
  recorded once, so it runs nothing twice.
  - roll code events come from the same durable places webhooks do (`webhooks.rs`): an event's
    inbox rows are written by the same write that queues its webhook deliveries (and a promised
    event's outbox row, `webhook_outbox`, counts automations as listeners, so a node dying between a
    state change and its event loses nothing), and landings come from the store's land log in the
    same transaction that moves the forwarder's cursor. Keys: `code:<outbox row>`, `code:landed:<seq>`.
  - agent events are written by the relay's mirror (`agents/store.rs::mirror`), in the transaction
    that stores the daemon's records, when a record changes the way a trigger listens for (a run
    completing, a thread starting to wait, …). Keys carry the record's `seq`, so a daemon's replay
    after a reconnect records nothing twice.
  - schedules and intervals fire by a compare-and-swap on the automation's `next_at` in the write
    that records the tick (`schedule:<automation>:<due ms>`): two nodes never fire one slot twice,
    and a slot missed while every node was down fires once when one comes back (not once per
    missed slot).
  - inbound webhooks record before answering 202; manual runs record before answering.
- **Matching.** The engine takes new events in order; one write marks an event done and makes its
  runs (`UNIQUE (automation_id, event_id)`): the trigger takes it (source, kinds, the integration's
  own `matches`), and the filters hold. An event that matches nothing is just done.
- **Runs** are records (`automation_runs`): status, steps (status, attempts, output, log), the
  threads they started, and each step's private state. A node works on a run under a **claim** (a
  random value and an expiry, set by compare-and-swap, renewed while a step runs); every write of the
  run checks the claim, so two nodes never work on one run at once, and a node that dies leaves the
  run to whoever claims it after the expiry. A step attempt cut short runs again: steps are
  idempotent (below).
- **Waiting** doesn't hold a task: a step that waits (an agent working, a delay, a retry's backoff)
  stores its state, sets the run's `next_at` and lets go of the claim. Events about a thread a run
  started (`automation_threads`) move the run's `next_at` to now in the mirror's own transaction.
- **Placement.** An owner's runs are worked on by the node holding the owner's agents lease
  (`agents:<owner id>`, docs/code/cluster.md), where its daemons' links and event streams are, so
  commands go out locally and run events reach live streams at once; claims keep it correct
  anywhere. A node that sees work for an owner it doesn't serve kicks the owner's lease holder (the
  lease is taken if nobody holds it); the lease isn't let go as idle while the owner has enabled
  schedules or runs going on.

## Steps act through the API

A step acts as its automation: through roll's own API (`StepContext::api`), with a token made for that
one call (`rlr_…`, stored as sha256 in `automation_tokens`, ten minutes at most, dropped after the
call, looked up on every request like every token). It is the owner (`automation:<id>` as the
token's name), or `agent:<run_as>` when the automation says so, with an agent's narrower rights. So
waking a sleeping sandbox, git grants, the agent policy, landing rules and cluster routing are the
API's own, never copied.

Daemon commands are idempotent by their id: a step's ids are made from the run and the step
(`auto_<run>_<step>_<part>`), so an attempt that runs again sends the same command, and the daemon
answers it from its receipt instead of running it twice.

## Triggers

| Trigger | Events (`kind`) | Data |
| --- | --- | --- |
| `manual` | `run` | `{by, input}` |
| `schedule {cron, timezone}` | `tick` | `{at, scheduled}` |
| `interval {every_s}` | `tick` | `{at, scheduled}` |
| `webhook` | `post` | `{body, headers}` |
| `code {events, repos}` | the webhook events | the webhook payload's `data` (a work item's `number`, `title`, `labels`, …; a check's `{change, check}`), plus `repo` and `repo_id` |
| `agent {events}` | `thread.created`, `thread.done`, `thread.failed`, `thread.waiting`, `thread.settled`, `request.opened` | `{machine, project, thread, run?, request?}`: `thread` is the thread's brief on every event (`thread.id`, `thread.title`, `thread.status`, `thread.provider`, `thread.change`, …; only `thread.id` if the relay hasn't seen the thread), `run` the run (`run.status`, `run.error`, `run.prompt`) on `thread.done` and `thread.failed` and `{id}` on `request.opened`, `request` the request (`request.body.action`, `request.body.command`) |
| `integration {source, events, config}` | the integration's | the integration's |

## Templates

Strings in step configs are templates: `{{ event.data.change.title }}`, `{{ steps.start.thread }}`,
`{{ run.id }}`, `{{ automation.name }}`, `{{ owner }}`, with pipes (`default:"x"`, `upper`, `lower`,
`trim`, `truncate:N`, `json`, `url`, `join:", "`, `first`, `last`, `length`, `lines:N`). Logic-less: no
conditionals, loops or code; what one renders to is at most 64 KiB. `{{ secrets.<name> }}` only works
in a webhook step's header values (refused at save time anywhere else), so a secret never ends up in a
prompt, a body or a log. Code: `template.rs`.

## Filters

`{field, op, value, not?}` on the run's context (`event.…`, `steps.<id>.…`): `equals` (a number and its
string are equal), `contains` (case-insensitive; a list: has it), `any_of`, `has_label` (strings or
`{name}` objects), `glob` (roll code's path patterns: `*` within a segment, `**` only as a whole
segment, never recursive), `exists`, `gt`, `lt`. An automation's filters decide whether an event
starts a run; a step's `when` decides whether the step runs. Code: `filter.rs`.

## Integrations

Integrations plug in through `registry.rs`, without the engine knowing them:

- a `TriggerSource` (its catalog entry, a save-time `check` of the trigger's config for the owner,
  `matches(config, event)`), registered with `code.automations.registry.register_trigger`; its events
  go into the inbox with `Code::automation_event(owner_id, NewEvent {source, kind, id, data, summary,
  automation})`, or `registry::record_event(conn, …)` inside the integration's own write;
- a `StepKind` (its catalog entry with the JSON Schema of its config, a save-time `check`,
  `secrets_at`, default retry and timeout, `run(&mut StepContext) -> StepOutcome`, and `cleanup` when
  the run ends), registered with `register_step`. `StepContext` gives the run's context and
  templates, the step's config, the automation's sealed secrets, an SSRF-guarded HTTP client
  (`review::apps::PublicOnly`, no redirects, the literal-host check, 30 s, answers read up to 1 MiB),
  the API as the automation, durable state between attempts and waits, a log, and
  `idempotency_key()` for making side effects happen once. Outcomes: `Done(output)`,
  `Wait {after}`, `Retry(why)` (transient: backoff, then fail), `Fail(why)`.

An integration's own credentials (OAuth tokens, API keys) are its own to keep, sealed with
`ROLL_SECRET_KEY` and never returned or logged; an automation's secrets (`PUT …/secrets/{name}`) are
for steps that call out (`webhook`). Provider credentials are never touched (docs/code/agents.md).

## Steps

| Kind | Does | Output |
| --- | --- | --- |
| `agent.start` | starts a thread. Targets: `machine` (id or name), `sandbox` (a new one of a size, deleted when the run ends unless `keep`), `any_online` (a connected machine with the project; a repo project is cloned onto one if none has it), `project_machine`. The project is `id` or `repo` (cloned with `project.add {clone}` when the machine has none). Every `thread.create` field: provider, model, effort, runtime and interaction mode, worktree, title, prompt, orchestrator. Unset fields come from the automation's `defaults`, then the project's. | `machine`, `project`, `thread`, `sandbox` |
| `agent.wait` | until the thread is `done`, `failed`, `waiting` (on you) or `settled`, at most `timeout_s` (`on_timeout: continue` makes a timeout an outcome) | `outcome`, `thread`, `run`, `status`, `error`, `message` (the agent's last text), `change` |
| `agent.message` | `message.send`, queue or steer | `thread`, `run` |
| `agent.respond` | answers pending approvals by rules (action, command glob, files glob, tool glob → decision; else `otherwise`) and questions (answers by id or header, or the first option); the rest are left for the owner | `answered`, `left` |
| `agent.publish` | `thread.publish`, `land` sends it to landing (the merge queue on `queue` repos) | `branch`, `commit`, `change`, `url`, `landing` |
| `agent.script` | runs a project script in a terminal (the thread's worktree, or the project's checkout) | `terminal`, `machine` |
| `agent.settle` | settle, archive or un-settle | `thread`, `action` |
| `webhook` | an HTTP request to a public address (`review::apps::PublicOnly`, no redirects, the literal-host check, 30 s, answers read up to 1 MiB); signed with one of the automation's secrets like roll's own webhooks; `X-Roll-Delivery` and `Idempotency-Key` are the step's idempotency key, the same on every attempt. 5xx, 429, 408 and network errors are retried, other statuses fail. It is a step, not a delivery queue: its answer feeds later steps, and the engine's retries are its redelivery. | `status`, `headers`, `body` |
| `notify` | a `notice` on the owner's stream (a toast on every open device), kept for later (the newest 200 per owner, `GET …/automations/notices`) | `id` |
| `delay` | waits, durably | — |

A config field that is `null` is unset, the same as leaving it out: it's dropped as the automation is
parsed (`agent_proto::automation`), so every kind's defaults apply (only the config's own fields: a
`null` deeper in, in a webhook's JSON body, is data). The builder leaves unset fields out.

Steps without a thread say which by `thread` (a template); unset, the run's latest thread. A step's
`when` filters decide whether it runs; `on_error: continue` lets the run go on (later steps see
`steps.<id>.error`); `retry {attempts, backoff_s}` (×2 each time, an hour at most) and `timeout_s`
override the kind's own.

When a run ends (succeeded, failed or cancelled), each step that kept state gets its `cleanup`
(`agent.start` deletes a sandbox it made unless `keep`). A run cancelled while a step is in the
middle of an attempt stops at that step's next write; what the attempt did is kept.

## Runs

- **Concurrency.** `parallel` (at most 20 at once, the rest queue), `skip` (recorded as `skipped`),
  `queue` (one at a time, in order), `cancel_previous` (the runs going on are cancelled first).
- **Rate limit.** At most `max` runs start per `per_s` seconds (unset: 100 an hour); past it, runs
  are recorded as `skipped` with the reason. It also stops loops (an automation started by agent
  events that starts agents).
- **By hand.** `POST …/run` makes a `manual` event for the automation (`event.data.input`,
  `event.data.by`), whatever its trigger; its filters still apply (a run they refuse is `skipped`,
  so the owner sees why). The same `key` is the same run.
- **Cancel and retry.** Cancelling stops a run and cleans up; retrying a failed or cancelled run
  goes on from the step that stopped it, its attempts starting over, earlier steps kept.
- **Waits** park the run (`waiting`, `next_at`): records of a thread the run started or follows wake
  it at once, in the mirror's own write; `next_at` is the safety net.
- **Stream.** Every change of a run is an `automation_run` event (the whole run, with a cursor);
  automations are `automation` and `automation_removed` events. An automation's `last_run` is
  updated without an event, so live status comes from `automation_run`.

## The catalog

`GET …/automations/catalog` lists every trigger source and step kind with JSON Schemas, for the
builder. Configs are `{type: "object", properties, required}`, in display order; a string's
`format` picks its widget: `template` (one line), `template-long` (prompts, bodies), `roll-thread`,
`roll-target`, `roll-project`, `roll-model`, `roll-secret`. A trigger's `events[].fields` are the
`event.data` paths worth filtering on and `sample` a realistic `event.data`, with the fields a real
event of that kind has (the agent events' samples are made by the code that makes the events, and
tests compare a real work item's and a real `thread.done`'s data with theirs); a step's `output` its
`steps.<id>.…` fields. The dashboard keeps a copy of the built-in entries
(`apps/dashboard/src/lib/automation-catalog.json`, `ROLL_FIXTURES=write cargo test -p code-control
--test automations`): its templates are tested against these samples, so a template that reads a
field its event doesn't have fails.

## For integrations

Besides the traits (above):

- `registry::wake_run(conn, run_id)`: have a run looked at now, inside the integration's own
  write (a reply that the run's waiting step handles); `Code::wake_automations(owner)` after the
  commit.
- `registry::listens(conn, owner, source, kind)`: whether an enabled automation listens.
- `StepContext::command(machine, part, Command)`: a daemon command as the automation, with an
  idempotent id (`part` unique per side effect); `follow_thread(machine, thread)`: be woken by a
  thread the run didn't start.
- `Code::automation_api(owner, automation, run_as, method, path, body)`: the API as an automation
  outside a step (a Slack button answering a request).
- `Code::create_automation(owner_id, spec, by)`: an integration's built-in automation, with the
  route's checks.
- Register in `automations::register_integrations` (one line per integration), called when the
  engine starts on a node.

Slack, Discord and GitHub are integrations of this kind: docs/code/chatops.md
(`crates/code-control/src/chatops/`).

## Surfaces

- Routes: docs/code/http-api.md, "Automations"; the auth matrix in `routes.rs`.
- `roll agent automation --owner <owner> …` (an owner token in `--token` or ROLL_CODE_TOKEN):
  `list`, `get`, `create`, `update`, `delete`, `enable`, `disable`, `run [--input] [--key]
  [--wait]`, `runs`, `show`, `cancel`, `retry`, `set-secret` (the value from stdin),
  `remove-secret`, `rotate-webhook-secret`, `catalog`, `test`.
- MCP tools, mapped onto the routes: `list_agent_automations`, `save_agent_automation`,
  `run_agent_automation`, `list_agent_automation_runs`, `get_agent_automation_run`,
  `cancel_agent_automation_run`.
- The dashboard: `lib/automations-api.ts` (types, decoders, client; `createAgentsApi().automations`)
  and `lib/automations-demo.ts` (the demo's automations and runs, in memory).

Tests: docs/code/test-coverage.md, "Automations".


## Linear

Code: `crates/code-control/src/agents/linear/` (connection and tokens `client.rs`, `store.rs`;
webhooks `webhook.rs`; the trigger `trigger.rs`; steps `steps.rs`; the agent-session bridge
`session.rs`; the built-in automation `templates.rs`; routes `api.rs`, docs/code/http-api.md
"Linear"). Dashboard: the Linear card in the owner settings (`components/agents/linear-settings.tsx`,
`lib/linear-api.ts`) and the template gallery (`lib/linear-templates.ts`, listed in `TEMPLATES` in `lib/automation-templates.ts`). Tests: `tests/linear.rs`
against a mock of Linear's GraphQL and OAuth endpoints (docs/code/test-coverage.md, "Linear").

### What Linear offers (researched 2026-11-04)

- **GraphQL** at `https://api.linear.app/graphql` ([docs](https://linear.app/developers/graphql)). A
  personal API key goes in `Authorization` as is; an OAuth token as `Bearer`. Errors come in `errors`
  with HTTP 200 too; a rate limit is HTTP 429 with `Retry-After` and `extensions.code: RATELIMITED`
  ([rate limiting](https://linear.app/developers/rate-limiting)). Creates take a client `id`, so
  roll makes them idempotent with an id derived from the step's idempotency key.
- **OAuth 2.0** ([docs](https://linear.app/developers/oauth-2-0-authentication),
  [actor](https://linear.app/developers/oauth-actor-authorization)): authorize at
  `https://linear.app/oauth/authorize` (comma-separated `scope`, `state`, PKCE, `prompt=consent`,
  `actor=app` so the app acts as its own user), tokens at `POST https://api.linear.app/oauth/token`
  (form-encoded only). Access tokens last 24 hours and come with a refresh token (every app since
  2026-04-01); a refresh returns both again, and the same refresh may be replayed for 30 minutes.
  Revoke at `POST /oauth/revoke`.
- **Webhooks** ([docs](https://linear.app/developers/webhooks)): `Linear-Signature` is hex
  HMAC-SHA256 of the raw body under the signing secret; the body's `webhookTimestamp` (ms, inside
  what's signed) should be within a minute. Headers `Linear-Delivery`, `Linear-Event`. A delivery
  must be answered within 5 s, and is retried after 1 min, 1 h and 6 h. An OAuth app's webhook is
  configured once on the app (one signing secret) and created in every workspace that installs it;
  `organizationId` says which. `webhookCreate` (admins) makes one with an API key.
- **Agents** ([agents](https://linear.app/developers/agents),
  [interaction](https://linear.app/developers/agent-interaction),
  [signals](https://linear.app/developers/agent-signals),
  [best practices](https://linear.app/developers/agent-best-practices)): an `actor=app` install with
  `app:assignable` / `app:mentionable` makes the app a user that can be delegated issues and
  mentioned. Each delegation or mention opens an **agent session**: an `AgentSessionEvent` webhook,
  `created` (with `agentSession` (its issue and comment) and `promptContext`), then `prompted` for each
  reply (`agentActivity`, its `signal`, e.g. `stop`). The agent answers with **activities**
  (`agentActivityCreate`): `thought`, `action` (`action`, `parameter`, `result`), `elicitation`,
  `response`, `error`; thoughts and actions may be `ephemeral`; an elicitation may carry the `select`
  signal with options. An activity is due within 10 s of `created` or the session shows as
  unresponsive. `agentSessionUpdate` sets the session's `plan` and external links (the thread, the
  PR). `OAuthApp revoked` ends an install.

### Design

- **Connecting** an owner's workspace: roll's OAuth app (`ROLL_LINEAR_CLIENT_ID`,
  `ROLL_LINEAR_CLIENT_SECRET`, and `ROLL_LINEAR_WEBHOOK_SECRET`, the app's signing secret; the app's
  webhook URL is `{api}/v1/code/linear/webhook`, its redirect `{api}/v1/code/linear/oauth/callback`,
  or `ROLL_LINEAR_REDIRECT_URI`), installed with `actor=app` and the agent scopes; or a personal API
  key, for which roll makes the workspace's webhook (`/v1/code/linear/webhook/{hook}`, its own
  secret), or takes a hand-made one's secret. Tokens, keys and secrets are sealed with
  `ROLL_SECRET_KEY` (bound to the owner), never returned or logged. One owner per workspace.
- **Tokens** are refreshed when they expire within five minutes, or once when Linear refuses them;
  refreshes take turns per owner on a node and are stored by compare-and-swap on `token_rev`, so two
  nodes refreshing at once keep a working token. A refused refresh token marks the connection
  `reauth` (the card says connect again).
- **Webhooks** are checked (signature, the one-minute window), then one write records the events in
  the inbox with Linear's ids as keys (`linear:issue.created:<issue>`,
  `linear:issue.updated:<issue>:<updatedAt>`, `linear:comment.created:<comment>`,
  `linear:agent_session.created:<session>`, `linear:agent_session.prompted:<activity>`), a session's
  bookkeeping and the delivery's log line. Within the window a replay is the same event, so nothing
  runs twice; past it, it's refused. Nothing calls out before the 200. A session no enabled automation
  listens for gets an `error` activity at once (after the commit) saying so.
- **Events** carry the issue in one shape (`issue.{identifier, title, team, state, labels, assignee,
  delegate, priority, project}`), `changes` for updates (from `updatedFrom`: the state moved to,
  labels added and removed, the assignee, the priority), `repo` (the owner's team → repo setting),
  `thread` (the roll thread already working on the issue), `by_roll`, and for sessions `session`,
  `prompt`, `prompt_context`, `user`, `signal` and `attached` (a run showing the session took the
  reply).
- **The trigger** (`integration` with source `linear`) narrows events by `teams`, `projects`,
  `labels`, `label_added`, `states`, `state_changed_to`, `assignees` (`roll`, `none`), `priorities`,
  `matches` and `mentions_roll`; events roll caused and replies a run already took are left out
  unless `include_roll` / `include_attached`. Sessions need an OAuth (agent) connection.
- **Steps**: `linear.comment`, `linear.update_issue` (state by name or type, assignee, delegate,
  labels, priority, title, description), `linear.attach` (a link on the issue and the session),
  `linear.create_issue`, `linear.activity`, and `linear.agent_session`.
- **`linear.agent_session`** shows one thread in one session. It takes the session for its run
  (`linear_sessions.bridge_run`; a run that still holds it is woken instead, and gets the reply),
  follows the thread (`follow_thread`), links the thread on the session, then on each wake: passes
  replies on in order (`stop` interrupts; a reply to an elicitation answers its request with
  `request.respond`: approve, decline (any other words decline and tell the agent why), an option,
  or text; anything else steers the agent or starts its next run), and shows the thread's mirrored
  records past its cursor: tools as actions (ephemeral while running), reasoning and interim
  messages as thoughts, todos as the session's plan, approvals and questions as elicitations
  (`select` options), the last message (and the change's link) as the response, or an error. It
  ends when the thread is idle after the run it waited for, letting the session go with a
  compare-and-swap that fails while a reply hasn't been passed on. A reply after that is a new
  event (`attached: false`) whose run resumes the same thread.
- **The Linear agent** (`templates.rs`, also in the dashboard's gallery) is installed once when roll
  is connected as an agent (not again after the owner turns it off or deletes it): on
  `agent_session.created` and unattended `agent_session.prompted`, a thought at once, the issue to
  `started`, `agent.start` in a worktree of the team's repo on any online machine (skipped when the
  issue's thread goes on), `linear.agent_session`, `agent.publish`, and `linear.attach` of the
  change. The owner edits its target (a machine, a sandbox), provider and prompt like any automation.
  Follow-up comments outside the session steer the thread with the "Steer from Linear comments"
  template (`agent.message` to `event.data.thread`).
