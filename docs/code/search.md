# Searching across repos

Status: design (2026-11-22). Not built yet.

`GET /v1/code/search` (roll MCP's `search`, #745) finds the changes and work items of the caller's
own repos. It reads control.db, which every node shares, so it never leaves the node that got the
request. This document is the plan for the next step: files by content, across the same repos.

## What's in the way

- **The index is per repo, on the repo's owner.** code-index keeps one SQLite file per repo under
  the node's data directory, and only the repo's owner updates it as landings arrive
  (docs/code/cluster.md, "A repo's work, on its owner"). Another node has no copy, or an old one.
- **A search must not slow landings.** Landing stays O(change). A search reads the index, never
  the lander's state, and never waits on a repo's lock.
- **The caller can own many repos.** One search must stay bounded however many there are.

## The route

`GET /v1/code/search?q=&kinds=&limit=` gets a `kinds` parameter: `changes`, `work` and `files`.
Leaving it out means `changes,work`, as today, so existing clients get the same answer. A file
result is:

```
{id: "f.<repo id>.<rev>.<path, base64url>.<line>", title: "owner/repo: path:line", url: "<web>/owner/repo/blob/<rev>/<path>#L<line>",
 kind: "file", repo, path, line, text /* the matching line */, outside: false}
```

The answer gains `skipped: [{repo, why}]`. `why` is `timeout`, `unavailable` (its owner didn't
answer), `indexing` (the first build hasn't finished) or `limit` (past the repos one search
covers). A repo that wasn't searched is always listed there. It is never left out silently.

`GET /v1/code/documents/{id}` takes file ids. The id names the repo id, so `cluster::classify`
routes it to the owner as a new `Target::RepoId` (the route key is the repo id). The owner reads
the file at that revision through `Store::materialize`'s rules (no symlinks followed) and answers
the lines around the match, up to 400 lines, with the whole file's URL.

## The scatter

The node that gets the request:

1. Lists the caller's searchable repos (`Code::searchable`, as for changes and work items). It
   keeps the 200 most recently landed. The rest go in `skipped` with `limit`.
2. Groups them by owner (`Cluster::route_async` on each repo's key): local repos and one group per
   other node.
3. Searches its own repos in process. For each other node it sends one signed request,
   `POST /internal/code/search {repos: [repo id], q, regex: false, per_repo: 5, deadline_ms}`, on
   the internal router (`require_signed`, as `/internal/code/repos/{id}/kick` is).
4. Waits up to 3 s overall. A node that hasn't answered by then has all of its repos skipped with
   `unavailable`.

The owner checks `Code::owns_repo` for each repo id (a repo that moved since is skipped as
`unavailable`; the first node doesn't retry). It runs each repo's search on the blocking pool with
a 1 s budget, skips with `timeout` what runs out, and answers matches with their `repo`. The
caller's identity doesn't cross the hop: the first node already chose the repos for the caller,
and the hop is signed by a node of the cluster. That is the same trust `kick` and `events` rely
on. Results still go back through the first node's gate, whose list filter (`results` by `repo`)
narrows a scoped token's answer. On one node the same code runs without the hop.

## Ranking

Within a repo, code-index's order: a case-exact match first, then by path. Across repos, results
interleave round-robin, most recently landed repo first. That way one big repo can't fill the
page. Changes and work items keep their recency order and come before files when `kinds` asks for
both.

## Limits

- 200 repos, 5 matches per repo, `limit` 1-50 overall.
- A literal query only, at least 3 characters, so the trigram index narrows it. Regex stays
  per-repo (`search_code`).
- A 1 s budget per repo and 3 s overall.
- 64 KiB per internal answer. Lines are cut at 300 characters.

## Tests to write with it

- `code-control/tests/cluster.rs`: three nodes, repos owned by each. A file search through a
  non-owner finds a match in every repo. A stopped node's repos come back in `skipped` as
  `unavailable` within the overall wait.
- `code-control/tests/mcp.rs`: `search` with `kinds=files` and `fetch` of a file id. Another
  owner's public repo is still left out.
- `routes.rs`: file document ids in the documents matrix.
