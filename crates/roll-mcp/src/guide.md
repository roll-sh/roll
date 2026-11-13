# Using roll over MCP

roll runs serverless Postgres, Redis, storage, streams, queues, tasks, NoSQL, source control
(roll code), coding agents, secrets, apps, previews and search, grouped into workspace projects.

## Before you act

Read `roll://access/me`. It says, in words, what your connection may do: which products, which
actions (read, write, delete, reveal, deploy, admin), on which resources and environments, and what
happens to everything else (refused, or asked of a person). `tools/list` shows only the tools you
can use at all; a tool being listed doesn't mean every resource is allowed.

## Tools

- Named `<product>_<verb>`. Ids are each product's own: Postgres projects `prj_…`, Redis
  databases `rdb_…`, streams `st_…`, buckets by name, repos `owner/name`.
- Each tool is one request to roll's API with your token, decided by the same rules as the API and
  the CLI.
- `postgres_query` is read-only; `postgres_execute` changes data.
- Credentials come back as `[redacted]` unless your connection may reveal them.
- `apps_deploy` takes an `idempotency_key`: reuse it when you retry, and the deploy runs once.

## When a person must approve

Some actions ask a person first. The call waits up to a minute, then returns
`status: "approval_pending"` with an approval id and a link. Tell the user, then call the same tool
again with the same arguments plus `"approval": "apr_…"`: it waits for the decision and runs once
approved. `access_wait_approval` waits on an approval without running anything.

## When a call fails

- `not allowed`: your connection's rules refuse it. Don't retry or try another tool for the same
  thing: ask the user to widen the token, or to approve it if the token asks for anything else.
- `429`: too many requests for this token. Wait for `Retry-After` seconds.
- `the token isn't valid any more`: it expired or was revoked. Ask the user to reconnect.
