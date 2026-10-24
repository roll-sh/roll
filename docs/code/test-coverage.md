# roll code — test coverage

Every feature of phases 1–3 maps to the tests that cover it: every route and the statuses it
answers, the auth matrix, every event, MCP tool and CLI command, every GitHub event, every merge
queue transition, every review rule and blocker, and every mode, command, error and job state of
roll's reviewer. `pnpm test:code` runs all of them: about 430 Rust tests across the code crates and
`roll-cli`, and about 440 vitest tests for the dashboard (node and jsdom).

`crates/code-control/tests/coverage_map.rs` keeps this map honest. It fails when a route the
routers declare, an MCP tool or an event kind has no row here naming a test, or when a test named
here doesn't exist. `roll-cli`'s `every_code_command_is_in_the_coverage_map` does the same for
every `roll code` command. When you add a feature, add its row.

Paths are relative to `crates/`, except the dashboard rows. `file::fn` names a test function; a
bare `file.rs` without a crate is code-control's `tests/` (or the crate the section is about).
## HTTP routes

Every method and path the routers declare (`api.rs`, `review/api.rs`, `ai/api.rs`, `mcp.rs` and
code-git's `/git/{*path}`), with every status it answers short of a 500. Errors are JSON `{error}`
everywhere, bad query strings included. 401 means no or a bad token, 403 a caller who can see the
thing but not do it, 404 one who can't see it (docs/code/http-api.md).

### Core

| Route | Statuses | Tests |
| --- | --- | --- |
| `GET /v1/code` | 200 | `api.rs::land_merge_and_conflict`, `contract.rs::auth_matrix`, `contract.rs::open_mode_makes_everyone_admin`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/names/{name}` | 200 | `api.rs::names_and_owners`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/names/{owner}/{repo}` | 200, 401, 404 | `api.rs::repos_rename_and_visibility`, `contract.rs::auth_matrix`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/owners` | 200, 401 | `contract.rs::auth_matrix`, `contract.rs::open_mode_makes_everyone_admin`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/owners` | 201, 400, 401, 403, 409, 422 | `api.rs::names_and_owners`, `api.rs::repos_rename_and_visibility`, `contract.rs::auth_matrix`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/owners/{owner}` | 200, 401, 404 | `api.rs::names_and_owners`, `api.rs::repos_rename_and_visibility`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `PATCH /v1/code/owners/{owner}` | 200, 401, 403, 404 | `api.rs::names_and_owners`, `contract.rs::auth_matrix`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/owners/{owner}/tokens` | 201, 401, 403, 404 | `contract.rs::auth_matrix`, `tokens.rs::tokens_are_listed_and_revoked_everywhere_at_once`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/owners/{owner}/tokens` | 200, 401, 404 | `tokens.rs::tokens_are_listed_and_revoked_everywhere_at_once`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `DELETE /v1/code/owners/{owner}/tokens/{id}` | 204, 401, 403, 404 | `tokens.rs::tokens_are_listed_and_revoked_everywhere_at_once`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos` | 200, 400, 401, 404 | `contract.rs::auth_matrix`, `api.rs::repos_rename_and_visibility`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos` | 201, 400, 401, 403, 404, 409, 422 | `api.rs::repos_rename_and_visibility`, `contract.rs::auth_matrix`, `contract.rs::errors_are_json_with_the_right_status`, `contract.rs::github_without_an_app`, `github.rs::connecting_needs_the_admin_and_fork_prs_are_untrusted`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}` | 200, 301, 401, 404 | `api.rs::repos_rename_and_visibility`, `contract.rs::auth_matrix`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `PATCH /v1/code/repos/{owner}/{repo}` | 200, 301, 400, 401, 403, 404, 409, 422 | `api.rs::repos_rename_and_visibility`, `contract.rs::auth_matrix`, `contract.rs::errors_are_json_with_the_right_status`, `queue.rs::queue_events_and_shapes`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `DELETE /v1/code/repos/{owner}/{repo}` | 204, 401, 403, 404 | `api.rs::repos_rename_and_visibility`, `contract.rs::auth_matrix`, `contract.rs::deleting_a_repo_removes_its_previews`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/refs` | 200, 301, 401, 404 | `api.rs::land_merge_and_conflict`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/tree/{rev}` | 200, 301, 401, 404 | `api.rs::land_merge_and_conflict`, `api.rs::repos_rename_and_visibility`, `contract.rs::auth_matrix`, `contract.rs::errors_are_json_with_the_right_status`, `contract.rs::state_survives_a_restart`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/tree/{rev}/` | 200, 301, 401, 404 | `contract.rs::writes_binary_content_deletes_and_blob_caching`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/tree/{rev}/{*path}` | 200, 401, 404 | `api.rs::land_merge_and_conflict`, `contract.rs::writes_binary_content_deletes_and_blob_caching`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/blob/{rev}/{*path}` | 200, 401, 404 | `contract.rs::writes_binary_content_deletes_and_blob_caching`, `contract.rs::errors_are_json_with_the_right_status`, `api.rs::readme_is_rendered_and_sanitized`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/readme/{rev}` | 200, 400, 401, 404 | `api.rs::readme_is_rendered_and_sanitized`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/blame/{rev}/{*path}` | 200, 401, 404 | `webhooks.rs::blame_matches_git`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/webhooks` | 200, 401, 403, 404 | `webhooks.rs::webhooks_deliver_signed_events`, `webhooks.rs::webhooks_need_writers_and_public_addresses`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/webhooks` | 201, 400, 401, 403, 404, 422 | `webhooks.rs::webhooks_deliver_signed_events`, `webhooks.rs::webhooks_need_writers_and_public_addresses`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/webhooks/{id}` | 200, 401, 403, 404 | `webhooks.rs::webhooks_deliver_signed_events`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `PATCH /v1/code/repos/{owner}/{repo}/webhooks/{id}` | 200, 400, 401, 403, 404 | `webhooks.rs::webhooks_deliver_signed_events`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `DELETE /v1/code/repos/{owner}/{repo}/webhooks/{id}` | 204, 401, 403, 404 | `webhooks.rs::webhooks_deliver_signed_events`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/webhooks/{id}/ping` | 202, 401, 403, 404 | `webhooks.rs::webhooks_deliver_signed_events`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/webhooks/{id}/deliveries` | 200, 401, 403, 404 | `webhooks.rs::webhooks_deliver_signed_events`, `webhooks.rs::webhooks_need_writers_and_public_addresses`, `webhooks.rs::deliveries_keep_the_start_of_the_response`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/webhooks/{id}/deliveries/{did}/redeliver` | 200, 401, 403, 404 | `webhooks.rs::webhooks_deliver_signed_events`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/statuses/{rev}` | 200, 401, 404 | `webhooks.rs::commit_statuses_become_checks`, `webhooks.rs::pushes_are_webhook_events`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/statuses/{rev}` | 201, 400, 401, 403, 404 | `webhooks.rs::commit_statuses_become_checks`, `webhooks.rs::webhooks_need_writers_and_public_addresses`, `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/paths/{rev}` | 200, 401, 404 | `api.rs::land_merge_and_conflict`, `contract.rs::paths_are_capped`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/log` | 200, 400, 401, 404 | `api.rs::land_merge_and_conflict`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/changesets` | 201, 400, 401, 403, 404 | `contract.rs::writes_binary_content_deletes_and_blob_caching`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/changesets/{id}` | 200, 401, 404 | `api.rs::land_merge_and_conflict`, `contract.rs::writes_binary_content_deletes_and_blob_caching`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/compare/{spec}` | 200, 400, 401, 404 | `api.rs::land_merge_and_conflict`, `contract.rs::errors_are_json_with_the_right_status`, `review.rs::review_events_and_diff_format`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/land` | 200, 202, 400, 401, 403, 404, 409, 422 | `api.rs::land_merge_and_conflict`, `contract.rs::auth_matrix`, `contract.rs::errors_are_json_with_the_right_status`, `queue.rs::bypass_needs_the_admin_token`, `review.rs::direct_landings_and_pushes_need_review`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/changes` | 200, 400, 401, 404 | `api.rs::changes_and_work`, `contract.rs::errors_are_json_with_the_right_status`, `contract.rs::lookups_by_number_and_id`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/changes` | 201, 400, 401, 403, 404 | `api.rs::changes_and_work`, `contract.rs::change_authors`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/changes/{number}` | 200, 401, 404 | `contract.rs::lookups_by_number_and_id`, `contract.rs::errors_are_json_with_the_right_status`, `api.rs::changes_and_work`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/work` | 200, 400, 401, 404 | `api.rs::changes_and_work`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/work` | 201, 301, 400, 401, 403, 404 | `api.rs::changes_and_work`, `contract.rs::errors_are_json_with_the_right_status`, `contract.rs::auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/work/claim` | 200, 204, 400, 401, 403, 404 | `api.rs::changes_and_work`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/work/{number}` | 200, 401, 404 | `contract.rs::lookups_by_number_and_id`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/events` | 200, 401, 404, 429 | `streams.rs::each_token_gets_a_bounded_number_of_streams`, `streams.rs::anonymous_streams_count_per_client_address_and_in_total`, `streams.rs::a_trusted_proxy_keys_anonymous_streams_by_forwarded_address`, `streams.rs::open_daemons_count_everyone_by_address`, `api.rs::events_stream_landings_and_changes`, `contract.rs::event_shapes_and_previews`, `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/queue` | 200, 401, 404 | `queue.rs::queue_events_and_shapes`, `queue.rs::bypass_needs_the_admin_token`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/queue/pause` | 200, 401, 403, 404 | `queue.rs::pause_and_resume`, `queue.rs::bypass_needs_the_admin_token`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`, `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/repos/{owner}/{repo}/queue/resume` | 200, 401, 403, 404 | `queue.rs::pause_and_resume`, `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/repos/{owner}/{repo}/audit` | 200, 401, 403, 404 | `queue.rs::bypass_needs_the_admin_token`, `queue.rs::blockers_keep_changes_out_of_the_queue`, `queue.rs::lanes_can_change_without_losing_place`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/changes/{id}` | 200, 401, 404 | `api.rs::changes_and_work`, `contract.rs::auth_matrix`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `PATCH /v1/code/changes/{id}` | 200, 400, 401, 403, 404 | `queue.rs::blockers_keep_changes_out_of_the_queue`, `queue.rs::post_merge_repos_land_directly_but_respect_drafts`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/changes/{id}/enqueue` | 202, 400, 401, 403, 404, 409, 422 | `queue.rs::batches_land_together_in_fifo_order`, `queue.rs::urgent_lane_goes_first`, `queue.rs::blockers_keep_changes_out_of_the_queue`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `DELETE /v1/code/changes/{id}/enqueue` | 200, 401, 403, 404, 409 | `queue.rs::dequeueing_a_change_under_test_rebuilds_the_batch`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `PATCH /v1/code/changes/{id}/enqueue` | 200, 400, 401, 403, 404, 409 | `queue.rs::lanes_can_change_without_losing_place`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/changes/{id}/approve-run` | 200, 401, 403, 404 | `queue.rs::blockers_keep_changes_out_of_the_queue`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/changes/{id}/revise` | 200, 400, 401, 403, 404, 409 | `api.rs::changes_and_work`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/changes/{id}/land` | 200, 202, 400, 401, 403, 404, 409, 422 | `api.rs::changes_and_work`, `contract.rs::event_shapes_and_previews`, `contract.rs::auth_matrix`, `contract.rs::errors_are_json_with_the_right_status`, `review.rs::ai_approvals_and_land_change_gate`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/changes/{id}/close` | 200, 401, 403, 404, 409 | `api.rs::changes_and_work`, `contract.rs::auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/changes/{id}/checks` | 200, 400, 401, 403, 404, 409 | `api.rs::changes_and_work`, `contract.rs::errors_are_json_with_the_right_status`, `contract.rs::auth_matrix`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/changes/{id}/preview` | 200, 401, 403, 404, 422 | `contract.rs::event_shapes_and_previews`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `DELETE /v1/code/changes/{id}/preview` | 200, 401, 403, 404 | `contract.rs::event_shapes_and_previews`, `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/work/{id}` | 200, 401, 404 | `api.rs::changes_and_work`, `contract.rs::auth_matrix`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `PATCH /v1/code/work/{id}` | 200, 400, 401, 403, 404 | `api.rs::changes_and_work`, `contract.rs::lookups_by_number_and_id`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/work/{id}/heartbeat` | 200, 401, 403, 404, 409 | `api.rs::changes_and_work`, `contract.rs::errors_are_json_with_the_right_status`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/work/{id}/release` | 200, 401, 403, 404, 409 | `contract.rs::lookups_by_number_and_id`, `contract.rs::auth_matrix`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/work/{id}/complete` | 200, 401, 403, 404, 409 | `api.rs::changes_and_work`, `contract.rs::lookups_by_number_and_id`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::statuses_of_every_route`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/github` | 200 | `contract.rs::github_without_an_app`, `routes.rs::github_webhook_statuses`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/github/installations` | 200, 401, 403 | `contract.rs::auth_matrix`, `contract.rs::github_without_an_app`, `github.rs::connect_import_webhooks_and_mirror`, `github.rs::connecting_needs_the_admin_and_fork_prs_are_untrusted`, `routes.rs::every_repo_route_follows_the_auth_matrix`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/github/webhook` | 200, 400, 401, 404 (202 untested: see below) | `github.rs::connect_import_webhooks_and_mirror`, `github.rs::pull_request_issue_and_installation_actions`, `routes.rs::github_webhook_statuses`, `contract.rs::github_without_an_app`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `GET /v1/code/github/setup` | 302 | `contract.rs::github_without_an_app`, `routes.rs::bad_tokens_are_401_on_every_route` |
| `POST /v1/code/mcp` | 200, 202, 400, 401, 403, 404, 429 | `mcp.rs::transport_sessions_and_protocol`, `mcp.rs::protocol_edges`, `mcp.rs::responses_as_server_sent_events`, `mcp.rs::tools_follow_the_whole_auth_matrix`, `mcp.rs::invalid_tokens_and_foreign_sessions`, `mcp.rs::tools_enforce_the_routes_authorization`, `mcp.rs::sessions_work_through_any_node`, `tokens.rs::tokens_are_listed_and_revoked_everywhere_at_once`, `streams.rs::mcp_event_stream_answers_share_the_callers_slots` |
| `GET /v1/code/mcp` | 405 | `mcp.rs::transport_sessions_and_protocol`, `mcp.rs::protocol_edges` |
| `DELETE /v1/code/mcp` | 204, 400, 404 | `mcp.rs::transport_sessions_and_protocol`, `mcp.rs::tools_follow_the_whole_auth_matrix`, `mcp.rs::invalid_tokens_and_foreign_sessions`, `mcp.rs::sessions_work_through_any_node` |
| `GET /git/{*path}` | 200, 401, 403, 404 | `git_more.rs::git_auth_matrix_over_smart_http`, `git.rs::git_private_repo_needs_token`, `code-git/tests/http.rs::advertisements` |
| `POST /git/{*path}` | 200, 400, 401, 403, 404, 405, 413, 503 | `git_more.rs::git_auth_matrix_over_smart_http`, `git.rs::git_clone_push_land_and_conflict`, `code-git/tests/http.rs::advertisements`, `code-git/tests/http.rs::gzip_bodies_and_protocol_errors`, `code-git/tests/http.rs::request_limits`, `code-git/tests/memory.rs::fetches_are_charged_and_requests_without_memory_fail_cleanly` |

### Reviews

| Route | Statuses | Tests |
| --- | --- | --- |
| `GET /v1/code/repos/{owner}/{repo}/review-settings` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::auth_matrix`, `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::contract_details_caller_comment_index_renames_and_apps` |
| `PATCH /v1/code/repos/{owner}/{repo}/review-settings` | 200, 400, 401, 403, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::rules_matrix`, `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::review_apps_lifecycle_and_deliveries`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate` |
| `GET /v1/code/changes/{id}/reviews` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::auth_matrix` |
| `POST /v1/code/changes/{id}/reviews` | 201, 400, 401, 403, 404, 409 | `review.rs::every_review_route_answers_every_status`, `review.rs::rules_matrix`, `review.rs::auth_matrix`, `review.rs::contract_details_caller_comment_index_renames_and_apps`, `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |
| `POST /v1/code/changes/{id}/reviews/{rid}/dismiss` | 200, 400, 401, 403, 404, 409 | `review.rs::every_review_route_answers_every_status`, `review.rs::rules_matrix`, `review.rs::auth_matrix` |
| `GET /v1/code/changes/{id}/threads` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::auth_matrix` |
| `POST /v1/code/changes/{id}/threads` | 201, 400, 401, 404, 409 | `review.rs::every_review_route_answers_every_status`, `review.rs::threads_suggestions_and_carry_forward`, `review.rs::auth_matrix` |
| `GET /v1/code/changes/{id}/review-status` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::auth_matrix` |
| `GET /v1/code/changes/{id}/review-requests` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::review_requests_and_pending_reviews` |
| `POST /v1/code/changes/{id}/review-requests` | 201, 400, 401, 403, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::review_requests_and_pending_reviews`, `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |
| `GET /v1/code/threads/{id}` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::auth_matrix` |
| `POST /v1/code/threads/{id}/comments` | 201, 400, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::auth_matrix` |
| `POST /v1/code/threads/{id}/resolve` | 200, 401, 403, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::auth_matrix`, `review.rs::threads_suggestions_and_carry_forward` |
| `POST /v1/code/threads/{id}/unresolve` | 200, 401, 403, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::auth_matrix` |
| `POST /v1/code/threads/{id}/apply` | 200, 401, 403, 404, 409, 422 | `review.rs::every_review_route_answers_every_status`, `review.rs::threads_suggestions_and_carry_forward` |
| `GET /v1/code/owners/{owner}/review-apps` | 200, 401, 403, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::auth_matrix`, `review.rs::review_apps_lifecycle_and_deliveries` |
| `POST /v1/code/owners/{owner}/review-apps` | 201, 400, 401, 403, 404, 409, 422 | `review.rs::every_review_route_answers_every_status`, `review.rs::review_apps_lifecycle_and_deliveries` |
| `GET /v1/code/review-apps/{id}` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::review_apps_lifecycle_and_deliveries` |
| `PATCH /v1/code/review-apps/{id}` | 200, 400, 401, 404, 409 | `review.rs::every_review_route_answers_every_status`, `review.rs::review_apps_lifecycle_and_deliveries` |
| `DELETE /v1/code/review-apps/{id}` | 204, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::review_apps_lifecycle_and_deliveries` |
| `POST /v1/code/review-apps/{id}/rotate` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::review_apps_lifecycle_and_deliveries` |
| `GET /v1/code/review-apps/{id}/deliveries` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |
| `POST /v1/code/review-apps/{id}/deliveries/{did}/redeliver` | 200, 401, 404 | `review.rs::every_review_route_answers_every_status`, `review.rs::review_apps_lifecycle_and_deliveries`, `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |

### roll's reviewer

| Route | Statuses | Tests |
| --- | --- | --- |
| `GET /v1/code/pricing` | 200 | `reviewer.rs::owner_reviewer_routes_answer_every_status`, `reviewer.rs::pricing_caps_allowances_and_byok` |
| `GET /v1/code/repos/{owner}/{repo}/reviewer` | 200, 404 | `reviewer.rs::repo_reviewer_routes_answer_every_status`, `reviewer.rs::custom_prompt_is_for_writers`, `reviewer.rs::triggers_settings_and_auth` |
| `PATCH /v1/code/repos/{owner}/{repo}/reviewer` | 200, 400, 401, 403, 404 | `reviewer.rs::repo_reviewer_routes_answer_every_status`, `reviewer.rs::triggers_settings_and_auth`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate` |
| `GET /v1/code/repos/{owner}/{repo}/learnings` | 200, 404 | `reviewer.rs::repo_reviewer_routes_answer_every_status`, `reviewer.rs::learnings_from_resolving_and_dismissing` |
| `POST /v1/code/repos/{owner}/{repo}/learnings` | 201, 400, 401, 403, 404 | `reviewer.rs::repo_reviewer_routes_answer_every_status`, `reviewer.rs::learnings_from_resolving_and_dismissing` |
| `DELETE /v1/code/repos/{owner}/{repo}/learnings/{id}` | 204, 401, 403, 404 | `reviewer.rs::repo_reviewer_routes_answer_every_status`, `reviewer.rs::learnings_from_resolving_and_dismissing` |
| `GET /v1/code/repos/{owner}/{repo}/search` | 200, 400, 404, 422, 503 | `reviewer.rs::repo_reviewer_routes_answer_every_status`, `reviewer.rs::search_is_unprocessable_without_the_index`, `reviewer.rs::auto_mode_deep_context_and_classifier`, `webhooks.rs::search_at_a_change_head`, `index_scale.rs::searches_answer_503_while_a_first_build_runs_in_the_background` |
| `POST /v1/code/changes/{id}/ai-review` | 202, 400, 401, 403, 404, 409 | `reviewer.rs::change_reviewer_routes_answer_every_status_and_jobs_queue`, `reviewer.rs::triggers_settings_and_auth` |
| `GET /v1/code/changes/{id}/ai-jobs` | 200, 404 | `reviewer.rs::change_reviewer_routes_answer_every_status_and_jobs_queue`, `reviewer.rs::triggers_settings_and_auth` |
| `GET /v1/code/owners/{owner}/review-provider` | 200, 404 | `reviewer.rs::owner_reviewer_routes_answer_every_status`, `reviewer.rs::pricing_caps_allowances_and_byok`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate` |
| `PUT /v1/code/owners/{owner}/review-provider` | 200, 400, 401, 403, 404, 422 | `reviewer.rs::owner_reviewer_routes_answer_every_status`, `reviewer.rs::byok_needs_a_server_secret`, `reviewer.rs::byok_endpoints_on_private_addresses_are_refused`, `reviewer.rs::pricing_caps_allowances_and_byok` |
| `DELETE /v1/code/owners/{owner}/review-provider` | 204, 401, 403, 404 | `reviewer.rs::owner_reviewer_routes_answer_every_status`, `reviewer.rs::pricing_caps_allowances_and_byok` |
| `GET /v1/code/owners/{owner}/billing` | 200, 404 | `reviewer.rs::owner_reviewer_routes_answer_every_status`, `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate` |
| `PUT /v1/code/owners/{owner}/billing` | 200, 400, 401, 403, 404 | `reviewer.rs::owner_reviewer_routes_answer_every_status`, `reviewer.rs::pricing_caps_allowances_and_byok` |
| `GET /v1/code/owners/{owner}/usage` | 200, 400, 404 | `reviewer.rs::owner_reviewer_routes_answer_every_status`, `reviewer.rs::reviews_a_change_automatically_with_the_right_request`, `reviewer.rs::pricing_caps_allowances_and_byok` |

### Behaviours across routes

| Behaviour | Tests |
| --- | --- |
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
| bad query strings answer a JSON 400 (core and reviewer routes) | `routes.rs::statuses_of_every_route`, `reviewer.rs::bad_query_strings_are_json_400s` |

## Auth matrix

Every cell is asserted for the API, git and MCP. API: `contract.rs::auth_matrix` and, for all 55
repo, change and work routes, `routes.rs::every_repo_route_follows_the_auth_matrix`; git:
`git_more.rs::git_auth_matrix_over_smart_http` (plus `git.rs::git_private_repo_needs_token`,
`git_more.rs::git_auth_for_other_owners_and_anonymous`); MCP: `mcp.rs::tools_follow_the_whole_auth_matrix`
(plus `mcp.rs::every_tool_is_authorized_like_its_route`, `mcp.rs::tools_enforce_the_routes_authorization`).
Open mode: `contract.rs::open_mode_makes_everyone_admin`.

| Caller | Read public | Read private | Write public | Write private |
| --- | --- | --- | --- | --- |
| open mode (no `ROLL_CODE_TOKEN`) | ✓ | ✓ | ✓ | ✓ |
| admin token | ✓ | ✓ | ✓ | ✓ |
| owner's token | ✓ | ✓ | ✓ | ✓ |
| another owner's token | ✓ | 404 | 403 | 404 |
| anonymous | ✓ | 404 (git: 401 with a Basic challenge, so git sends credentials) | 401 | 404 (git: 401) |
| unknown, garbage or revoked token | 401 | 401 | 401 | 401 |

- Unknown, garbage, revoked and unknown `rla_` tokens get 401 on every route that looks at the
  caller: `routes.rs::bad_tokens_are_401_on_every_route`, `tokens.rs::tokens_are_listed_and_revoked_everywhere_at_once`.
- Token listing and revocation (owner or admin only; 404/401/403 for others; revoked at once for
  the API, git and MCP): `tokens.rs::tokens_are_listed_and_revoked_everywhere_at_once`.
- Agent tokens (`agent:<name>`) land code and read private repos, but get 403 on every management
  route (tokens, owner, repo settings and delete, review rules, reviewer, webhooks, queue
  pause/resume, approve-run, billing, provider, review apps) and 404 on the owner-only reads. They
  review only as their own agent: `tokens.rs::agent_tokens_work_code_but_cant_manage_or_impersonate`,
  `routes.rs::every_repo_route_follows_the_auth_matrix`.
- Review-app tokens read the repos the app is installed on (private ones too) and get 403 on every
  write and management route there, 404 elsewhere; owner and admin routes refuse them; they can
  approve or request changes only where installed: `routes.rs::every_repo_route_follows_the_auth_matrix`,
  `review.rs::app_tokens_are_scoped_even_on_an_open_daemon`,
  `review.rs::review_apps_events_scopes_requests_and_failed_deliveries`.
- Anonymous writes are 401 (the dashboard opens its sign-in prompt), never 403, including owner
  create, owner patch and token create: `api.rs::repos_rename_and_visibility`, `routes.rs::every_repo_route_follows_the_auth_matrix`.

## Events

Every kind `Code::emit` sends is in `webhooks::EVENTS`, and
`webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks` produces each one and checks
that SSE and the webhook delivery carry the same data, with exact key sets, inside the delivery
envelope `{event, delivery, webhook, repo{id, full_name, default_branch, visibility}, data}`.

| Event | Asserted | Tests |
| --- | --- | --- |
| `landed` | exact keys, `change`, `count: 1`; durable delivery (bursts, restarts, no history for new hooks) | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `contract.rs::event_shapes_and_previews`, `api.rs::events_stream_landings_and_changes`, `webhooks.rs::webhooks_deliver_signed_events`, `webhooks.rs::landed_deliveries_survive_a_burst_larger_than_the_broadcast_channel`, `webhooks.rs::landings_while_the_daemon_was_down_are_delivered_after_a_restart`, `webhooks.rs::a_new_webhook_gets_no_history` |
| `change` | every `Change` field | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `contract.rs::event_shapes_and_previews`, `webhooks.rs::webhooks_deliver_signed_events` |
| `work` | exact `WorkItem` keys | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `contract.rs::event_shapes_and_previews`, `webhooks.rs::webhooks_deliver_signed_events` |
| `check` `{change, check}` | exact keys of the event and of `Check` | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `contract.rs::event_shapes_and_previews`, `webhooks.rs::commit_statuses_become_checks` |
| `check` `{changeset, status}` | exact keys, equal to the POST response | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks` |
| `preview` | `Preview` keys; deleting sends `preview: null` | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `contract.rs::event_shapes_and_previews` |
| `queue` | exact `{state, paused, depth, batch}`; every state including `landing` | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `queue.rs::queue_events_and_shapes`, `queue.rs::pausing_lets_the_batch_under_test_finish` |
| `review` | exact `Review` keys (no `threads`); repeated once the `ai` record is attached | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `webhooks.rs::webhooks_deliver_signed_events`, `review.rs::review_events_and_diff_format` |
| `thread` | exact `Thread` keys, linked to its review | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `review.rs::review_events_and_diff_format` |
| `push` | created, refused (`accepted: false`, `reason`) and deleted refs | `webhooks.rs::every_event_is_the_same_on_the_stream_and_in_webhooks`, `webhooks.rs::pushes_are_webhook_events` |
| `ping` (webhooks only) | signed, on creation and on request | `webhooks.rs::webhooks_deliver_signed_events` |
| per-repo isolation, private feeds | | `api.rs::events_stream_landings_and_changes`, `contract.rs::event_shapes_and_previews` |
| stream limits: per token, per anonymous address, forwarded address when trusted, open daemons by address, server total, slot freed on close, refused requests take none, MCP event-stream answers share the slots | 429 with `Retry-After` and a JSON error | `streams.rs` (all) |

## MCP (`code-control/tests/mcp.rs`)

### Tools

| Tool | Behaviours covered | Tests |
| --- | --- | --- |
| `search_code` | default branch, `rev` (change head), regex, case_sensitive, path glob, limit → same JSON as `GET /search`; missing query/repo, bad repo formats, bad regex, ill-typed limit; auth matrix | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `read_file` | default branch, `rev` (changeset, slash branch), line ranges, 2,000-line window with `next`, past the end, binary, text block is the file; not found (path, rev); missing path/repo; auth; SSE answer | `mcp.rs::every_tool_end_to_end`, `mcp.rs::read_file_ranges_and_binary_files`, `mcp.rs::land_outcomes_queued_branches_and_errors`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route`, `mcp.rs::tools_enforce_the_routes_authorization`, `mcp.rs::responses_as_server_sent_events` |
| `list_tree` | root, subdirectory, `rev` → same JSON as `GET /tree`; unknown path; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::land_outcomes_queued_branches_and_errors`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `log` | limit (default 30), path, rev, cursor paging → same JSON as `GET /log`; unknown rev, ill-typed limit; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::land_outcomes_queued_branches_and_errors`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `blame` | ranges, default branch and `rev` → same JSON as `GET /blame`; missing path, unknown file; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::land_outcomes_queued_branches_and_errors`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `get_changeset` | by id → same JSON as `GET /changesets/{id}`; missing id, unknown id; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `compare` | changesets and files → same JSON as `GET /compare/a...b`; missing base/head, unknown head; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `create_change` | files, body, draft, message defaults to the title, `work_item` as id / `owner/repo#N` / number (item goes to review), unknown work item; missing title, bad files/mode, missing base; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `revise_change` | by `owner/repo#N` and id; missing/malformed/unknown change, bad files; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `land` | landed (direct, change id, `owner/repo#N`, `ref` branch), queued (position, urgent lane, direct), conflict (paths), blocked (blockers), landing twice = isError 409; malformed/unknown change, bad lane, bad bypass; auth (both routes) | `mcp.rs::every_tool_end_to_end`, `mcp.rs::land_outcomes_queued_branches_and_errors`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route`, `mcp.rs::tools_enforce_the_routes_authorization` |
| `get_change` | change + review_status + threads, each equal to its route; by id and `owner/repo#N`; missing/malformed/unknown; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `list_changes` | all and by state → same JSON as `GET /changes`; bad state; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::land_outcomes_queued_branches_and_errors`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `claim_work` | `{work: null}` (none, none with label), labels, lease_s; missing agent, ill-typed labels; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `heartbeat_work` | by `owner/repo#N`; another agent's lease is an error; missing/malformed/unknown work; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `release_work` | by id and `owner/repo#N`; missing/malformed/unknown work; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `complete_work` | by id and `owner/repo#N`; missing/malformed/unknown work; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `list_work` | state, label (and no match) → same JSON as `GET /work`; bad state; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `create_work` | title, body, labels, priority; missing title, bad priority/labels; repo errors; "needs a token"; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::tools_enforce_the_routes_authorization`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `comment` | new thread (line, range, side new/old), reply, as an `X-Roll-Agent`; missing change and thread, malformed/unknown change, unknown thread, missing body/path, bad line/side; another owner may comment on a public repo; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `review` | approve with inline comments, request_changes, comment; reviewer from `X-Roll-Agent` / `X-Roll-User`; missing/bad verdict, bad comments, malformed/unknown change; another owner: comment allowed, approve "not allowed"; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `queue_status` | post_merge; queue repo (depth, urgent first) → same JSON as `GET /queue`; missing repo; repo errors; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::land_outcomes_queued_branches_and_errors`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::protocol_edges`, `mcp.rs::every_tool_is_authorized_like_its_route` |
| `request_ai_review` | queued job (mode, full) visible in `GET /ai-jobs`; bad mode, malformed/unknown change; auth | `mcp.rs::every_tool_end_to_end`, `mcp.rs::tools_map_onto_their_routes`, `mcp.rs::tool_argument_errors`, `mcp.rs::every_tool_is_authorized_like_its_route` |

### Protocol

| Behaviour | Tests |
| --- | --- |
| sessions (400 without, 404 unknown, ended, bound to the token), version negotiation, `initialized`, ping, tools/list, unknown method and tool, non-object arguments, parse errors, wrong `jsonrpc`, batches with notifications, `MCP-Protocol-Version`, Origin, `GET` 405, `DELETE` | `mcp.rs::transport_sessions_and_protocol` |
| tools enforce the routes' authorization: anonymous public reads, private 404, writes need a token, another owner can't write, admin can | `mcp.rs::tools_enforce_the_routes_authorization` |
| every tool's schema is closed and its required fields exist; path and query encoding | `code-control/src/mcp.rs` unit tests |
| `Accept: text/event-stream`: one `message` event per response (single and batch), notifications 202, sessions still required, initialize answers JSON | `mcp.rs::responses_as_server_sent_events` |
| a revoked token ends its session (401) and can't start one | `tokens.rs::tokens_are_listed_and_revoked_everywhere_at_once` |
| all four versions and the default, initialize inside a batch (400), empty batch, client responses and notification-only batches (202), one bad entry in a batch, `resources/list` and `prompts/list`, `tools/call` without arguments or a name, Origin edge cases, `GET` `Allow`, `DELETE` without a session | `mcp.rs::protocol_edges` |
| a never-valid token: 401 on initialize, and it ends the session it's used on; anonymous callers can't use or end another token's session | `mcp.rs::invalid_tokens_and_foreign_sessions` |
| `tools/list` is exactly the 22 tools of mcp.md, each with `readOnlyHint` | `mcp.rs::every_tool_end_to_end` |
| event-stream answers hold one of the caller's stream slots (429 past the limit) | `streams.rs::mcp_event_stream_answers_share_the_callers_slots` |

## CLI (`roll code`)

The CLI is covered by `roll-cli/tests/code.rs`, which runs the real binary against a live API, and
the unit tests in `roll-cli/src/code.rs`. Exit codes: 0 success, 1 any error (usage errors
included), 2 a conflict to rebase, 3 `work claim` with nothing to claim.

| Command | Behaviours / exit codes covered | Tests |
| --- | --- | --- |
| `roll code owners` | list text and `--json` | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code owners create` | `--org`, `--user`, `--display-name`; taken and invalid exit 1 | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code name` | available exit 0; taken/reserved/invalid exit 1 with the reason | `roll-cli/tests/code.rs::owners_repos_and_reading` |
| `roll code repos` | `--owner`, all, `ls-repos` alias, empty, closed pipe | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges`, `roll-cli/tests/code.rs::a_closed_pipe_ends_quietly` |
| `roll code create` | `--private`, `-d`, `--default-branch`; taken exit 1, bad name; `--github` without `--installation` refused; `--github` malformed or no App: exit 1 | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges`, `roll-cli` `code::tests::args` |
| `roll code repo` | text (empty head, clone, landing policy, stats); not found and malformed exit 1 | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code rename` | renames; onto a taken name exit 1 | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code delete` | refuses without `--yes` unattended, `--yes`, missing repo exit 1 | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code clone-url` | prints the URL (`.git` accepted) | `roll-cli/tests/code.rs::owners_repos_and_reading` |
| `roll code ls` | root, `@rev path`, `@branch`, exec bit | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code cat` | default, `@cs`, `@branch`, binary-safe; missing exit 1 | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::landing_changes_checks_and_conflicts`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code log` | `-n`, `--path`, `@branch` | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code show` | text diff, `--json`; unknown changeset exit 1 | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code land` | local files, `local=path`, exec bit, `--author`, `--stdin-json` (map / whole body / not JSON / not an object / bad `files`), `--delete`, binary, `--ref`; conflict exit 2 (text and `--json`); queued exit 0; usage errors exit 1 | `roll-cli/tests/code.rs::owners_repos_and_reading`, `roll-cli/tests/code.rs::landing_changes_checks_and_conflicts`, `roll-cli/tests/code.rs::merge_queue_commands`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code change list` | text, `--state`, `ls` alias | `roll-cli/tests/code.rs::landing_changes_checks_and_conflicts`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code change create` | from files, `--changeset`, `--work` id and number; unknown `--work` exit 1; `--changeset` with files refused | `roll-cli/tests/code.rs::landing_changes_checks_and_conflicts`, `roll-cli/tests/code.rs::change_and_work_edges`, `roll-cli` `code::tests::args` |
| `roll code change show` | by number, text; unknown exit 1 | `roll-cli/tests/code.rs::landing_changes_checks_and_conflicts`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code change revise` | from files, default message, `--changeset`; closed change exit 1 | `roll-cli/tests/code.rs::landing_changes_checks_and_conflicts`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code change land` | landed; conflict exit 2; blocked, draft, already landed or closed exit 1; queued exit 0; `--bypass` | `roll-cli/tests/code.rs::landing_changes_checks_and_conflicts`, `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::merge_queue_commands`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code change enqueue` | `--urgent` position 1; draft blocked exit 1 with the blockers | `roll-cli/tests/code.rs::merge_queue_commands` |
| `roll code change dequeue` | dequeued; not queued exit 1 | `roll-cli/tests/code.rs::merge_queue_commands`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code change approve` | approved | `roll-cli/tests/code.rs::merge_queue_commands` |
| `roll code change draft` | draft; blocks enqueue and land | `roll-cli/tests/code.rs::merge_queue_commands`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code change ready` | ready, then it lands or enqueues | `roll-cli/tests/code.rs::merge_queue_commands`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code change close` | `--json` state closed; text | `roll-cli/tests/code.rs::landing_changes_checks_and_conflicts`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code check` | `--url`/`--summary`, by id and number; bad status and unknown change exit 1 | `roll-cli/tests/code.rs::landing_changes_checks_and_conflicts`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code work list` | all, `--label`, `--state`, `ls` alias | `roll-cli/tests/code.rs::work_queue_tokens_and_watch`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code work create` | `--label`, `-p`, `--body`; `-p 4` exit 1 | `roll-cli/tests/code.rs::work_queue_tokens_and_watch`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code work claim` | prints only the id, priority order, `--label`, `--lease`, `ROLL_AGENT`; no work exit 3; no agent exit 1 | `roll-cli/tests/code.rs::work_queue_tokens_and_watch`, `roll-cli/tests/code.rs::change_and_work_edges`, `roll-cli` `code::tests::args` |
| `roll code work show` | by id and number; unknown and malformed exit 1 | `roll-cli/tests/code.rs::work_queue_tokens_and_watch`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code work heartbeat` | by id and number, `ROLL_AGENT`; another agent's lease and not claimed exit 1 | `roll-cli/tests/code.rs::work_queue_tokens_and_watch`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code work release` | `--json`, text by number; another agent exit 1 | `roll-cli/tests/code.rs::work_queue_tokens_and_watch`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code work complete` | `--json`, text by number; malformed ref exit 1 | `roll-cli/tests/code.rs::work_queue_tokens_and_watch`, `roll-cli/tests/code.rs::change_and_work_edges` |
| `roll code watch` | a line per landing, `--json` events; missing repo exit 1 | `roll-cli/tests/code.rs::work_queue_tokens_and_watch`, `roll-cli/tests/code.rs::queue_watch_follows_the_queue` |
| `roll code queue` | show (text, `--json`), needs a repo, `--watch` (queue then events; `--json`); missing repo exit 1 | `roll-cli/tests/code.rs::merge_queue_commands`, `roll-cli/tests/code.rs::queue_watch_follows_the_queue` |
| `roll code queue pause` | prints the paused queue | `roll-cli/tests/code.rs::merge_queue_commands` |
| `roll code queue resume` | `--json` paused false | `roll-cli/tests/code.rs::merge_queue_commands` |
| `roll code token create` | prints `rlc_…`, `--token` vs `ROLL_CODE_TOKEN`; unknown owner exit 1 | `roll-cli/tests/code.rs::work_queue_tokens_and_watch`, `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code token list` | no secrets, prefix, last use, empty | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code token revoke` | refused at once afterwards; unknown id exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::repo_land_and_usage_edges` |
| `roll code review` | status, comment with inline comments as an agent, approve, request, request-changes, dismiss, `--ai --mode --full`; usage and unknown targets exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code thread list` | all, `--open`, ranges and `(old)`; unknown change exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code thread open` | `--line`, `--start-line`, `--side old` | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code thread reply` | `--agent`, a suggestion; unknown thread exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code thread resolve` | resolves; unknown exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code thread unresolve` | unresolves | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code thread apply` | latest suggestion, `--comment`; unknown comment exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code review-settings` | show, `--set`; invalid and malformed exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code review-apps list` | lists; unknown owner exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code review-apps create` | `--event`, `--repo`, `--required`; bad event exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code review-apps show` | unknown exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code review-apps update` | `--required`, `--event`, `--active`, `--url`, `--repo` | `roll-cli/tests/code.rs::reviews_threads_and_review_apps`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code review-apps rotate` | new token and secret | `roll-cli/tests/code.rs::reviews_threads_and_review_apps` |
| `roll code review-apps deliveries` | `--json` list | `roll-cli/tests/code.rs::reviews_threads_and_review_apps` |
| `roll code review-apps redeliver` | unknown delivery exit 1 | `roll-cli/tests/code.rs::reviews_threads_and_review_apps` |
| `roll code review-apps delete` | deletes | `roll-cli/tests/code.rs::reviews_threads_and_review_apps` |
| `roll code reviewer` | show, `--enable`, `--disable`, `--mode`, `--speed`, `--set`; bad values exit 1 | `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code usage` | current month, `--month`; unknown owner exit 1 | `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code billing` | show, `--cap`, `none`, `--on-cap`; bad values exit 1 | `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code provider set` | anthropic, openai_compatible, key from `ROLL_REVIEW_API_KEY`, never printed; errors exit 1 | `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code provider show` | key_last4 only, hosted | `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code provider clear` | back to hosted | `roll-cli/tests/code.rs::ai_reviewer_commands` |
| `roll code learnings list` | empty, path scope | `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code learnings add` | `--path`, repo-wide | `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code learnings rm` | forgets; unknown exit 1 | `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code webhooks list` | text, `--json`, none | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code webhooks create` | generated secret once, `--event`, own `--secret` (not echoed), `--inactive`; bad input exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code webhooks update` | `--pause`, `--resume`, `--event`, `--url`, `--secret`; nothing to change exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code webhooks delete` | deletes; unknown exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code webhooks ping` | receiver gets `ping`; unknown exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code webhooks deliveries` | state, response body; unknown exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code webhooks redeliver` | same delivery again; unknown exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code status` | post, combined view; bad state and unknown rev exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code blame` | aligned, one author per range, `@rev`; missing file, unknown repo exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code jobs` | none, stage and groups; unknown change exit 1 | `roll-cli/tests/code.rs::webhooks_statuses_blame_jobs_and_tokens`, `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| `roll code search` | literal, `--regex`, `-n`, `--case`, `--path`, `@rev`; bad regex, unknown rev or repo exit 1 | `roll-cli/tests/code.rs::ai_reviewer_commands`, `roll-cli/tests/code.rs::reviewer_webhook_and_thread_edges` |
| exit codes | usage errors exit 1 (not clap's 2, which means a conflict), `--help` 0, API down exit 1 with "is rolld running?" | `roll-cli/tests/code.rs::repo_land_and_usage_edges` |

## Merge queue (`code-control/tests/queue.rs`, fake runner)

### Transitions

Every entry state (`waiting`, `testing`, `bisecting`, `landing`, `landed`, `ejected`), queue state and
change state transition `src/queue.rs` makes.

| Transition | Trigger | Tests |
| --- | --- | --- |
| `(none) → waiting` | enqueue | `queue.rs::batches_land_together_in_fifo_order` |
| `waiting → waiting` | re-enqueue (no-op), lane change | `queue.rs::urgent_lane_goes_first`, `queue.rs::lanes_can_change_without_losing_place` |
| `waiting → testing` | batch built | `queue.rs::the_batch_is_rebuilt_when_main_moves_under_it`, `queue.rs::the_batch_under_test_is_shown_in_landing_order` |
| `testing → landing → landed` | green batch | `queue.rs::batches_land_together_in_fifo_order`, `queue.rs::pausing_lets_the_batch_under_test_finish` |
| `testing → landed` | head already on main | `queue.rs::a_change_already_on_main_counts_as_landed` |
| `testing → ejected` | conflict | `queue.rs::conflicts_are_ejected_with_their_paths` |
| `testing → bisecting` | red batch | `queue.rs::the_batch_shows_the_prefix_under_test`, `queue.rs::a_red_batch_is_bisected_to_its_culprit` |
| `testing → bisecting → ejected` | timeout | `queue.rs::a_batch_that_runs_out_of_time_is_ejected` |
| `testing → waiting` | dequeue; revision (stays queued) | `queue.rs::dequeueing_a_change_under_test_rebuilds_the_batch`, `queue.rs::revising_or_closing_a_queued_change` |
| `testing → removed` | close, draft, PR closed on GitHub | `queue.rs::revising_or_closing_a_queued_change`, `queue.rs::blockers_keep_changes_out_of_the_queue`, `github.rs::pull_request_issue_and_installation_actions` |
| `testing → waiting` | left the queue or head moved by landing time | `queue.rs::a_batch_never_lands_a_change_that_left_the_queue` |
| `testing → waiting` | a blocker appeared during the test | `queue.rs::a_check_going_red_during_the_test_keeps_the_change_off_main` |
| `testing → landing → waiting` | main moved (CAS failed) | `queue.rs::the_batch_is_rebuilt_when_main_moves_under_it` |
| `testing → waiting` | restart; a failed step | `queue.rs::a_restart_retests_what_was_in_flight`, `queue.rs::a_failed_step_puts_the_batch_back_and_retries` |
| `bisecting → landing → landed` | the prefix before the culprit | `queue.rs::a_red_batch_is_bisected_to_its_culprit` |
| `bisecting → ejected` | failing check (culprit), two culprits | `queue.rs::a_red_batch_is_bisected_to_its_culprit`, `queue.rs::two_culprits_are_both_ejected` |
| `bisecting → waiting` | after the culprit; dequeue mid-bisect; main moved; restart | `queue.rs::a_red_batch_is_bisected_to_its_culprit`, `queue.rs::dequeueing_during_a_bisect_rebuilds_the_batch`, `queue.rs::main_moving_while_a_batch_is_bisected_retests_everything`, `queue.rs::a_restart_during_a_bisect_retests_the_batch` |
| `landing → landed` / `landing → waiting` | restart, tip on main or not | `queue.rs::a_restart_finishes_a_batch_that_had_landed` |
| `waiting → ejected` | gate: untrusted revision, check red, draft, closed, change gone | `queue.rs::changes_that_stop_being_mergeable_while_waiting_are_ejected`, `queue.rs::blockers_that_appear_while_waiting_eject_at_the_gate` |
| `waiting → removed` | draft; PR closed, merged or back to draft on GitHub | `queue.rs::blockers_keep_changes_out_of_the_queue`, `github.rs::pull_request_issue_and_installation_actions` |
| `ejected → waiting` | re-enqueue | `queue.rs::conflicts_are_ejected_with_their_paths` |
| queue state `idle` | | `queue.rs::queue_events_and_shapes`, `queue.rs::batches_land_together_in_fifo_order` |
| queue state `paused` | pause, also over a finishing batch | `queue.rs::pause_and_resume`, `queue.rs::pausing_lets_the_batch_under_test_finish` |
| queue state `testing` | | `queue.rs::the_batch_is_rebuilt_when_main_moves_under_it` |
| queue state `bisecting` | | `queue.rs::the_batch_shows_the_prefix_under_test` |
| queue state `landing` | event while the CAS runs | `queue.rs::pausing_lets_the_batch_under_test_finish` |
| batch size: halve on red, double on green, cap, floor 1, a lowered max | | `queue.rs::the_batch_size_halves_on_red_and_doubles_on_green` |
| change `open → queued` | enqueue | `queue.rs::batches_land_together_in_fifo_order` |
| change `queued → landed` | batch landed, already on main, merged on GitHub | `queue.rs::batches_land_together_in_fifo_order`, `queue.rs::a_change_already_on_main_counts_as_landed`, `github.rs::pull_request_issue_and_installation_actions` |
| change `queued → conflict` | conflict ejection | `queue.rs::conflicts_are_ejected_with_their_paths` |
| change `queued → open` | ejected (check, gate, timeout), dequeue, draft | `queue.rs::a_red_batch_is_bisected_to_its_culprit`, `queue.rs::dequeueing_a_change_under_test_rebuilds_the_batch`, `queue.rs::blockers_that_appear_while_waiting_eject_at_the_gate`, `queue.rs::blockers_keep_changes_out_of_the_queue` |
| change `queued → queued` | revision | `queue.rs::revising_or_closing_a_queued_change` |
| change `queued → closed` | close in roll or on GitHub; a closed change stays closed at the gate | `queue.rs::revising_or_closing_a_queued_change`, `github.rs::pull_request_issue_and_installation_actions`, `queue.rs::blockers_that_appear_while_waiting_eject_at_the_gate` |
| change `conflict → queued` / `conflict → open` | re-enqueue / revise | `queue.rs::conflicts_are_ejected_with_their_paths` |
| change `landed` or `closed` → queued refused | 409 | `queue.rs::blockers_keep_changes_out_of_the_queue` |

### Behaviours

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

## Reviews (`code-control/tests/review.rs`, `src/review/*` unit tests)

### Rules

| Rule | Covered | Tests |
| --- | --- | --- |
| `required_approvals` | on/off, bounds 0–20 (20 ok; 21, -1, "1", 1.5 → 400; null keeps), file tightens (max wins), invalid file (21) falls back | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::rules_matrix`, `review.rs::codeowners_auto_approve_and_the_review_file`, `review::rules` unit `file_tightens_settings` |
| `require_codeowners` | on/off, file tightens (turns on, can't turn off) | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::codeowners_locations_last_match_and_every_owner_kind`, `review.rs::codeowners_auto_approve_and_the_review_file` |
| `count_ai_approval` | on/off, file tightens (holds only when both allow) | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::ai_approvals_and_land_change_gate`, `github.rs::review_events_every_action_against_the_rules` |
| `block_on_request_changes` | on/off, file tightens, lifted by the same reviewer approving | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review::rules` unit `request_changes_blocks_until_superseded` |
| `dismiss_stale` | on/off, file tightens, code-owner approvals too | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::rules_matrix`, `review.rs::codeowners_locations_last_match_and_every_owner_kind`, `github.rs::review_events_every_action_against_the_rules` |
| `require_resolved_threads` | on/off, file tightens, outdated threads count, GitHub resolutions decide | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::threads_suggestions_and_carry_forward`, `github.rs::review_events_every_action_against_the_rules` |
| `allow_self_approval` | on/off, file tightens (holds only when both allow), code owners too | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::rules_matrix`, `review.rs::codeowners_locations_last_match_and_every_owner_kind` |
| `auto_approve` (`paths`, `comment_only`) | settings only, file only, both, paths+comment_only in one rule, validation (51 rules, 101 paths, empty pattern, non-list, neither key) in settings and file; review_pending and changes_requested still block | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::codeowners_auto_approve_and_the_review_file`, `review::rules` unit `auto_approve`, `review::comments` units |
| CODEOWNERS | the four locations in order, a symlinked file skipped, per-line errors, last match wins, un-owning lines, `@name`, `@org/team`, `agent:`, `app:`, `roll`, two groups, stale code-owner approvals | `review.rs::codeowners_locations_last_match_and_every_owner_kind`, `review.rs::codeowners_auto_approve_and_the_review_file` |
| review apps: scope | outside apps comment but get 403 on approve/request_changes; event filters; install scope; disabled or deleted required apps owe nothing | `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |

### Blockers

| Blocker | Covered | Tests |
| --- | --- | --- |
| blocker `needs_approval` | appears/clears (count message per approval) | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::rules_matrix` |
| blocker `needs_codeowner` | appears/clears (per group, owners named) | `review.rs::codeowners_locations_last_match_and_every_owner_kind`, `review.rs::every_rule_on_and_off_from_settings_and_the_file` |
| blocker `changes_requested` | appears/clears (approve, dismiss, GitHub dismiss) | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `review.rs::rules_matrix`, `github.rs::review_events_every_action_against_the_rules` |
| blocker `unresolved_threads` | appears/clears (roll and GitHub resolve, GitHub unresolve brings it back) | `review.rs::every_rule_on_and_off_from_settings_and_the_file`, `github.rs::review_events_every_action_against_the_rules` |
| blocker `review_pending` | appears/clears (review of head, new head, by hand, disabled or deleted app) | `review.rs::review_requests_and_pending_reviews`, `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |
| blocker `review_required` | appears (`/land`, git push, required app)/clears (approved change lands, app removed) | `review.rs::direct_landings_and_pushes_need_review`, `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |

### GitHub review events

| Event | Covered | Tests |
| --- | --- | --- |
| `pull_request_review.submitted` | OWNER (any case), MEMBER and COLLABORATOR decide; NONE, CONTRIBUTOR or missing → comment; a bot's verdict kept, counts only via `count_ai_approval`; the PR author's own doesn't count; pending ignored; roll's own echo ignored; redelivery once; non-head or unknown commit → stale; unknown PR ignored; settles the review request; links earlier inline comments | `github.rs::reviews_both_ways`, `github.rs::review_events_every_action_against_the_rules`, `code-github/tests/events.rs::review_events` |
| `pull_request_review.edited` | known: body updated in place; unknown: recorded | `github.rs::reviews_both_ways`, `github.rs::review_events_every_action_against_the_rules`, `code-github/tests/events.rs::review_events` |
| `pull_request_review.dismissed` | stops counting or blocking, `dismissed_by @sender`, redelivery no-op, unknown ignored | `github.rs::reviews_both_ways`, `github.rs::review_events_every_action_against_the_rules`, `code-github/tests/events.rs::review_events` |
| `pull_request_review_comment.created` | thread (RIGHT→new, LEFT→old, no line→outdated), suggestion, pending review link, replies once, `@roll` commands | `github.rs::reviews_both_ways`, `github.rs::review_events_every_action_against_the_rules`, `github.rs::roll_commands_from_github_comments`, `code-github/tests/events.rs::review_events` |
| `pull_request_review_comment.edited` | in place; unknown no-op | `github.rs::reviews_both_ways`, `github.rs::review_events_every_action_against_the_rules`, `code-github/tests/events.rs::review_events` |
| `pull_request_review_comment.deleted` | removed; unknown no-op | `github.rs::reviews_both_ways`, `github.rs::review_events_every_action_against_the_rules`, `code-github/tests/events.rs::review_events` |
| `pull_request_review_thread.resolved` | resolved by @sender, node id kept, not echoed; roll's own and unknown ignored | `github.rs::reviews_both_ways`, `github.rs::review_events_every_action_against_the_rules`, `code-github/tests/events.rs::review_events` |
| `pull_request_review_thread.unresolved` | unresolved in roll, blocker returns, not echoed; unknown ignored | `github.rs::review_events_every_action_against_the_rules`, `code-github/tests/events.rs::review_events` |

### Behaviours

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
| review apps decide (approve, request changes) only on repos they're installed on; a disabled or deleted required app owes nothing | `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |

## roll's reviewer (`code-control/tests/reviewer.rs`, `code-review/tests/engine.rs`, `code-index/tests/index.rs`, unit tests)

No test calls a real model API: Anthropic and OpenAI-compatible endpoints are mocks that record
every request.

### Modes, speeds and caps

| Feature | Covered | Tests |
| --- | --- | --- |
| mode `standard` | no repository context, effort medium, no verification | `reviewer.rs::reviews_a_change_automatically_with_the_right_request`, `engine.rs::standard_has_no_context_and_thorough_raises_effort`, `engine.rs::effort_follows_the_table_for_every_mode_and_speed` |
| mode `deep` | repository context, confidence shown, effort high, goes on without context while the index is first built | `reviewer.rs::auto_mode_deep_context_and_classifier`, `reviewer.rs::a_deep_review_reports_indexing_while_the_index_is_built`, `reviewer.rs::a_deep_review_goes_on_without_context_while_the_index_is_first_built` |
| mode `max` | verification drops unconfirmed findings | `reviewer.rs::fast_streams_threads_and_max_verifies`, `reviewer.rs::modes_speeds_effort_classifier_fallback_and_profiles`, `engine.rs::max_mode_verifies_and_uses_context` |
| mode `auto` | `deep_paths` → deep, `max_paths` → max, small change → standard | `reviewer.rs::auto_mode_deep_context_and_classifier`, `reviewer.rs::modes_speeds_effort_classifier_fallback_and_profiles`, `code-review` `mode::tests::routing` |
| chosen_by `auto-heuristic` | including the fallback when the classifier call fails | `reviewer.rs::auto_mode_deep_context_and_classifier`, `reviewer.rs::modes_speeds_effort_classifier_fallback_and_profiles` |
| chosen_by `auto-model` | classifier on the secondary model at low effort, metered | `reviewer.rs::auto_mode_deep_context_and_classifier`, `engine.rs::classifier_and_replies` |
| chosen_by `settings` | a fixed mode | `reviewer.rs::reviews_a_change_automatically_with_the_right_request`, `reviewer.rs::roll_commands_from_writers_act_and_from_readers_are_ignored` |
| chosen_by `command` | `@roll review deep`, `@roll full review max`, `mode` in POST ai-review | `reviewer.rs::roll_commands_in_threads`, `reviewer.rs::roll_commands_from_writers_act_and_from_readers_are_ignored` |
| speed `fast` | secondary model, low effort, groups in parallel, threads streamed, no verification | `reviewer.rs::fast_streams_threads_and_max_verifies`, `engine.rs::fast_runs_groups_in_parallel_and_streams`, `engine.rs::effort_follows_the_table_for_every_mode_and_speed` |
| speed `balanced` | standard → medium, deep and max → high | `engine.rs::effort_follows_the_table_for_every_mode_and_speed`, `reviewer.rs::auto_mode_deep_context_and_classifier` |
| speed `thorough` | xhigh, verifies even standard reviews | `engine.rs::effort_follows_the_table_for_every_mode_and_speed`, `reviewer.rs::modes_speeds_effort_classifier_fallback_and_profiles` |
| effort table (9 mode × speed cells) and the `effort` override | override checked in every cell | `engine.rs::effort_follows_the_table_for_every_mode_and_speed`, `reviewer.rs::modes_speeds_effort_classifier_fallback_and_profiles` |
| profile `chill` / `assertive` | chill drops nits (approve); assertive posts them (comment) | `reviewer.rs::modes_speeds_effort_classifier_fallback_and_profiles`, `engine.rs::dedupes_against_open_threads_and_profiles` |
| incremental vs `full` | | `reviewer.rs::incremental_re_reviews_dedupe_and_skips`, `reviewer.rs::change_reviewer_routes_answer_every_status_and_jobs_queue`, `reviewer.rs::roll_commands_from_writers_act_and_from_readers_are_ignored` |
| on_cap `pause` | no model call, comment, job skipped, change stops waiting, nothing metered | `reviewer.rs::on_cap_pause_byok_fallback_and_standard_only`, `reviewer.rs::pricing_caps_allowances_and_byok` |
| on_cap `byok_fallback` | without a key: paused, chats refused; with a key: the owner's key at BYOK prices | `reviewer.rs::on_cap_pause_byok_fallback_and_standard_only` |
| on_cap `standard_only` | deep/max → standard; standard goes on past the cap and is charged; chats refused | `reviewer.rs::on_cap_pause_byok_fallback_and_standard_only`, `reviewer.rs::pricing_caps_allowances_and_byok` |

### Commands

| Command | Covered | Tests |
| --- | --- | --- |
| command `@roll review` | "On it"; skipped when nothing changed; resumes a paused change | `reviewer.rs::roll_commands_from_writers_act_and_from_readers_are_ignored` |
| command `@roll review [mode]` | forced mode | `reviewer.rs::roll_commands_in_threads` |
| command `@roll full review [mode]` | everything reviewed | `reviewer.rs::roll_commands_from_writers_act_and_from_readers_are_ignored` |
| command `@roll pause` / `@roll resume` | | `reviewer.rs::roll_commands_in_threads`, `reviewer.rs::roll_commands_from_writers_act_and_from_readers_are_ignored` |
| command `@roll resolve` | | `reviewer.rs::roll_commands_in_threads` |
| command `@roll explain` | metered as a chat | `reviewer.rs::roll_commands_in_threads` |
| command `@roll fix` | a revision with the suggestion | `reviewer.rs::roll_commands_in_threads` |
| command `@roll don't flag` / `ignore` / `remember` | learnings with `source: command` and the thread | `reviewer.rs::roll_commands_in_threads`, `reviewer.rs::roll_commands_from_writers_act_and_from_readers_are_ignored` |
| command: a question (`@roll <anything>`) | | `reviewer.rs::roll_commands_in_threads`, `reviewer.rs::config_and_key_errors_give_up_without_retrying` |
| command from a non-writer | all 13 forms ignored: no reply, job, model call, learning, pause or resolve | `reviewer.rs::roll_commands_from_writers_act_and_from_readers_are_ignored`, `reviewer.rs::roll_commands_in_threads` |

### Errors

| Error | Covered | Tests |
| --- | --- | --- |
| error `bad key (401)` | not retried, reported, change no longer waits; a chat says why in its thread | `reviewer.rs::retries_then_gives_up_and_no_provider`, `reviewer.rs::a_chat_with_a_bad_key_fails_without_retrying`, `engine.rs::every_group_failing_is_an_error_and_stop_reasons_are_read` |
| error `bad key (403)` | | `reviewer.rs::provider_errors_403_persistent_529_and_400`, `engine.rs::timeouts_forbidden_server_errors_and_garbage` |
| error `429` | `retry-after` honoured | `engine.rs::retries_429_with_retry_after_but_not_400` |
| error `529` | retried by the provider, then by the job (3 attempts), then gives up | `engine.rs::retries_429_with_retry_after_but_not_400`, `reviewer.rs::provider_errors_403_persistent_529_and_400`, `reviewer.rs::a_failing_review_runs_three_times` |
| error `5xx` | | `reviewer.rs::retries_then_gives_up_and_no_provider`, `engine.rs::timeouts_forbidden_server_errors_and_garbage`, `engine.rs::retries_429_with_retry_after_but_not_400` |
| error `400` | not retried; one failing group is listed and the rest reviewed | `engine.rs::retries_429_with_retry_after_but_not_400`, `reviewer.rs::provider_errors_403_persistent_529_and_400`, `engine.rs::a_failing_group_is_listed_and_the_rest_is_reviewed` |
| error `timeout` | retried, then exhausted | `engine.rs::timeouts_forbidden_server_errors_and_garbage` |
| error `refusal` | files listed, no approval | `reviewer.rs::refusals_and_cut_off_reviews_list_the_files_and_dont_approve`, `engine.rs::max_tokens_splits_and_refusals_skip`, `engine.rs::a_failing_group_is_listed_and_the_rest_is_reviewed` |
| error `max_tokens` | split and retried; what still doesn't fit is listed | `reviewer.rs::refusals_and_cut_off_reviews_list_the_files_and_dont_approve`, `engine.rs::max_tokens_splits_and_refusals_skip` |
| error: undecodable, oversized or redirected answer | | `engine.rs::timeouts_forbidden_server_errors_and_garbage`, `engine.rs::oversized_answers_and_redirects_are_refused` |
| error `config error` | | `reviewer.rs::config_and_key_errors_give_up_without_retrying`, `provider::openai` unit tests |
| error `no provider` | review and chat | `reviewer.rs::retries_then_gives_up_and_no_provider`, `reviewer.rs::config_and_key_errors_give_up_without_retrying` |
| error `invalid settings` | 400 names the field; `.roll/review.yaml` errors reported | `reviewer.rs::repo_reviewer_routes_answer_every_status`, `reviewer.rs::triggers_settings_and_auth`, `code-review` `settings::tests::overlay_and_errors` |
| error `ROLL_SECRET_KEY` missing or weak | when setting a key | `reviewer.rs::byok_needs_a_server_secret` |
| error `ROLL_SECRET_KEY` missing or changed at review time | no fallback to the hosted key | `reviewer.rs::config_and_key_errors_give_up_without_retrying` |

### Jobs

| Job | Covered | Tests |
| --- | --- | --- |
| job state `queued` | a plain request folds into the queued job | `reviewer.rs::jobs_go_queued_running_reviewing_verifying_done`, `reviewer.rs::change_reviewer_routes_answer_every_status_and_jobs_queue` |
| job state `running` | one per change | `reviewer.rs::change_reviewer_routes_answer_every_status_and_jobs_queue`, `reviewer.rs::jobs_go_queued_running_reviewing_verifying_done` |
| job state `done` | | `reviewer.rs::jobs_go_queued_running_reviewing_verifying_done`, `reviewer.rs::byok_openai_compatible_end_to_end` |
| job state `failed` | | `reviewer.rs::provider_errors_403_persistent_529_and_400`, `reviewer.rs::config_and_key_errors_give_up_without_retrying`, `reviewer.rs::a_chat_with_a_bad_key_fails_without_retrying` |
| job state `skipped` | | `reviewer.rs::incremental_re_reviews_dedupe_and_skips`, `reviewer.rs::change_reviewer_routes_answer_every_status_and_jobs_queue`, `reviewer.rs::on_cap_pause_byok_fallback_and_standard_only` |
| job stages `queued` → `indexing` → `chunking` → `reviewing` → `verifying` → `posting` → `done` / `failed` | in order; `indexing` with a 1,500-file first build; `posting` lasts microseconds, checked as an allowed value | `reviewer.rs::jobs_go_queued_running_reviewing_verifying_done`, `reviewer.rs::a_deep_review_reports_indexing_while_the_index_is_built`, `reviewer.rs::jobs_report_their_stage_while_running`, `engine.rs::max_mode_verifies_and_uses_context` |
| restart recovery | | `reviewer.rs::restart_recovers_running_jobs` |
| spend caps under concurrency | reviews, chats and free public reviews reserve before the model call; the same races with the jobs on two nodes; 32 reservations at once on one node and across two; a dead node's reservation lapses; metering releases in the same transaction | `reviewer.rs::concurrent_reviews_and_chats_never_overrun_the_cap`, `reviewer.rs::concurrent_public_reviews_never_share_the_last_free_one`, `reviewer.rs::concurrent_reviews_on_two_nodes_never_overrun_the_cap`, `reviewer.rs::concurrent_public_reviews_on_two_nodes_never_share_the_last_free_one`, `code-control/src/ai/mod.rs::reservations_hold_the_cap_and_the_free_reviews_under_contention`, `code-control/src/ai/mod.rs::reservations_hold_the_cap_across_nodes`, `code-control/src/ai/mod.rs::reservations_of_a_dead_node_lapse` |

### Behaviours

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

## GitHub

### Webhook events

Parsing (`code-github/tests`) and the effect in roll against a mock GitHub
(`code-control/tests/github.rs`). Review events are under Reviews.

| Event | Kind | Tests |
| --- | --- | --- |
| `ping` | parse + effect | `code-github/tests/github.rs::webhook_events`, `code-github/tests/events.rs::states_and_accessors`, `github.rs::connecting_needs_the_admin_and_fork_prs_are_untrusted` |
| `push` default branch, fast-forward | parse + effect | `code-github/tests/github.rs::webhook_events`, `github.rs::push_to_github_main_is_ingested_and_merged` |
| `push` default branch, both moved → merge | effect | `github.rs::push_to_github_main_is_ingested_and_merged` |
| `push` default branch, overlapping files → mirror paused | effect | `github.rs::push_to_github_main_is_ingested_and_merged` |
| `push` default branch, rewritten history → mirror paused | parse + effect | `code-github/tests/events.rs::states_and_accessors` (forced), `github.rs::push_webhooks_for_branches_tags_and_echoes` |
| `push` roll's own bot echo → ignored | parse + effect | `code-github/tests/github.rs::webhook_events`, `github.rs::push_webhooks_for_branches_tags_and_echoes` |
| `push` branch created / updated / same head again | parse + effect | `code-github/tests/github.rs::webhook_events`, `github.rs::push_webhooks_for_branches_tags_and_echoes` |
| `push` branch deleted → change closed (unless a PR owns it) | parse + effect | `code-github/tests/events.rs::deleted_refs_and_malformed_payloads`, `github.rs::push_webhooks_for_branches_tags_and_echoes` |
| `push` tag created / deleted (annotated and lightweight) | parse + effect | `code-github/tests/events.rs::deleted_refs_and_malformed_payloads`, `github.rs::push_webhooks_for_branches_tags_and_echoes` |
| `push` to a repo that isn't connected → ignored | effect | `github.rs::push_webhooks_for_branches_tags_and_echoes` |
| `pull_request.opened` | parse + effect | `code-github/tests/events.rs::every_pull_request_action`, `github.rs::connect_import_webhooks_and_mirror` |
| `pull_request.opened` from a fork → untrusted | parse + effect | `code-github/tests/github.rs::webhook_events`, `github.rs::connecting_needs_the_admin_and_fork_prs_are_untrusted` |
| `pull_request.opened` as a draft → draft change | parse + effect | `code-github/tests/events.rs::states_and_accessors`, `github.rs::pull_request_issue_and_installation_actions` |
| `pull_request.opened` into another branch → ignored | effect | `github.rs::pull_request_issue_and_installation_actions` |
| `pull_request.synchronize` | parse + effect | `code-github/tests/events.rs::every_pull_request_action`, `github.rs::issue_pr_and_installation_lifecycles` |
| `pull_request.edited` | parse + effect | `code-github/tests/events.rs::every_pull_request_action`, `github.rs::pull_request_issue_and_installation_actions` |
| `pull_request.closed` (leaves the queue) | parse + effect | `code-github/tests/events.rs::states_and_accessors`, `github.rs::issue_pr_and_installation_lifecycles`, `github.rs::pull_request_issue_and_installation_actions` |
| `pull_request.closed` merged (leaves the queue, landed) | parse + effect | `code-github/tests/github.rs::webhook_events`, `github.rs::pull_request_issue_and_installation_actions` |
| `pull_request.reopened` | parse + effect | `code-github/tests/events.rs::every_pull_request_action`, `github.rs::pull_request_issue_and_installation_actions` |
| `pull_request.ready_for_review` | parse + effect | `code-github/tests/events.rs::every_pull_request_action`, `github.rs::pull_request_issue_and_installation_actions` |
| `pull_request.converted_to_draft` (dequeues) | parse + effect | `code-github/tests/events.rs::every_pull_request_action`, `github.rs::pull_request_issue_and_installation_actions` |
| `pull_request` other actions (labeled, …) → ignored | parse | `code-github/tests/events.rs::every_pull_request_action`, `code-github/tests/github.rs::webhook_events` |
| `issues.opened` | parse + effect | `code-github/tests/events.rs::every_issue_action`, `github.rs::connect_import_webhooks_and_mirror` |
| `issues.edited` | parse + effect | `code-github/tests/events.rs::every_issue_action`, `github.rs::issue_pr_and_installation_lifecycles` |
| `issues.labeled` (P0–P3 priority) | parse + effect | `code-github/tests/github.rs::webhook_events`, `github.rs::issue_pr_and_installation_lifecycles` |
| `issues.unlabeled` | parse + effect | `code-github/tests/events.rs::every_issue_action`, `github.rs::pull_request_issue_and_installation_actions` |
| `issues.closed` completed / no reason / not_planned | parse + effect | `code-github/tests/events.rs::states_and_accessors`, `code-github/tests/github.rs::webhook_events`, `github.rs::connect_import_webhooks_and_mirror`, `github.rs::issue_pr_and_installation_lifecycles`, `github.rs::pull_request_issue_and_installation_actions` |
| `issues.reopened` | parse + effect | `code-github/tests/events.rs::every_issue_action`, `github.rs::issue_pr_and_installation_lifecycles` |
| `issues` about a PR → ignored | effect | `github.rs::pull_request_issue_and_installation_actions` |
| `issues.deleted`, pinned, transferred, … | parse (ignored) + effect | `code-github/tests/events.rs::every_issue_action`, `github.rs::pull_request_issue_and_installation_actions` |
| `issue_comment.created` with `@roll` | parse + effect | `code-github/tests/events.rs::every_issue_action`, `github.rs::roll_commands_from_github_comments` |
| `issue_comment.created` plain, on an issue, by a reader or a bot | effect | `github.rs::roll_commands_from_github_comments`, `github.rs::issue_pr_and_installation_lifecycles` |
| `issue_comment.edited` | parse + effect | `code-github/tests/events.rs::every_issue_action`, `github.rs::roll_commands_from_github_comments` |
| `issue_comment.deleted` | parse | `code-github/tests/events.rs::every_issue_action` |
| `installation.created` | parse + effect | `code-github/tests/github.rs::webhook_events`, `github.rs::issue_pr_and_installation_lifecycles` |
| `installation.deleted` | parse + effect | `code-github/tests/events.rs::installation_actions`, `github.rs::issue_pr_and_installation_lifecycles`, `github.rs::pull_request_issue_and_installation_actions` |
| `installation.suspend` / `unsuspend` | parse + effect | `code-github/tests/events.rs::installation_actions`, `github.rs::pull_request_issue_and_installation_actions` |
| `installation.new_permissions_accepted` | parse + effect | `code-github/tests/events.rs::installation_actions`, `github.rs::pull_request_issue_and_installation_actions` |
| `installation_repositories.added` / `removed` | parse + effect | `code-github/tests/github.rs::webhook_events`, `code-github/tests/events.rs::installation_actions`, `github.rs::pull_request_issue_and_installation_actions` |
| unknown events (star, deployment, check_suite) | parse + effect | `code-github/tests/events.rs::deleted_refs_and_malformed_payloads`, `code-github/tests/github.rs::webhook_events`, `github.rs::issue_pr_and_installation_lifecycles` |
| delivery: signature forms, unsigned 401, malformed 400, dedupe, a failed delivery processed on redelivery, 25 MB bodies | effect | `code-github/tests/events.rs::signature_header_forms`, `github.rs::connect_import_webhooks_and_mirror`, `github.rs::pull_request_issue_and_installation_actions`, `github.rs::connecting_needs_the_admin_and_fork_prs_are_untrusted`, `routes.rs::github_webhook_statuses` |

### Outbound and the mirror

| Action | Tests |
| --- | --- |
| outbound comment + close the PR on land, once | `github.rs::connect_import_webhooks_and_mirror`, `github.rs::outbound_notifications_once_each` |
| outbound: a PR already closed on GitHub gets no write | `github.rs::outbound_notifications_once_each` |
| outbound: GitHub's own close or merge isn't echoed | `github.rs::issue_pr_and_installation_lifecycles`, `github.rs::pull_request_issue_and_installation_actions` |
| outbound close the issue `completed` / `not_planned`; an issue closed on GitHub isn't written to | `github.rs::connect_import_webhooks_and_mirror`, `github.rs::outbound_notifications_once_each` |
| outbound check runs: create, update, all 6 statuses; GitHub-sourced and non-PR checks aren't sent | `github.rs::connect_import_webhooks_and_mirror`, `github.rs::outbound_notifications_once_each` |
| outbound `@roll` answers (PR comment, thread reply) | `github.rs::roll_commands_from_github_comments` |
| outbound review posted as one GitHub review, threads resolved both ways | `github.rs::reviews_both_ways`, `github.rs::roll_reviewer_posts_to_the_pull_request` |
| mirror `ok` | `github.rs::connect_import_webhooks_and_mirror` |
| mirror `pending` (roll ahead) | `github.rs::push_webhooks_for_branches_tags_and_echoes` |
| mirror `error` → `ok` | `github.rs::issue_pr_and_installation_lifecycles` |
| mirror `paused` (overlap, rewritten history, disconnected) and back | `github.rs::push_to_github_main_is_ingested_and_merged`, `github.rs::push_webhooks_for_branches_tags_and_echoes`, `github.rs::pull_request_issue_and_installation_actions`, `github.rs::issue_pr_and_installation_lifecycles` |
| mirror coalescing (200 landings ≤ 10 pushes), stops on delete; scheduler: max delay, backoff, lost responses, GitHub ahead, diverged | `github.rs::connect_import_webhooks_and_mirror`, `github.rs::issue_pr_and_installation_lifecycles`, `code-github/tests/mirror.rs` (all) |

### Behaviours

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
| two nodes forwarding one land log at once (their background forwarders and four more racing rounds) while landings arrive: each landing queued and delivered exactly once per webhook | `webhooks.rs::two_forwarders_deliver_each_landing_once` |
| a forwarder failing after queueing a page (a crash mid-page) keeps nothing of it and leaves the cursor; earlier pages stay forwarded, the next round forwards the rest, none twice | `webhooks.rs::a_forwarder_failing_mid_page_neither_drops_nor_duplicates` |
| a webhook created, changed or deleted through one node applies at once to events emitted on another (no per-node cache) | `webhooks.rs::webhooks_made_on_one_node_apply_to_events_on_another` |
| webhooks created through two nodes while the forwarder starts for the first time (no cursor yet) all succeed, and each gets the next landing once | `webhooks.rs::webhooks_created_while_the_forwarder_starts` |
| two nodes' delivery workers on one queue, every first attempt failing: each delivery sent exactly twice (the failure and the retry), both attempts in the log | `webhooks.rs::two_delivery_workers_send_each_delivery_once_including_retries` |
| a delivery claimed by a node that died is sent by another node once the claim lapses, not before, and once | `webhooks.rs::a_claim_held_by_a_dead_node_is_retried_after_it_lapses` |
| a receiver slower than the claim: the request times out inside the claim and is recorded; the retry is the only other send | `webhooks.rs::a_slow_receiver_never_gets_an_unrecorded_second_send` |
| failing statuses on the default branch: one `broken-main` item per context (deduplicated), a certain culprit reverted (and landed with `auto_revert`), an uncertain one named as a range, closed by a later success for that context only, not by an older one; statuses off the default branch break nothing | `webhooks.rs::failing_statuses_on_the_default_branch_break_main`, `webhooks.rs::failing_statuses_auto_revert` |
| statuses: pending/error/success → checks on the changes at that head (`source: external`, URL, `error:` summary); a queue repo's `required_checks` unblocked by external CI; combined state over contexts; a commit with no change recorded only; by git commit (and prefix) after a push; validation (state, target_url, context length), unknown commit 404; `check` events; deleting the repo removes hooks and statuses | `webhooks.rs::commit_statuses_become_checks`, `webhooks.rs::pushes_are_webhook_events` |

## Work trees (`code-control/tests/checkout.rs`, `code-store/tests/checkout.rs`)

| Behaviour | Tests |
| --- | --- |
| `checkout_delta` equals a fresh `materialize` both ways across adds, deletes, emptied directories, file↔directory, exec bit, symlink retargets and symlink↔file; same changeset is a no-op | `code-store/tests/checkout.rs::delta_matches_a_fresh_materialize` |
| a tampered tree (a directory swapped for a symlink, a missing file) fails instead of writing outside; a hostile target tree (duplicate names) never escapes | `checkout.rs::a_tampered_tree_fails_instead_of_writing_outside`, `checkout.rs::a_hostile_target_tree_never_escapes` |
| leases (concurrent jobs get different trees), reuse and delta moves, no-op at the same changeset, fresh checkout after a failed move, throwaway checkout when all trees are busy (removed after), trees removed with the repo | `code-control/tests/checkout.rs::trees_are_leased_moved_and_replaced` |
| the queue and the runner use them (2,000-change queue run: 25 → 74–124 merges/s) | `queue.rs::throughput_with_culprits`, `code-preview/tests/service.rs` |
| previews read the pooled trees (moved by diff between changes, no per-change copies) | `contract.rs::previews_use_pooled_work_trees` |
| a restart sets aside and removes what the last process left (pooled trees, throwaway checkouts, old per-change preview trees, a half-finished cleanup); no lease survives | `checkout.rs::a_restart_clears_what_the_last_process_left` |

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
| an oversize push is refused (413, or the connection closed under the client while it still sends) and the server keeps answering | `code-git/tests/http.rs::request_limits` |
| memory budget: concurrent pushes over the budget stay within it (tracked peak), each lands or gets the busy `unpack` report, and the budget returns to zero | `code-git/tests/memory.rs::concurrent_pushes_stay_within_the_budget` |
| memory budget: a client going away mid-push releases everything it held | `code-git/tests/memory.rs::a_client_going_away_mid_push_releases_its_memory` |
| memory budget: a fetch whose client goes away while it waits for memory stops and releases at once | `code-git/tests/memory.rs::a_fetch_whose_client_goes_away_releases_its_memory` |
| memory budget: small pushes flow while a large one holds most of the budget; a second large one waits, or fails as the younger | `code-git/tests/memory.rs::small_pushes_flow_while_a_large_one_holds_most_of_the_budget` |
| memory budget: a push past one request's share is refused with an `unpack` report once its body is read | `code-git/tests/memory.rs::a_push_past_one_requests_share_fails_cleanly` |
| memory budget: fetches are charged; with no memory, upload-pack answers 503 + `Retry-After`, a push gets a report git prints, a fetch that runs out mid-pack gets a side-band error | `code-git/tests/memory.rs::fetches_are_charged_and_requests_without_memory_fail_cleanly` |
| memory budget rules: per-request share, the bytes kept for small requests, waiting, wait-die when every holder waits, cancellation | `code-git/src/budget.rs::grants_waits_and_releases`, `code-git/src/budget.rs::holders_that_would_all_wait_fail_instead`, `code-git/src/budget.rs::the_youngest_holder_fails_even_when_an_older_one_blocks_last`, `code-git/src/budget.rs::cancel_wakes_waiters` |
| streamed pack parsing (any chunk boundaries, truncation, corruption, trailing data) and metering (what's held is charged, a refusing meter stops the read) | `code-git/tests/pack.rs::streamed_packs_read_like_whole_ones`, `code-git/tests/pack.rs::reading_charges_what_it_holds_and_stops_when_refused` |
| local object storage opened durable fsyncs blobs and reads back the same; S3 unchanged | `pg-storage/src/objects.rs::durable_local_store_fsyncs_and_works_the_same`, `pg-storage/src/objects.rs::s3_is_durable_either_way` |
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
| durability: a landing (batch, `advance`, ref update) is acknowledged and published only after the WAL is fsynced past its commit; concurrent syncs coalesce; checkpoints run on a background thread, off the writer's lock | `lander.rs::landings_are_synced_before_they_are_acknowledged_and_checkpointed_in_the_background`, `code-sql` unit `durable::tests::concurrent_syncs_share_fsyncs_and_all_return_durable` |
| two landers on one ref (two nodes, or two stores on one database): a batch whose ref moved underneath is rebuilt on the new head (merging with what landed, conflicts and `already` checked against it, a non-fast-forward reset honoured); concurrent landings through both land once each, logged once, in order | `lander.rs::a_batch_is_rebuilt_when_another_lander_moved_the_ref`, `lander.rs::two_landers_on_one_ref_lose_and_duplicate_nothing` |
| a commit whose reply was lost (`CommitUnknown`) is resolved before answering: committed is acknowledged and published once, rolled back fails and lands again once (batches, ref updates, `advance`); the decision from `pg_xact_status` or the ref | `lander.rs::a_lost_commit_reply_is_resolved_before_answering`, `lander::tests::the_transaction_status_settles_a_lost_commit`, `lander::tests::without_it_the_ref_decides` |
| every test above on Postgres too (`ROLL_CODE_TEST_DATABASE`); several statements pipelined in one round trip | `code-store/tests` (all), `code-sql/tests/backends.rs::execute_pipeline_runs_statements_in_order` |

## Checks and previews

| Behaviour | Tests |
| --- | --- |
| roll.toml parsing and errors, env injection, host mapping, hop-by-hop headers | `code-preview/src/{config,preview,data,docker}.rs` tests |
| change checks, stale heads dropped, post-merge `broken-main` open, close and claimed, coalescing, broken roll.toml, no roll.toml, auto previews follow their change, resume after restart | `code-preview/tests/service.rs` (fake runner) |
| broken main: bisected to the culprit, a revert opened (item stays claimable), closed when main is green again; `[main] auto_revert` lands it; approved fork changes and queued changes run checks | `service.rs::post_merge_opens_one_broken_main_item_and_closes_it_on_green`, `service.rs::auto_revert_undoes_the_change_that_broke_main`, `service.rs::approved_fork_changes_and_queued_changes_run_their_checks` |
| HTTP and WebSocket proxying, unknown and closed previews, idle reaper, reconcile after restart | `code-preview/src/preview.rs::proxies_http_and_websockets_and_reconciles_after_restart` |
| real Docker: success, failure, timeout, broken config, no leftovers; preview through the host router; post-merge; cleanup on land | `code-preview/tests/docker.rs` (all) |

## control.db on SQLite and Postgres (`code-control/tests/concurrency.rs`, `code-sql/tests/backends.rs`)

Every code-control test runs on SQLite, and on Postgres with `ROLL_CODE_TEST_DATABASE` set
(control.db in a fresh schema; a restart in a test reopens the same one).

| Behaviour | Tests |
| --- | --- |
| two nodes on one control database (the same SQLite file, or the same Postgres schema through their own connections): change and work item numbers created at once from both are each used exactly once | `concurrency.rs::numbers_are_unique_across_nodes` |
| agents claiming at once through both nodes' APIs: each work item goes to exactly one claimant | `concurrency.rs::each_work_item_has_exactly_one_claimant` |
| merge queue entries made at once on both nodes get distinct positions; a change enqueued on both at once has one entry | `concurrency.rs::queue_positions_are_unique_across_nodes` |
| tokens made at once on both nodes all work on both; a revoked one stops working on the other node at once | `concurrency.rs::tokens_work_and_revoke_across_nodes` |
| MCP sessions in control.db: one started through a node works through another and after a restart, ends everywhere when ended on one, lapses when idle; `last_seen` written only when a few seconds old | `mcp.rs::sessions_work_through_any_node` |
| text with a NUL character is a 400 on Postgres (which can't store it), leaving nothing half-written; SQLite keeps it | `api.rs::nul_in_text_is_a_400_on_postgres` |
| one SQL dialect on both backends: types, byte-order text, upserts, serializable re-run writes, rollbacks, migrations, columns named like types | `code-sql/tests/backends.rs::writes_are_serializable_and_retried`, `code-sql/tests/backends.rs::failures_roll_back_and_are_never_reported_as_success`, `code-sql/tests/backends.rs::schema_migrations_and_columns`, `code-sql/tests/backends.rs::columns_named_like_types` |

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
| review apps approving or requesting changes on other owners' public repos | `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |
| a secret shown once (owner token, review-app credentials, webhook secret) shown again after navigating back to a page Next kept in `<Activity>` | `connect-agent.test.tsx`, `owner.test.tsx`, `webhooks.test.tsx` (dashboard) |

## Other regressions

Defects found by the phase-3 coverage audit, each with the test that failed before its fix.

| Defect fixed | Test |
| --- | --- |
| concurrent public reviews both priced free against the last free review, then one was charged past `monthly_cap_usd` | `reviewer.rs::concurrent_public_reviews_never_share_the_last_free_one` |
| a failing review ran four times instead of three | `reviewer.rs::a_failing_review_runs_three_times` |
| a failed `@roll` chat left the thread without an answer | `reviewer.rs::a_chat_with_a_bad_key_fails_without_retrying` |
| acknowledged landings could be lost on an OS crash or power loss (WAL not fsynced) | `lander.rs::landings_are_synced_before_they_are_acknowledged_and_checkpointed_in_the_background` |
| a PR closed (or merged) on GitHub stayed in the merge queue and could land | `github.rs::pull_request_issue_and_installation_actions` |
| GitHub draft PRs weren't drafts; `ready_for_review` and `converted_to_draft` were ignored | `github.rs::pull_request_issue_and_installation_actions`, `code-github/tests/events.rs::every_pull_request_action` |
| a required check going red while its batch was tested didn't keep the change off main | `queue.rs::a_check_going_red_during_the_test_keeps_the_change_off_main` |
| no `queue` event ever carried state `landing` | `queue.rs::pausing_lets_the_batch_under_test_finish` |
| a disabled or deleted required review app blocked changes forever | `review.rs::review_apps_events_scopes_requests_and_failed_deliveries` |
| bad query strings answered plain text instead of a JSON 400 | `routes.rs::statuses_of_every_route`, `reviewer.rs::bad_query_strings_are_json_400s` |
| anonymous owner create, owner patch and token create answered 403, so the dashboard never asked to sign in | `routes.rs::every_repo_route_follows_the_auth_matrix`, `api.rs::repos_rename_and_visibility` |
| MCP `comment` hid "not found" behind "needs a thread"; `create_change` refused work items by number | `mcp.rs::tool_argument_errors`, `mcp.rs::tools_map_onto_their_routes` |
| `roll code` exited 2 (rebase and retry) for usage errors and for landing a landed or closed change | `roll-cli/tests/code.rs::repo_land_and_usage_edges`, `roll-cli/tests/code.rs::change_and_work_edges` |
| the MCP snippet lost its auth header placeholder before a token was created | `connect-agent.test.tsx` (dashboard) |
| a thread resolved on GitHub could be resolved back there when a notification read the thread between "GitHub has it resolved" and the resolution itself (now one write, read in one statement) | `github.rs::reviews_both_ways` |

## Dashboard (`apps/dashboard`, vitest)

`src/**/*.test.ts` run in node; `src/**/*.test.tsx` render components with Testing Library in
jsdom (`vitest.config.mts` projects `node` and `dom`, setup in `src/test/`).

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
| diff view: threads at their line and side (unified and split, context lines, ranges, renames), off-diff, outdated and "elsewhere" threads, n/p focus, collapse, binary and too-large files, line selection and shift-click ranges, composer and drafts at their rows | `src/components/code/diff.test.tsx` (`DiffViewer: threads in a unified diff`, `DiffViewer: split view`, `DiffViewer: commenting on lines`) |
| change page review flow: click a line, Comment now → `createThread` with path, side, line and start_line; Suggest; old side; failures keep text; pending review (Start/Add to review, edit, remove, stored across reloads, stale revision, discard) submitted with a verdict → `submitReview` body; closed changes read-only | `src/components/code/changes.test.tsx` (`CodeChange: commenting on a line`, `CodeChange: a pending review`) |
| change page landing: Add to queue and Urgent lanes, post_merge Land, blockers, ejected note with output, conflict note, conflict answer | `src/components/code/changes.test.tsx` (`CodeChange: landing and the merge queue`) |
| markdown XSS through the components (thread comments, review bodies, drafts, composer preview): javascript:/data:/vbscript:/file: and percent-encoded links, `\` and control-character links, protocol-relative links, raw script/img/iframe/svg/math/object/form/meta/base/style, event handlers and style attributes, details/sub/table cells, suggestions, dismissal reasons; safe links keep `rel` | `src/components/code/markdown.test.tsx` (`Markdown in the dashboard: links`, `… raw HTML stays text`, `Markdown in the composer's preview`) |
| merge queue tab: loading, error, empty, paused (Resume), Pause, readers, post_merge, roll.toml error, urgent and normal lanes (move, remove), testing, bisecting (prefix split, or no prefix yet), landing (pausing after it), recent landed and ejected with output | `src/components/code/queue.test.tsx` (`QueueTab`) |
| roll reviewer header: mode, chosen_by (all four), reasons fold, speed, model, hosted vs your key, price and list price, tokens and cache, files, findings, dropped, skipped files, incremental; body header line stripped | `src/components/code/ai.test.tsx` (`AiReviewHeader`, `ReviewTimeline with roll's review`) |
| blame gutter: toggle and `b` key, read once, one id/author/age per run, merged adjacent ranges, uncovered lines, bands and dividers, links and titles, incomplete note, binary note, errors | `src/components/repo/code-lines.test.tsx` (`CodeLines: the blame gutter`) |
| owner token panel: placeholder before create, new token shown once in the snippet (claude and JSON), list reloads with prefix only, gone after unmount and after an `<Activity>` hide and show, re-fetch never shows a secret, failed create, revoke, open daemon, non-managers | `src/components/code/connect-agent.test.tsx` (`ConnectAgent and AccessTokens: a new token is shown once`) |
| review apps: token and secret shown once after Register and after Rotate, the reloaded list never shows them, "I've saved them" clears them, gone after an `<Activity>` hide and show and after unmount | `src/components/code/owner.test.tsx` (`ReviewApps: credentials are shown once`) |
| repo webhooks: generated secret shown once after adding (create request body checked), the reloaded list never shows it, "I've saved it" clears it, a secret the user typed is never shown back, gone after an `<Activity>` hide and show and after unmount | `src/components/code/webhooks.test.tsx` (`WebhooksPanel: a generated secret is shown once`) |

## Measurements

| What | Test | Result |
| --- | --- | --- |
| landing throughput: 40,000 changes, 64 agents, hot-file overlap (release; `pnpm test:code` runs it) | `code-store/tests/throughput.rs::land_40k_merges` | 2026-10-22, 16-core host shared with other builds (load 6–11): tmpfs 883–911 merges/s, real disk (SATA SSD RAID 0) 448–496 merges/s, with every landing fsynced before it's acknowledged (architecture.md, "Landing") |
| first codebase-index build of a 50,000-file repo, streamed and committed 200 files at a time (release, `CODE_INDEX_SCALE_N=50000`, run under a 2 GB memory limit) | `index_scale.rs::a_big_first_build_is_streamed_and_bounded` | 120 s (417 files/s) on real disk; peak RSS grew 51 MB (207 → 258 MB); 500,000 symbols, 12.4 M trigram postings. `pnpm test:code` runs 2,000 files |
| 2,000 changes through the merge queue with 2% culprits (`CODE_QUEUE_N`) | `queue.rs::throughput_with_culprits` | every other change lands once, in order, on a linear main |

## Not covered by automated tests

- **Timing that's a fixed constant**: the GitHub webhook's 202 after its 8 s budget, the queue
  worker retiring after 300 s idle, MCP's 15 s keep-alive comments, `roll code watch`'s 10 s rate
  line, and the review-app/webhook delivery backoff schedule (×3 up to 6 h; the 8 attempts,
  `failed` and redelivery are tested). Testing them would mean waiting that long.
- **Faults that need injection**: 500s, the `review` blocker ("couldn't check reviews"), the queue's
  "couldn't be rebased" ejection, a store failure mid-rebase, the reviewer's `posting` stage
  (microseconds long; checked as an allowed value).
- **Push-ingest corner cases from GitHub**: a gitlink in GitHub's changes, roll's main moving on all
  3 merge retries, a conflict while landing GitHub's push. Mirror RemoteAhead/Diverged pauses are
  covered by the scheduler tests (`code-github/tests/mirror.rs`), not end to end.
- **Large-scale limits**: MCP's 10,000-session cap; JSON bodies over 64 MiB answer 400, not 413.
- **Old-side threads on a change with no merge base** (422): the API can't build such a change.
- **The dashboard's README rendering, sign-in dialog, board and app shell**: README HTML is
  sanitized on the server (`code-control/src/render.rs` tests); the rest are type-checked and were
  driven in headless Chrome in the integration pass. Component tests cover the diff, change page,
  markdown, queue tab, reviewer header, blame gutter and secrets shown once.
- **`roll code delete`'s prompt on a real terminal** (needs a pty; the unattended refusal is
  tested), colour output, and `create --github` success (needs a GitHub App; the GitHub tests cover
  connecting through the API).
- **S3-backed storage for code.** Tests use the local object store, the same `pg_storage::Objects`
  API the other suites test against S3. The local store doesn't fsync blobs.
