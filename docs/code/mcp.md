# roll code — MCP server

roll code is an MCP server at `/v1/code/mcp`. Any agent that speaks the Model Context Protocol
can use it to search and read code, take work, open and revise changes, review, and land, without
cloning anything. Every tool is a call to the [HTTP API](http-api.md) on the caller's behalf: tools
send the caller's own token, see exactly what the routes allow, and return the same shapes.

## Connect

The server uses the Streamable HTTP transport. Authenticate with an owner token (`roll code token
create <owner> <name>`) or the admin token, sent as a Bearer token. A local daemon with no
`ROLL_CODE_TOKEN` is open, like the rest of the local API.

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
| `log` | first-parent history, optionally for one path | `GET /repos/{r}/log` |
| `blame` | which changeset last changed each line | `GET /repos/{r}/blame/{rev}/{path}` |
| `get_changeset` | a changeset and its diff | `GET /repos/{r}/changesets/{id}` |
| `compare` | changesets and file diffs between two revisions | `GET /repos/{r}/compare/{a}...{b}` |
| `create_change` | open a change from file edits on a base | `POST /repos/{r}/changes` |
| `revise_change` | a new revision of a change | `POST /changes/{id}/revise` |
| `land` | land a change, or edits directly | `POST /changes/{id}/land`, `POST /repos/{r}/land` |
| `get_change` | a change with its blockers, checks, review status and threads | `GET /changes/{id}` + `/review-status` + `/threads` |
| `list_changes` | a repo's changes | `GET /repos/{r}/changes` |
| `claim_work` | claim the highest-priority open work item (a lease) | `POST /repos/{r}/work/claim` |
| `heartbeat_work`, `release_work`, `complete_work` | keep, give back or finish a claimed item | `POST /work/{id}/…` |
| `list_work`, `create_work` | work items | `GET`/`POST /repos/{r}/work` |
| `comment` | reply in a thread, or start one on a line of a change | `POST /threads/{id}/comments`, `POST /changes/{id}/threads` |
| `review` | approve, request changes or comment, with inline comments | `POST /changes/{id}/reviews` |
| `queue_status` | the merge queue | `GET /repos/{r}/queue` |
| `request_ai_review` | ask roll's reviewer for a review | `POST /changes/{id}/ai-review` |

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

## Protocol

- **Versions:** the server speaks `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05`. It answers
  `initialize` with the version the client asked for when it supports it, and with the newest
  otherwise.
- **Sessions:** `initialize` returns `Mcp-Session-Id`. Every later request has to send it:
  - without a session id: 400
  - with an unknown or ended session: 404
  - from a different token than the one that started the session: 404

  `DELETE /v1/code/mcp` ends a session.
- **Messages:**
  - Notifications and responses get `202 Accepted`.
  - JSON-RPC batches are accepted, and the reply has one answer per request.
  - Responses are plain JSON by default. A request whose `Accept` is `text/event-stream` (without
    `application/json`) gets the answer as server-sent events instead: one `message` event per
    response, with `:` keep-alive comments every 15 seconds while a long call (a `land` waiting on
    a lander, say) runs. `initialize` always answers in JSON, because its session id is a header,
    and notifications alone still get 202.
  - There's no server-initiated stream, so `GET` answers 405.
- **Revoked tokens:** a request with a token that no longer works (revoked, or never valid) gets
  401 and ends its session, and `initialize` with one gets 401 too.
- **Origin:** a request with an `Origin` header (from a browser) must come from the web app's origin
  or the API's own origin, otherwise it gets 403. This stops a web page from driving a local daemon
  through DNS rebinding.

Tests are in `crates/code-control/tests/mcp.rs`.
