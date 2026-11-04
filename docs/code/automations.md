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
| `code {events, repos}` | the webhook events | the webhook payload's `data`, plus `repo` |
| `agent {events}` | `thread.created`, `thread.done`, `thread.failed`, `thread.waiting`, `thread.settled`, `request.opened` | `{machine, project, thread, run?, request?}` |
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
