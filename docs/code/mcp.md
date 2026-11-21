# roll code — MCP server

roll code is an MCP server at `/v1/code/mcp`. Any agent that speaks the Model Context Protocol
can use it to search and read code, take work, open and revise changes, review, and land, without
cloning anything. Every tool is a call to the [HTTP API](http-api.md) on the caller's behalf: tools
send the caller's own token, see exactly what the routes allow, and return the same shapes.

## Connect

The server uses the Streamable HTTP transport. Authenticate with an owner token (`roll code token
create <owner> <name>`) or an admin token (`ROLL_CODE_TOKEN` or rolld's `ROLL_ADMIN_TOKEN`), sent as
a Bearer token. A local daemon with neither is open, like the rest of the local API.

Claude Code:

```sh
claude mcp add --transport http roll https://api.roll.sh/v1/code/mcp \
  --header "Authorization: Bearer $ROLL_CODE_TOKEN"
# local daemon
claude mcp add --transport http roll http://127.0.0.1:7070/v1/code/mcp
```

Any MCP client that takes a JSON config:

```json
{
  "mcpServers": {
    "roll": {
      "type": "http",
      "url": "https://api.roll.sh/v1/code/mcp",
      "headers": { "Authorization": "Bearer rlc_…", "X-Roll-Agent": "fixer-7" }
    }
  }
}
```

`X-Roll-Agent` (optional) is the name the agent reviews and comments as. It works as it does on the
HTTP API.

## Tools

Repos are `"owner/repo"`. Changes are `"chg_…"` or `"owner/repo#N"`, and work items are `"wi_…"` or
`"owner/repo#N"`. When `rev` is left out, it means the default branch.

| tool | what it does | HTTP route |
| --- | --- | --- |
| `search_code` | literal or regex search; `rev` searches a change's head or a branch | `GET /repos/{r}/search` |
| `read_file` | a file at a revision; long files come back in line ranges (`start_line`, `end_line`) | `GET /repos/{r}/blob/{rev}/{path}` |
| `list_tree` | a directory, with each entry's last change | `GET /repos/{r}/tree/{rev}/{path}` |
| `list_repos` | the repositories the caller can read, optionally one owner's | `GET /repos` |
| `log` | first-parent history, optionally for one path | `GET /repos/{r}/log` |
| `blame` | which changeset last changed each line | `GET /repos/{r}/blame/{rev}/{path}` |
| `get_changeset` | a changeset and its diff | `GET /repos/{r}/changesets/{id}` |
| `compare` | changesets and file diffs between two revisions | `GET /repos/{r}/compare/{a}...{b}` |
| `create_change` | open a change from file edits on a base | `POST /repos/{r}/changes` |
| `revise_change` | a new revision of a change | `POST /changes/{id}/revise` |
| `update_change` | a change's title, description, labels or work item; mark a draft ready (back to draft would dequeue it: `dequeue_change`) | `PATCH /changes/{id}` |
| `land` | land a change, or edits directly | `POST /changes/{id}/land`, `POST /repos/{r}/land` |
| `get_change` | a change with its blockers, checks, review status, reviews (with what each reviewer wrote), threads and review requests | `GET /changes/{id}` + `/review-status` + `/reviews` + `/threads` + `/review-requests` |
| `list_changes` | a repo's changes | `GET /repos/{r}/changes` |
| `claim_work` | claim the highest-priority open work item (a lease) | `POST /repos/{r}/work/claim` |
| `heartbeat_work`, `release_work`, `complete_work` | keep, give back or finish a claimed item | `POST /work/{id}/…` |
| `list_work`, `create_work` | work items | `GET`/`POST /repos/{r}/work` |
| `get_work`, `update_work` | one work item; change its title, body, labels or priority (its state moves through claim, release and complete) | `GET`/`PATCH /work/{id}` |
| `comment` | reply in a thread, or start one on a line of a change | `POST /threads/{id}/comments`, `POST /changes/{id}/threads` |
| `review` | approve, request changes or comment, with inline comments | `POST /changes/{id}/reviews` |
| `resolve_thread`, `reopen_thread` | mark a review thread resolved, or open again | `POST /threads/{id}/resolve`, `/unresolve` |
| `apply_suggestion` | apply a comment's ```` ```suggestion ```` as a new revision and resolve its thread | `POST /threads/{id}/apply` |
| `request_review` | ask a person, an agent, a review app or roll to review a change (`required`: it can't land until they've reviewed its head) | `POST /changes/{id}/review-requests` |
| `queue_status` | the merge queue | `GET /repos/{r}/queue` |
| `enqueue_change`, `dequeue_change` | put a change in the merge queue (a lane; blocked is an outcome, as with `land`), or take it out | `POST`/`DELETE /changes/{id}/enqueue` |
| `set_change_lane` | move a queued change to the other lane, keeping its place in time | `PATCH /changes/{id}/enqueue` |
| `close_change` | close a change without landing it (it can't be reopened: `destructiveHint`) | `POST /changes/{id}/close` |
| `report_check` | a check's result on a change, from CI outside roll | `POST /changes/{id}/checks` |
| `set_commit_status` | a commit status on a revision (GitHub's statuses) | `POST /repos/{r}/statuses/{rev}` |
| `get_commit_statuses` | a revision's combined state and each context's latest status | `GET /repos/{r}/statuses/{rev}` |
| `list_refs` | the repo's branches and tags | `GET /repos/{r}/refs` |
| `create_repo` | a new, empty repository under an owner you manage | `POST /repos` |
| `request_ai_review` | ask roll's reviewer for a review | `POST /changes/{id}/ai-review` |
| `list_ai_reviews` | roll's review jobs for a change, oldest first (state, stage, progress, the review made) | `GET /changes/{id}/ai-jobs` |
| `get_ai_review_pricing`, `get_review_usage` | what an AI review costs by mode; an owner's spend in a month (by repo, mode, key; the cap left) | `GET /pricing`, `GET /owners/{owner}/usage` |
| `get_reviewer`, `get_review_rules` | how roll reviews a repo (effective settings, model, jobs, index); what a change needs to land | `GET /repos/{r}/reviewer`, `/review-settings` |
| `get_repo_audit` | what changed in the queue and settings (bypasses, pauses, lanes, approved runs, webhooks, review settings, dismissals, stored credentials), newest first (writers) | `GET /repos/{r}/audit` |
| `start_preview`, `stop_preview` | make a change's preview (it starts on its first visit), or remove it with its database (`destructiveHint`) | `POST`/`DELETE /changes/{id}/preview` |
| `list_agent_machines` | the owner's machines running `roll agent up` (online, providers) | `GET /agents/{owner}/machines` |
| `list_agent_threads` | agent threads (with their machine and project name), each machine's projects (with the repo each clones) and the requests waiting for an answer | `GET /agents/{owner}/state` |
| `create_agent_thread` | start a coding agent in a project on a machine | `POST /agents/{owner}/machines/{m}/commands` (`thread.create`) |
| `send_agent_message` | message a thread: start a run, queue after it, or steer it | `POST …/commands` (`message.send`) |
| `respond_agent_request` | answer an approval (`decision`) or questions (`answers`) | `POST …/commands` (`request.respond`) |
| `interrupt_agent_thread` | stop a thread's running turn | `POST …/commands` (`run.interrupt`) |
| `publish_agent_thread` | commit and push a thread's branch; in a roll code repo it becomes a change, and `land` sends it to landing (the answer's `landing` says landed, queued, blocked or conflict) | `POST …/commands` (`thread.publish`) |
| `create_agent_project` | create a roll code repo and clone it onto a machine as a project | `POST /agents/{owner}/machines/{m}/projects` |
| `claim_agent_work` | give a thread a work item: a given one, or the next one claimed from a repo (`work: null` when none) | `POST /agents/{owner}/threads/{t}/work` |
| `release_agent_work` | a thread lets go of its work item (the claim runs out on its lease) | `DELETE /agents/{owner}/threads/{t}/work` |
| `list_agent_automations` | the owner's automations (docs/code/automations.md) | `GET /agents/{owner}/automations` |
| `save_agent_automation` | create an automation, or replace one's definition (by id, over a `revision`) | `POST /agents/{owner}/automations`, `PUT …/automations/{id}` |
| `run_agent_automation` | run one now (by id or name) with `input` and an idempotency `key`; agents only when it allows agents | `POST …/automations/{id}/run` |
| `list_agent_automation_runs` | runs, newest first (of one automation, by status) | `GET …/automations/runs` |
| `get_agent_automation_run` | a run's steps (status, output, log) and its event | `GET …/automations/runs/{run}` |
| `cancel_agent_automation_run` | cancel a run going on (the owner's) | `POST …/automations/runs/{run}/cancel` |
| `retry_agent_automation_run` | a failed or cancelled run goes on from where it stopped (the owner's; `openWorldHint`, as `run_agent_automation`) | `POST …/automations/runs/{run}/retry` |
| `enable_agent_automation`, `disable_agent_automation` | turn an automation on or off (by id or name; runs going on go on) | `POST …/automations/{id}/enable`, `/disable` |
| `get_agent_automation_catalog` | every trigger and step kind, with each config's JSON Schema | `GET …/automations/catalog` |
| `test_agent_automation` | what a spec would do with an event, saving and running nothing | `POST …/automations/test` |
| `list_agent_sandboxes`, `stop_agent_sandbox`, `get_agent_sandbox_usage` | the owner's sandboxes; stop one (its billing stops); a month's running hours and price | `GET /agents/{owner}/sandboxes`, `POST …/{id}/stop`, `GET …/usage` |

The agents tools (docs/code/agents.md) take `owner`, and a `machine` by id (`mch_…`) or name.
Commands take an optional `id`, the idempotency key (a fresh one by default). Whatever the
daemon answers is a result, its errors too (`{type: "error", code, message}`: the thread is busy,
the machine is offline or didn't answer in 30 s); only the route refusing is a tool failure.
Agent tokens may use them to orchestrate agents, within what an agent's commands may do
(docs/code/agents.md, "Machines, login and tokens"): `create_agent_thread` in `ask` or
`auto_edit` only, and `respond_agent_request` and `create_agent_project` are the owner's.

`tools/list` returns each tool's JSON Schema. Every schema is closed (`additionalProperties: false`).

### Results

- Results come back twice: as `structuredContent`, which is the API's JSON (arrays are wrapped as
  `{items: […]}`), and as a `text` content block. `read_file`'s text block is the file content itself.
- **`land` outcomes are results, not errors.** `status` is one of:
  - `landed`
  - `queued`: the change is in the merge queue
  - `conflict`, with the conflicting `paths`: re-read them, revise, and land again
  - `blocked`, with `blockers`: an approval or a check is missing, for example
- API failures come back as tool results with `isError: true`. The text says what happened: "not
  found", "not allowed", or "this needs a token". Protocol problems, such as an unknown method or tool,
  or arguments that aren't an object, are JSON-RPC errors.
- `claim_work` returns `{work: null}` when nothing matches.
- Every `/v1/code` route the routers declare is a tool's or has a reason, method by method.
  `ROUTES` in `src/mcp.rs` lists the routes each tool calls; `src/no-tool.txt` lists every other
  route with why it has no tool (a person's, an admin's, a credential, an outside service's
  delivery, a stream, covered by another tool, or `no tool yet` for a gap). A new route fails
  `coverage_map.rs::every_route_has_an_mcp_tool_or_a_reason` until it has one or the other, as
  roll's other products do in `crates/roll-mcp`.

## Protocol

- **Versions:** the server speaks `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05`. It answers
  `initialize` with the version the client asked for when it supports it, and with the newest
  otherwise.
- **Sessions:** `initialize` returns `Mcp-Session-Id`. Every later request has to send it:
  - without a session id: 400
  - with an unknown or ended session: 404
  - from a different token than the one that started the session: 404

  `DELETE /v1/code/mcp` ends a session. Sessions are kept in control.db, so a session started
  through one node of a cluster works through any other (no sticky load balancing needed) and
  survives restarts. A session unused for 7 days ends (404: initialize again), and past 10,000
  sessions the least recently used go.
- **Messages:**
  - Notifications and responses get `202 Accepted`.
  - JSON-RPC batches are accepted, and the reply has one answer per request.
  - Responses are plain JSON by default. A request whose `Accept` is `text/event-stream` (without
    `application/json`) gets the answer as server-sent events instead: one `message` event per
    response, with `:` keep-alive comments every 15 seconds while a long call (a `land` waiting on
    a lander, say) runs. `initialize` always answers in JSON, because its session id is a header,
    and notifications alone still get 202. An event-stream answer holds one of the caller's event
    stream slots while it runs, shared with `GET …/events` (32 per caller by default); past that it
    gets `429` with `Retry-After`, and plain JSON answers still work.
  - There's no server-initiated stream, so `GET` answers 405.
- **Revoked tokens:** a request with a token that no longer works (revoked, or never valid) gets
  401 and ends its session, and `initialize` with one gets 401 too.
- **Origin:** a request with an `Origin` header (from a browser) must come from the web app's origin
  or the API's own origin, otherwise it gets 403. This stops a web page from driving a local daemon
  through DNS rebinding.

Tests are in `crates/code-control/tests/mcp.rs`.
