# roll code — test coverage

Every feature maps to the tests that cover it. `pnpm test:code` runs all of them: about 330 Rust
tests across the code crates and `roll-cli`, and about 150 vitest tests for the dashboard. When you add a
feature, add its row here.

Paths are relative to `crates/`, except the dashboard rows. `file::test` names a test function.

## HTTP API (`/v1/code`, docs/code/http-api.md)

| Route / behaviour | Tests |
| --- | --- |
| `GET /v1/code` (info, stats, `auth.required`, `auth.caller` for each kind of token) | `code-control/tests/api.rs::land_merge_and_conflict`, `contract.rs::auth_matrix`, `contract.rs::open_mode_makes_everyone_admin` |
| `GET /names/{name}`, `/names/{owner}/{repo}` (taken, reserved, invalid, case-insensitive) | `api.rs::names_and_owners`, `api.rs::repos_rename_and_visibility` |
| owners: list, create (admin only), get (private repos hidden), patch, tokens | `api.rs::names_and_owners`, `contract.rs::auth_matrix`, `contract.rs::open_mode_makes_everyone_admin` |
| repos: create, duplicate (409), invalid (422), list `?owner=`, get by any case, patch (name, description, visibility, land policy, default branch), delete | `api.rs::repos_rename_and_visibility`, `contract.rs::auth_matrix`, `contract.rs::errors_are_json_with_the_right_status` |
| rename: 301 with `{redirect}` and `Location`, the name can be reused, survives a restart | `api.rs::repos_rename_and_visibility`, `contract.rs::state_survives_a_restart` |
| refs, tree (kinds, modes, sizes, `last_change`), blob (content types, CSP, `nosniff`, ETag, immutable caching by full id), paths (cap, `truncated`), log (cursor, path), changeset by id, prefix or git commit, compare | `api.rs::land_merge_and_conflict`, `contract.rs::writes_binary_content_deletes_and_blob_caching`, `contract.rs::paths_are_capped` |
| readme: rendering, sanitizing, relative links and images (markdown and raw HTML, `srcset`), README in a subdirectory | `api.rs::readme_is_rendered_and_sanitized`, `code-control/src/render.rs` tests |
| `POST /changesets`, then land `{changeset}`; `content_b64`, `delete`, modes | `contract.rs::writes_binary_content_deletes_and_blob_caching` |
| `POST /land`: fast-forward, rebase, server-side merge, 409 conflict body, `ref` | `api.rs::land_merge_and_conflict` |
| changes: create, revise (checks reset), land with required checks (queued, 202), conflict, close, by number, `#n` or id, author attribution | `api.rs::changes_and_work`, `contract.rs::change_authors`, `contract.rs::lookups_by_number_and_id` |
| changes: `draft` (create, `PATCH`, blocks landing on both policies, dequeues), `PATCH` title/body validation, `approve-run` (per head, audited) | `queue.rs::blockers_keep_changes_out_of_the_queue`, `queue.rs::post_merge_repos_land_directly_but_respect_drafts` |
| checks: external report, status validation, timestamps | `api.rs::changes_and_work`, `contract.rs::event_shapes_and_previews`, `contract.rs::errors_are_json_with_the_right_status` |
| previews: create and delete through a backend; 422 without one | `contract.rs::event_shapes_and_previews`, `contract.rs::errors_are_json_with_the_right_status` |
| work: create, priority and label validation, claim (priority order, labels, 204), lease expiry, heartbeat (keeps the lease length), release, complete, patch, someone else's lease (409) | `api.rs::changes_and_work`, `contract.rs::lookups_by_number_and_id`, `contract.rs::errors_are_json_with_the_right_status` |
| SSE: `landed`, `change`, `work`, `check`, `preview` shapes, per-repo isolation, private feeds | `api.rs::events_stream_landings_and_changes`, `contract.rs::event_shapes_and_previews` |
| errors: JSON `{error}` with the right status for bad JSON, unknown revs, bad paths, bad enums and every 404 | `contract.rs::errors_are_json_with_the_right_status` |
| CORS: preflight with `Authorization`, `x-roll-changeset` exposed | `contract.rs::cors_for_the_browser` |
| GitHub endpoints without an App: info, installations, setup redirect, webhook 404, connect refused | `contract.rs::github_without_an_app` |
| restart: owners, repos, redirects, changes, checks, work items, the number counter | `contract.rs::state_survives_a_restart` |
| blame: per-line commits equal `git blame --first-parent` of a pushed history with a merge; author/time/message per range; cached; a revision is echoed; 404 file and revision | `webhooks.rs::blame_matches_git` |
| search at a revision (`rev`): the change's own edits and deletions, the index for the rest, `overlay`, resolved `rev`, globs and limits on both halves, 404 revision, 400 bad regex | `webhooks.rs::search_at_a_change_head`, `mcp.rs::every_tool_end_to_end` |
| SSE `push` event (`ref`, `before`, `after`, `accepted`) | `webhooks.rs::pushes_are_webhook_events` (same data as webhooks) |

## Webhooks and commit statuses (`code-control/tests/webhooks.rs`)

| Behaviour | Tests |
| --- | --- |
| create validation (url scheme and credentials, unknown/empty events, short secret, `active`), generated secret shown once, `"*"`, list/get/patch/delete | `webhooks.rs::webhooks_deliver_signed_events` |
| signed ping on creation and on request; `landed`, `change`, `work`, `review` (once subscribed) deliveries with the payload shape; events not subscribed aren't sent; inactive hooks get nothing; a caller-chosen secret signs | `webhooks.rs::webhooks_deliver_signed_events` |
| a failing receiver retried until it answers (attempts in the log), redelivery, unknown delivery 404 | `webhooks.rs::webhooks_deliver_signed_events` |
| `push` events from git pushes | `webhooks.rs::pushes_are_webhook_events` |
| writers only (401 anonymous, 403 other owners) for hooks and statuses; private addresses refused at delivery with an admin token | `webhooks.rs::webhooks_need_writers_and_public_addresses` |
| `landed` from the durable land log: a 4,500-landing burst (more than the broadcast channel holds) delivered exactly once each; landings while webhooks weren't running delivered after a restart, from the persisted cursor; a new webhook gets no history | `webhooks.rs::landed_deliveries_survive_a_burst_larger_than_the_broadcast_channel`, `webhooks.rs::landings_while_the_daemon_was_down_are_delivered_after_a_restart`, `webhooks.rs::a_new_webhook_gets_no_history`, `code-store/tests/land_log.rs` |
| the delivery log keeps the start of each response (2 KB, cut at a character) | `webhooks.rs::deliveries_keep_the_start_of_the_response` |
| failing statuses on the default branch: one `broken-main` item per context (deduplicated), a certain culprit reverted (and landed with `auto_revert`), an uncertain one named as a range, closed by a later success for that context only, not by an older one; statuses off the default branch break nothing | `webhooks.rs::failing_statuses_on_the_default_branch_break_main`, `webhooks.rs::failing_statuses_auto_revert` |
| statuses: pending/error/success → checks on the changes at that head (`source: external`, URL, `error:` summary); a queue repo's `required_checks` unblocked by external CI; combined state over contexts; a commit with no change recorded only; by git commit (and prefix) after a push; validation (state, target_url, context length), unknown commit 404; `check` events; deleting the repo removes hooks and statuses | `webhooks.rs::commit_statuses_become_checks`, `webhooks.rs::pushes_are_webhook_events` |

## MCP (`code-control/tests/mcp.rs`)

| Behaviour | Tests |
| --- | --- |
| sessions (400 without, 404 unknown, ended, bound to the token), version negotiation, `initialized`, ping, tools/list, unknown method and tool, non-object arguments, parse errors, wrong `jsonrpc`, batches with notifications, `MCP-Protocol-Version`, Origin, `GET` 405, `DELETE` | `mcp.rs::transport_sessions_and_protocol` |
| every tool end to end: land (landed, conflict, blocked), read_file (ranges, 404), list_tree, log, blame, get_changeset, search_code (default branch and a change head), create/revise/get/list changes by id and `owner/repo#N`, compare, comment (new thread and reply), review with inline comments, request_ai_review, queue_status, work claim (none, labels)/heartbeat/release/complete/list/create, argument errors | `mcp.rs::every_tool_end_to_end` |
| tools enforce the routes' authorization: anonymous public reads, private 404, writes need a token, another owner can't write, admin can | `mcp.rs::tools_enforce_the_routes_authorization` |
| every tool's schema is closed and its required fields exist; path and query encoding | `code-control/src/mcp.rs` unit tests |
| `Accept: text/event-stream`: one `message` event per response (single and batch), notifications 202, sessions still required, initialize answers JSON | `mcp.rs::responses_as_server_sent_events` |
| a revoked token ends its session (401) and can't start one | `tokens.rs::tokens_are_listed_and_revoked_everywhere_at_once` |

## Work trees (`code-control/tests/checkout.rs`, `code-store/tests/checkout.rs`)

| Behaviour | Tests |
| --- | --- |
| `checkout_delta` equals a fresh `materialize` both ways across adds, deletes, emptied directories, file↔directory, exec bit, symlink retargets and symlink↔file; same changeset is a no-op | `code-store/tests/checkout.rs::delta_matches_a_fresh_materialize` |
| a tampered tree (a directory swapped for a symlink, a missing file) fails instead of writing outside; a hostile target tree (duplicate names) never escapes | `checkout.rs::a_tampered_tree_fails_instead_of_writing_outside`, `checkout.rs::a_hostile_target_tree_never_escapes` |
| leases (concurrent jobs get different trees), reuse and delta moves, no-op at the same changeset, fresh checkout after a failed move, throwaway checkout when all trees are busy (removed after), trees removed with the repo | `code-control/tests/checkout.rs::trees_are_leased_moved_and_replaced` |
| the queue and the runner use them (2,000-change queue run: 25 → 74–124 merges/s) | `queue.rs::throughput_with_culprits`, `code-preview/tests/service.rs` |
| previews read the pooled trees (moved by diff between changes, no per-change copies) | `contract.rs::previews_use_pooled_work_trees` |
| a restart sets aside and removes what the last process left (pooled trees, throwaway checkouts, old per-change preview trees, a half-finished cleanup); no lease survives | `checkout.rs::a_restart_clears_what_the_last_process_left` |

## Merge queue (`code-control/tests/queue.rs`, fake runner)

| Behaviour | Tests |
| --- | --- |
| batching: one test for the batch, FIFO landing, positions, `queue_id`, `recent`, `landed_1h`, `last_green`, merges counted per changeset, scratch refs hidden, `queue_depth` | `batches_land_together_in_fifo_order` |
| lanes: urgent before normal, re-enqueue is a no-op, bad lane 400 | `urgent_lane_goes_first` |
| red batch: bisected to one culprit (prefix lands, rest re-queued, a one-line `error` and the failing check's log tail in `output`), batch size adapts | `a_red_batch_is_bisected_to_its_culprit` |
| moving lanes keeps the entry and `enqueued_at`, positions follow the lane order, audited; no lane 400, bad lane 400, not queued 409 | `lanes_can_change_without_losing_place` |
| `batch.testing_prefix` (the whole batch, then the half being bisected), `Repo.stats.queue_batch`, `output` on the change and in `recent` | `the_batch_shows_the_prefix_under_test` |
| two culprits | `two_culprits_are_both_ejected` |
| a check already red on the head doesn't count against the batch; its fix gets through | `a_check_that_fails_on_main_too_doesnt_eject_anyone` |
| conflicts ejected with paths (state `conflict`); revised, it lands | `conflicts_are_ejected_with_their_paths` |
| the default branch moves under a batch (admin bypass): rebuilt, audit entry | `the_batch_is_rebuilt_when_main_moves_under_it` |
| bypass: admin only (403 for an owner), owner writes queued with `enqueued_by`, audit visibility, anonymous can't pause, anyone can read a public queue | `bypass_needs_the_admin_token` |
| pause and resume (the worker waits), audited | `pause_and_resume` |
| dequeue mid-test (batch rebuilt), dequeue twice 409 | `dequeueing_a_change_under_test_rebuilds_the_batch` |
| revise while queued (stays queued, retested), close while queued (leaves) | `revising_or_closing_a_queued_change` |
| the batch shows in landing order (urgent first, in flight and waiting), with positions and its batch id | `the_batch_under_test_is_shown_in_landing_order` |
| landing re-checks that every change is still queued at the tested head | `a_batch_never_lands_a_change_that_left_the_queue` |
| the default branch moves while a batch is bisected: everything is tested again, nothing gets stuck | `main_moving_while_a_batch_is_bisected_retests_everything` |
| deleting a repo stops its queue worker and drops its entries | `deleting_a_repo_stops_its_queue_worker` |
| restart: the batch under test is tested again | `a_restart_retests_what_was_in_flight` |
| restart: a `landing` entry whose tip is on main counts as landed (once), one whose tip isn't goes back in line | `a_restart_finishes_a_batch_that_had_landed` |
| git push to main and to `refs/land/main` on a queue repo: `ng queued as change #N (position P)`, `remote:` hints, no duplicate change for the same head; other branches untouched | `git_pushes_to_main_are_queued` |
| blockers: draft, checks (`required_checks` and `require_change_checks`), untrusted (approve-run, reset on revision), closed 409 | `blockers_keep_changes_out_of_the_queue` |
| a change that stops being mergeable while it waits is ejected | `changes_that_stop_being_mergeable_while_waiting_are_ejected` |
| `GET …/queue` shape, settings from roll.toml (and a broken one), `queue` SSE event shape, 404, `land_policy` validation | `queue_events_and_shapes` |
| a `[checks.*]` table the runner rejects (e.g. `timeout = "60s"`) shows as the queue settings' error (`Runner::validate`) | `a_check_table_the_runner_rejects_shows_in_the_settings`, `config.rs::errors_name_the_key` |
| post_merge repos: drafts block landing; the queue still works when asked | `post_merge_repos_land_directly_but_respect_drafts` |
| reverts: inverse diff restores modified, deleted and added files, a directory and the mode; later work on the same file kept; links and finishes the work item; the first changeset can't be reverted | `a_revert_restores_the_parent_and_keeps_later_work` |
| a suggested revert (no `auto_revert`) leaves its broken-main item open, never passing through `review`; a revert that will land starts the work | `a_revert_restores_the_parent_and_keeps_later_work`, `webhooks.rs::failing_statuses_on_the_default_branch_break_main` |
| a revert on a queue repo is queued urgent, ahead of waiting changes | `a_revert_on_a_queue_repo_jumps_the_line` |
| `find_culprit`: none without a known green head; bisects 16 changesets in ≤ 4 test runs | `find_culprit_bisects_from_the_last_green_head` |
| 2,000 changes with 2% culprits: all others land exactly once, in queue order, on a linear main (`CODE_QUEUE_N`; 2,000 in `pnpm test:code`) | `throughput_with_culprits` |
| `Store::advance`: one merge and one event per changeset, refuses a moved ref or a tip that isn't on top, creates a missing ref, keeps the path index exact; scratch landings never listed, counted or published | `code-store/tests/advance.rs` (all) |
| `[queue]` and `[main]` parsing and errors (both parsers) | `code-control/src/settings.rs` tests, `code-preview/src/config.rs::queue_and_main_tables`, `config.rs::errors_name_the_key` |

## Reviews (`code-control/tests/review.rs`, `tests/github.rs::reviews_both_ways`, `src/review/*` unit tests)

| Behaviour | Tests |
| --- | --- |
| required approvals; self-approval; org token, `agent:` token and `X-Roll-Agent` identities; request changes blocks until superseded or dismissed (409 twice, audited); a comment doesn't lift it; stale approvals with and without `dismiss_stale`; `allow_self_approval`; bad input 400s; a landed change takes no reviews (409) | `rules_matrix` |
| dismissing a reviewer's latest change request doesn't revive their older one (a dismissed review supersedes, counts for nothing) | `rules_matrix`, `rules::a_dismissed_review_doesnt_bring_back_an_older_one` |
| a change's base stays where it left main when revisions stack on its head (applied suggestions, a second pushed commit) | `threads_suggestions_and_carry_forward`, `git.rs` (branch push revises) |
| the review provider reads as nulls (hosted) for its owner, 404 for anyone else | `reviewer.rs` (caps and BYOK), `code-api.test.ts` |
| roll's reviewer approvals and `count_ai_approval`; `land_change` blocked (422), owner bypass 403, admin bypass lands and is audited | `ai_approvals_and_land_change_gate` |
| CODEOWNERS groups and messages, `agent:` owners, the file can't lower repo settings, unknown keys reported, auto-approve by paths and comment-only, requested changes still block auto-approved changes, merge-base paths (a trailing docs commit doesn't hide code), a broken review file falls back | `codeowners_auto_approve_and_the_review_file` |
| threads: validation (path, lines, body, side, traversal), replies, resolve/unresolve, `require_resolved_threads`, suggestions applied (single and multi-line, writers only, old side 422, none 422), carry forward (moved, kept, outdated → 409) | `threads_suggestions_and_carry_forward` |
| `/land` and `git push` (main and `refs/land/main`) open a change when reviews are required; owner bypass 403; auto-approved paths land directly; other branches aren't gated | `direct_landings_and_pushes_need_review` |
| queue: enqueue blocked until approved, re-blocked after a revision, `/land` on a queue repo checks reviews | `the_queue_waits_for_approval` |
| review requests: required → `review_pending`, roll's review clears it, per head, by hand; bad input; writers only | `review_requests_and_pending_reviews` |
| review apps: validation, owner-only management, token shown once, scoped reads (private installed / not installed), no writes, signed `change.opened`, `review.requested` for required apps, diff URL, app reviews clear the request, no echo of its own comments, retries with backoff, delivery log, redeliver, update/disable/rotate/delete | `review_apps_lifecycle_and_deliveries` |
| app tokens are scoped on an open daemon; bogus app tokens 401 | `app_tokens_are_scoped_even_on_an_open_daemon` |
| webhooks to private addresses (IP and `localhost`) are refused | `private_webhook_addresses_are_refused` |
| `review`, `thread` and `change` events; `compare?format=` json, diff, bad 400 | `review_events_and_diff_format` |
| every review route × anonymous / another owner / the repo's owner, public and private; dismissing own vs others'; resolving own threads | `auth_matrix` (review.rs) |
| GitHub: CodeRabbit review → app review blocking the change, redelivery, edit, pending ignored, echo ignored; Greptile inline comment with suggestion → thread linked to its review when it arrives, replies (once), edit, delete, resolve, dismiss; roll review → one GitHub review with inline comments, keyed so GitHub replies land in roll's thread; standalone threads and replies posted once; refused approvals fall back to a comment | `reviews_both_ways` |
| gitignore/CODEOWNERS patterns, CODEOWNERS parsing and owners, the rules evaluation, `.roll/review.yaml` parsing and tightening, comment-only detection per language (strings with markers, lifetimes, raw strings, docstrings, indentation), line diffs, carry-forward, suggestions, line replacement, private address ranges, URL checks, signatures | `review::{pattern,codeowners,rules,comments,lines,apps}` unit tests |
| GitHub review webhook parsing (every action, ignored ones, malformed) | `code-github/tests/events.rs::review_events` |
| CLI: `review` (status, comment with inline comments as an agent, approve, request, request-changes, dismiss, bad action), `thread` (list, open, reply, resolve, unresolve, apply), `review-settings`, `review-apps` (create, list, show, update, rotate, deliveries, redeliver, delete) | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `code::review` unit tests |
| `GET /v1/code` `auth.caller.reviewer` (agent token, `X-Roll-Agent`, `X-Roll-User`, anonymous); an unplaceable inline comment → 400 `{comment: index}` and nothing written; review requests carry the `Reviewer` object; `review-settings` with no file and no CODEOWNERS (`file: null`, `codeowners.path: null`); review apps without `repos` are `all_repos`; renamed files: old side takes the old path, new side the new path, the other combinations 400 | `contract_details_caller_comment_index_renames_and_apps` |

## roll's reviewer (`code-control/tests/reviewer.rs`, `tests/github.rs`, `code-review/tests/engine.rs`, `code-index/tests/index.rs`, unit tests)

No test calls a real model API: Anthropic and OpenAI-compatible endpoints are mocks that record
every request.

| Behaviour | Tests |
| --- | --- |
| Anthropic request: `x-api-key`, `anthropic-version`, fallback beta header + `fallbacks: "default"` (not for Haiku), `max_tokens`, effort always set, no `thinking`, no `tool_choice`/tools, one user message (no prefill), system blocks with `cache_control` on the last, byte-identical prefix across reviews, schemas valid for structured outputs; `count_tokens` without `output_config`/`fallbacks`/beta; usage incl. cache tokens | `engine.rs::anthropic_request_shape_and_cache_prefix`, `provider::anthropic` unit tests, `schema` unit test |
| 429 with `retry-after` honoured, 529 retried, 400 not retried, retries exhausted → `Exhausted`, 401 surfaced | `engine.rs::retries_429_with_retry_after_but_not_400`, `every_group_failing_is_an_error_and_stop_reasons_are_read` |
| chunking by the token counter (split until it fits; a file too big alone skipped and listed), caller-skipped files listed, `files_reviewed` | `engine.rs::chunks_by_token_count_and_lists_skipped_files` |
| `max_tokens` → split and retry, one file that still doesn't fit skipped; `refusal` (with category) skips its files; cut-off usage still counted; other stop reasons | `engine.rs::max_tokens_splits_and_refusals_skip`, `anthropic::responses` |
| `fast`: secondary model, low effort, groups in parallel (bounded), findings streamed, no verification; `thorough`: xhigh + verification; effort override; `standard` gets no repository context; `max` verification drops unconfirmed findings and shares the cached prefix | `engine.rs::fast_runs_groups_in_parallel_and_streams`, `standard_has_no_context_and_thorough_raises_effort`, `max_mode_verifies_and_uses_context` |
| dedupe against open roll threads and within a review; chill drops nits; findings outside the diff become notes; verdicts per profile; suggestions only on new-side lines shown; rendering of body/table/diagram/skipped/finding | `engine.rs::dedupes_against_open_threads_and_profiles`, `engine` unit tests |
| classifier call (secondary model, low effort) and thread replies | `engine.rs::classifier_and_replies` |
| OpenAI-compatible: `/chat/completions`, bearer auth, strict `json_schema` response format, cached-token usage, no token counter; config validation; refusals and `length` | `engine.rs::openai_compatible_request_shape`, `provider::openai` unit tests |
| settings overlay/validation/round-trip; `auto` routing (deep/max paths, sensitive and critical paths, lock files aren't, labels, fan-out, size, modules, untested code, code owners, author history, reasons); prompt blocks and line numbering; pricing (list, per-file minimum, multipliers, BYOK fees, chat, public allowance, human cap, agents uncapped, provider costs incl. cache, env overrides); BYOK sealing (round-trip, owner binding, wrong server key, tampering, random nonces); `@roll` command parsing; globs | `code-review` unit tests |
| codebase index: build, definitions/references (most uses first, lines), fan-out count, callers outside the change, related tests, similar code, literal/case-sensitive/regex search with path filter and limits, bad regex; incremental update (renamed symbol, deleted file's postings gone, new caller), persistence, clear, remove, bad repo ids; symbol extraction for every language; test-path detection; regex literals | `code-index/tests/index.rs`, `code-index` unit tests |
| automatic review on open: `review_pending` while it runs, request shape through code-control (hosted key, guidelines from `AGENTS.md`, custom prompt), verdict + `ai` record, threads with suggestions, `changes_requested` after; metering (price, mode, key, provider cost); usage is the owner's only | `reviewer.rs::reviews_a_change_automatically_with_the_right_request` |
| incremental re-review (only changed files, not ignored ones), dedupe of a repeated finding, only-ignored changes skipped, `full` reviews everything | `reviewer.rs::incremental_re_reviews_dedupe_and_skips` |
| `auto` heuristic (deep_paths → deep with repository context and confidence; small → standard), `classifier: model` (secondary model, metered), code search (path glob, bad regex 400, private repo 404), index stats in the reviewer view | `reviewer.rs::auto_mode_deep_context_and_classifier` |
| `fast` streams threads without duplicating them in the review; `max` drops unverified findings | `reviewer.rs::fast_streams_threads_and_max_verifies` |
| `@roll` explain, question, don't flag (learning used by later reviews), pause/resume, review deep (forced mode), fix (a revision with the suggestion), resolve; chats metered; non-writers ignored | `reviewer.rs::roll_commands_in_threads` |
| learnings from a resolved thread with a reason (once), a dismissed roll review, the API (path-scoped), delete, auth | `reviewer.rs::learnings_from_resolving_and_dismissing` |
| public-repo allowance, human monthly cap, spending caps (`pause` without a model call, `byok_fallback`, `standard_only` downgrade), BYOK (used first, BYOK fee, owner's key in the request, key never returned nor in the database file, auth, validation, delete), pricing endpoint, billing validation and auth, bad month | `reviewer.rs::pricing_caps_allowances_and_byok` |
| BYOK refused without (or with a weak) `ROLL_SECRET_KEY` | `reviewer.rs::byok_needs_a_server_secret` |
| a failing provider retried (job attempts), a bad key not retried and reported on the change (no longer pending), no provider configured | `reviewer.rs::retries_then_gives_up_and_no_provider` |
| off by default, settings validation and auth, `.roll/review.yaml` wins (errors reported), drafts wait until ready, requested reviews (auth, bad mode, forced mode) | `reviewer.rs::triggers_settings_and_auth` |
| a job running when the daemon stopped runs again after a restart | `reviewer.rs::restart_recovers_running_jobs` |
| roll's review on a GitHub PR is posted once as a GitHub review with its suggestion | `github.rs::roll_reviewer_posts_to_the_pull_request` |
| BYOK OpenAI-compatible end to end through code-control: owner's key, base URL and model, strict `json_schema`, cached tokens metered, the hosted key unused; findings as data (`finding`); job `stage: done` with `progress` | `reviewer.rs::byok_openai_compatible_end_to_end` |
| job stages while running (`chunking` → `reviewing` with `{groups_done, groups_total}` → `done`) | `reviewer.rs::jobs_report_their_stage_while_running`, `engine.rs::fast_runs_groups_in_parallel_and_streams` (progress partials), `engine.rs::max_mode_verifies_and_uses_context` (`Verifying`) |
| `parse_finding` reads back what `render_finding` wrote | `code-review` `engine::tests::findings_read_back` |
| `AiConfig::from_env` (hosted Anthropic / OpenAI-compatible, concurrency, model rates), months | `ai::tests::config_from_env` |
| CLI: `reviewer` (show, enable, mode, speed, `--set`, bad mode), `review --ai --mode --full` (and `--mode` needs `--ai`), `usage` (current and other months), `billing` (cap, none, on-cap, bad cap), `provider set/show/clear` (key never printed), `learnings list/add/rm`, `search` (literal, regex, limit, bad regex, `repo@rev`) | `roll-cli/tests/code.rs::ai_reviewer_commands` |

## Auth matrix

`contract.rs::auth_matrix` covers this matrix, plus Basic auth and owner-scoped actions. The open
mode row is `contract.rs::open_mode_makes_everyone_admin`. The git rows are
`git.rs::git_private_repo_needs_token` and `git_more.rs::git_auth_for_other_owners_and_anonymous`.

| Caller | Read public | Read private | Write public | Write private |
| --- | --- | --- | --- | --- |
| open mode (no `ROLL_CODE_TOKEN`) | ✓ | ✓ | ✓ | ✓ |
| admin token | ✓ | ✓ | ✓ | ✓ |
| owner's token | ✓ | ✓ | ✓ | ✓ |
| another owner's token | ✓ | 404 | 403 | 404 |
| anonymous | ✓ | 404 | 401 | 404 |
| unknown, garbage or revoked token | 401 | 401 | 401 | 401 |

Token listing and revocation (owner or admin only; 404/401/403 for others; revoked at once for
the API, git and MCP): `tokens.rs::tokens_are_listed_and_revoked_everywhere_at_once`.

Agent tokens (`agent:<name>`) land code but get 403 on every management route (tokens, owner,
repo settings and delete, review rules, reviewer, webhooks, queue pause, billing, provider,
review apps) and 404 on the owner-only reads. They review only as their own agent (another
`X-Roll-Agent` is 403): `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`.

## Security regressions

| Defect fixed | Test |
| --- | --- |
| owners' provider `base_url` on private addresses (SSRF; literals at set and use time, credentials in the URL) | `reviewer.rs::byok_endpoints_on_private_addresses_are_refused` |
| provider answers over 16 MiB, and redirects | `code-review/tests/engine.rs::oversized_answers_and_redirects_are_refused` |
| NAT64/6to4-wrapped private IPv4, `*.localhost` | `review::apps` unit `private_ranges` |
| approvals or change requests from GitHub users who can't push counted | `github.rs::reviews_both_ways` (author_association cases) |
| roll approving after skipping files, or incrementally with its findings open | `reviewer.rs::roll_doesnt_approve_what_it_didnt_read`, `code-review` unit `approvals_need_everything_read` |
| exponential `**` matching (search `path`, CODEOWNERS) | `code-review` unit `glob::many_double_stars_stay_fast`, `review::pattern` unit `many_double_stars_stay_fast` |
| custom reviewer prompt shown to public readers | `reviewer.rs::custom_prompt_is_for_writers` |
| markdown links with `\` or control characters (`/\evil.dev`) | `markdown.test.ts` "drops unsafe link targets" |
| last 4 characters of very short keys | `code-review` unit `secret::round_trip_and_binding` |

## git

| Behaviour | Tests |
| --- | --- |
| clone (empty and full), push, fetch, branch create and delete, tags (lightweight, annotated, include-tag) | `code-git/tests/git_cli.rs::clone_push_fetch_branch_delete`, `code-control/tests/git_more.rs::tags_branches_and_merge_commits` |
| protocol v0 clone and push | `git_cli.rs::protocol_v0_clone_and_push` |
| v2: capability advertisement, `ls-refs` (`ref-prefix`, `symrefs`, `peel`, `unborn`), fetch acknowledgments | `code-git/tests/http.rs::advertisements`, `http.rs::ls_refs_arguments`, `http.rs::gzip_bodies_and_protocol_errors` |
| shallow (depth, deepen, unshallow) and partial clones (`blob:none`, `tree:0`), through the generic walk and through changesets | `git_cli.rs::shallow_and_partial_clones`, `git_more.rs::shallow_partial_and_incremental_fetches` |
| gzip request bodies, gzip bombs, request size limits | `http.rs::gzip_bodies_and_protocol_errors`, `http.rs::request_limits` |
| ERR packets: deepen-since, unsupported filter, bad oid, unknown argument, unknown command, garbage | `http.rs::gzip_bodies_and_protocol_errors` |
| receive-pack: report-status, side-band, atomic, delete-refs, push options, pusher identity, custom `ng` reasons | `git_cli.rs::rejected_push_shows_reason`, `http.rs::push_options_and_pusher_reach_the_backend`, `git_more.rs::atomic_push_with_a_conflict_applies_nothing_else` |
| auth: 401 with challenge, Basic and Bearer, 403 for another owner, 404 for an unknown repo | `http.rs::advertisements`, `client.rs::auth_errors`, `git_cli.rs::auth_and_large_chunked_push`, `git_more.rs::git_auth_for_other_owners_and_anonymous` |
| packs: deltas (ofs and ref), thin packs, truncation and corruption, hostile sizes, delta bombs, written packs indexed by git | `code-git/tests/pack.rs` (all) |
| smart HTTP client: push and fetch round trip, rejected pushes, auth | `code-git/tests/client.rs` (all) |
| push to main lands (fast-forward); `refs/land/<branch>` rebases a stale push; conflicts name paths in git's output | `code-control/tests/git.rs::git_clone_push_land_and_conflict` |
| force push to main lands instead of rewriting history | `git_more.rs::force_push_to_main_lands_instead_of_rewriting` |
| branch push opens a change, repush revises it, delete closes it, a push after landing opens a new one; merge commits fast-forward; other ref namespaces refused | `git.rs::git_clone_push_land_and_conflict`, `git_more.rs::tags_branches_and_merge_commits` |

## code-store

| Invariant | Tests |
| --- | --- |
| byte-exact git round trip (merges, exec bit, symlink, gitlink, file↔dir, encoding, gpgsig, annotated tag); `objects_for_fetch` equals `rev-list --objects`; `fsck --strict` | `code-store/tests/roundtrip.rs::git_round_trip` |
| signed commits round-trip; a rebase drops the signature; import is idempotent; corrupt objects are refused | `store.rs::rebased_commits_drop_signatures_and_imports_are_idempotent` |
| lander: fast-forward and already landed, trivial rebase, 3-way merge, stacks in order, merge commits only fast-forward | `lander.rs` (all), `store.rs::merge_commits_only_fast_forward` |
| conflicts: Content (text and binary), DeleteModify, FileDirectory, Mode; identical edits and repeated deletes are clean; Missing at changeset creation | `lander.rs::overlapping_edits_merge_or_conflict`, `lander.rs::delete_modify_and_file_directory_conflicts`, `store.rs::mode_and_binary_conflicts_and_repeated_deletes` |
| path index: floor for side-branch bases, reset on non-fast-forward moves | `lander.rs::side_branch_base_is_merged_not_overwritten`, `lander.rs::ref_compare_and_swap` |
| refs CAS, resolve by ref, prefix and git commit, `NotFound` | `lander.rs::ref_compare_and_swap`, `store.rs::blobs_diffs_compare_and_ancestry` |
| blobs (git ids, size, missing), `Edit::Blob`, diff, unified diff, compare, ancestry (all parents and first-parent only) | `store.rs::blobs_diffs_compare_and_ancestry` |
| stats, `land_daily`, `land_rate`, `queue_depth`, global feed, concurrent refs | `store.rs::stats_feeds_and_concurrent_refs` |
| LRU, paths, encodings | `code-store/src/{lru,path,model,git}.rs` tests |
| 40,000 merges, 64 agents, with hot-file overlap | `throughput.rs::land_40k_merges` (release, in `pnpm test:code`) |
| blame equals `git blame --first-parent` (edits, insertions, deletions, a merge's side-branch line); maximal ranges; cut short says so; binary and missing files | `code-store/tests/blame.rs`, `blame::tests::maps_unchanged_lines` |

## GitHub

| Behaviour | Tests |
| --- | --- |
| App JWT, installation tokens (cached and minted again), pagination, writes, branch protection, rate limits and retries | `code-github/tests/github.rs` (all) |
| every webhook event and action, ignored ones, deleted refs, tags, malformed payloads, signature header forms, config errors | `code-github/tests/events.rs` (all), `github.rs::webhook_events`, `github.rs::webhook_signature` |
| mirror scheduler: coalescing, max delay, landings during a push, backoff, lost responses, GitHub ahead | `code-github/tests/mirror.rs` (all) |
| connect and import (byte-exact), issues → work, PRs → changes, signature and redelivery, check runs, landing closes the PR, 200 landings → ≤10 pushes, finished item closes the issue | `code-control/tests/github.rs::connect_import_webhooks_and_mirror` |
| pushes to GitHub's main: fast-forward, merge, pause on overlap | `github.rs::push_to_github_main_is_ingested_and_merged` |
| issue lifecycle (edit, labels and priority, not planned, reopen), comments, unknown events, PR synchronize and close (not echoed back), uninstall and reinstall, mirror error and recovery, delete stops the mirror | `github.rs::issue_pr_and_installation_lifecycles` |
| GraphQL: review threads paged by first comment, resolve and unresolve, `errors` answers are errors, GHES endpoint | `code-github/tests/github.rs::review_threads_over_graphql`, `client::graphql_tests` |
| threads resolved and unresolved in roll are resolved and unresolved on GitHub (node id looked up once, or taken from the thread webhook), once each; GitHub's own resolutions aren't echoed back | `code-control/tests/github.rs::reviews_both_ways` |
| `@roll` in PR conversations (writer runs and gets a PR comment; readers, strangers, edits, roll's own comments, plain issues and comments without a command do nothing; line commands get a pointer; learnings) and in inline review comments (answered in the thread and replied on GitHub; readers ignored) | `github.rs::roll_commands_from_github_comments` |

## Checks and previews

| Behaviour | Tests |
| --- | --- |
| roll.toml parsing and errors, env injection, host mapping, hop-by-hop headers | `code-preview/src/{config,preview,data,docker}.rs` tests |
| change checks, stale heads dropped, post-merge `broken-main` open, close and claimed, coalescing, broken roll.toml, no roll.toml, auto previews follow their change, resume after restart | `code-preview/tests/service.rs` (fake runner) |
| broken main: bisected to the culprit, a revert opened (item stays claimable), closed when main is green again; `[main] auto_revert` lands it; approved fork changes and queued changes run checks | `service.rs::post_merge_opens_one_broken_main_item_and_closes_it_on_green`, `service.rs::auto_revert_undoes_the_change_that_broke_main`, `service.rs::approved_fork_changes_and_queued_changes_run_their_checks` |
| HTTP and WebSocket proxying, unknown and closed previews, idle reaper, reconcile after restart | `code-preview/src/preview.rs::proxies_http_and_websockets_and_reconciles_after_restart` |
| real Docker: success, failure, timeout, broken config, no leftovers; preview through the host router; post-merge; cleanup on land | `code-preview/tests/docker.rs` (all) |

## CLI (`roll code`)

The CLI is covered by `roll-cli/tests/code.rs` and the unit tests in `roll-cli/src/code.rs`. The
tests run the real binary against a live API.

| Area | Commands and behaviour |
| --- | --- |
| Owners and repos | `owners [create]`, `name` (exit 1), `create` (private, default branch, taken), `repos`, `repo`, `clone-url`, `rename`, `delete` (refuses without `--yes` when unattended) |
| Reading | `ls`, `cat` (binary-safe), `log`, `show` |
| Landing | `land`: local files, `local=path`, exec bit, `--stdin-json` as a map or a whole body, `--delete`, conflict exits 2, usage errors |
| Changes and checks | `change create/list/show/revise/land/close`, `check` (status validated) |
| Merge queue | `queue` (show, `--watch`, needs a repo), `queue pause/resume`, `land` queued (exit 0), `change enqueue [--urgent]` (blocked: exit 1 with the blockers), `dequeue`, `draft`, `ready`, `approve`, `land --bypass` |
| Work | `work create/list/claim/show/heartbeat/release/complete`. `claim` prints only the id and exits 3 with no work |
| Tokens and watch | `token create`, `--token` vs `ROLL_CODE_TOKEN`, `watch` (lines and `--json`) |
| Pipes | a reader that closes early (`roll code … \| head`) ends `roll` quietly, no panic: `code.rs::a_closed_pipe_ends_quietly` |
| Webhooks, statuses, blame, jobs, token admin | `webhooks list/create/update/delete/ping/deliveries/redeliver` (secret shown once, nothing-to-change error), `status` (post, combined view, bad state), `blame` (aligned, one author per range, `@rev`, missing file), `jobs` (none; stage and groups after AI reviews), `token list` (no secrets, last use) and `revoke` (refused afterwards): `code.rs::webhooks_statuses_blame_jobs_and_tokens`, `code.rs::ai_reviewer_commands` |

## Dashboard (`apps/dashboard`, vitest)

| Area | Tests |
| --- | --- |
| API client: paths, encoding, methods, bodies, token getter, `before`, `ApiError`, non-JSON errors, 404 and 401 hooks, 409 conflicts, 204 claim, blobs | `src/lib/code-api.test.ts` |
| event stream: parsing (comments, CRLF, multi-line, ids), dispatch, token header, reconnects, unsubscribe | `src/lib/sse.test.ts`, `src/lib/code-api.test.ts` |
| live rate, pulse throttle, feed batching | `src/lib/live-rate.test.ts` |
| diff parsing and split pairing | `src/lib/diff.test.ts` |
| highlighting: language mapping, token classes, limits | `src/lib/highlight.test.ts` |
| time formatting | `src/lib/time.test.ts` |
| git proxy: target URL, header allowlist, forwarding headers, streaming body, dropped response headers, 401 passthrough, 502 | `src/lib/git-proxy.test.ts` |
| sign-in token storage and events | `src/lib/code-token.test.ts` |
| owner tokens list and revoke requests, token rows (prefix or id, last use) | `src/lib/code-api.test.ts`, `src/lib/ci.test.ts` |
| 404 and rename proxy: path classification, 404 rewrite, 308 redirect, API down, app routes untouched | `src/proxy.test.ts` (and checked against `next dev`: real 404 and 308) |
| reserved names: lowercase, unique, every top-level route, in sync with Rust | `src/lib/reserved-names.test.ts`, `code-control/src/names.rs::reserved_matches_the_dashboard` |

## Not covered by automated tests

- **Rendering React components** (pages, the sign-in dialog, the board, the diff view).
  - They are type-checked by `pnpm typecheck`, and the integration pass drove every page in
    headless Chrome.
  - Automated browser tests would put Chrome in `pnpm test:code`, which this machine's memory
    budget doesn't allow. Add Playwright when CI has room.
- **The `delete` confirmation prompt on a real terminal.** It needs a pty. The non-interactive
  refusal is tested.
- **S3-backed storage for code.** Tests use the local object store, the same `pg_storage::Objects`
  API that the other suites test against S3.
