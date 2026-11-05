# roll code chat ops

roll's agents, driven and heard from where people talk: Slack, Discord and GitHub. Each one plugs
into automations (docs/code/automations.md) as a **trigger source** and **steps**, so "what happens
when someone types `/roll`" is an automation the owner can read and change, not hidden behaviour.

Code: `crates/code-chat` (the platforms' wire: signatures, OAuth, Web API clients, payload parsing,
Block Kit and Discord components; no roll types) and `crates/code-control/src/chatops/` (installs,
the durable action queue, bindings, triggers, steps, routes). GitHub's side reuses the App in
`crates/code-github` and `crates/code-control/src/github/`.

## The key flow

```
/roll fix the flaky login test           (Slack: a slash command, a mention, a reaction 🤖, a message)
  → chat_actions (verified, recorded once)  → automation inbox: slack/command
  → automation "/roll in Slack":  agent.start (build-box)  →  slack.status
      status message in the thread:   ● Running · 4 steps · running npm test
                                      ✓ npm install  ✓ edit src/login.ts  … npm test
      request card:  "Run the migration"  [Approve] [Decline] [Stop]   ← request.respond
      a reply in the thread           → message.send (steer) to the agent
      done:  ✓ Done · 9 steps   [Open in roll] [acme/app#12]
```

## Installs and who may drive roll

- **Slack.** `POST /v1/code/agents/{owner}/integrations/slack/install` returns Slack's OAuth v2
  authorize URL (bot scopes in `code_chat::slack::BOT_SCOPES`) with a one-time state (ten minutes).
  Slack sends the user to `GET /v1/code/chat/slack/oauth`. The state names the owner, and
  `oauth.v2.access` names the workspace (never the redirect's query). The bot token is sealed with
  `ROLL_SECRET_KEY` to the install, and it is never returned, logged or `Debug`-printed.
  Org-level installs are refused. Besides what the bot does, roll asks for `channels:read`,
  `groups:read`, `users:read` and `usergroups:read` so the dashboard's pickers can list the
  workspace's channels, people and user groups (`GET /v1/code/agents/{owner}/lookup/slack/…`,
  docs/code/http-api.md, "Lookups"). A workspace installed before roll asked for them keeps its
  old grant: its pickers say "Reconnect Slack" until someone installs it again (Add another, the
  same workspace), and ids can still be typed.
- **Discord.** The bot install (`bot applications.commands`). The guild comes from the OAuth token
  exchange's answer; a `guild_id` in the redirect is ignored. One bot token for the server lives in
  its config, and `/roll` is registered in the guild. The pickers list a server's channels and
  roles as the bot; listing members needs the application's Server Members Intent (Developer
  Portal, Bot), and only servers the owner installed it in are ever looked up.
- **One owner per workspace or server** (`UNIQUE (platform, team_id)`). Everything that comes from
  the workspace is that owner's. Another owner installing it gets a conflict.
- **Who may drive roll** is the install's: the installer (Slack's `authed_user`; Discord doesn't
  tell, so its list starts empty) and `members` (platform user ids). With `anyone` set, it is
  everyone in the workspace. Edit them with `PATCH …/integrations/{platform}/{id}`. Everyone
  else's commands, clicks and replies run nothing. They get an ephemeral "only people the roll
  owner allows".
- `app_uninstalled` and `tokens_revoked` forget the install.

## Inbound, durably

The platforms' endpoints (`/v1/code/chat/…`) are global, like GitHub's webhook: any node answers
them.

1. **Verify.** Slack: signing secret v0 (`v0:<timestamp>:<body>`, HMAC-SHA256, constant-time
   compare). Discord: Ed25519 over `timestamp + body`. Both refuse a timestamp more than five
   minutes off, which stops replays.
2. **Record.** The request goes into `chat_actions` with the platform's own id as its key:
   `slack:event:<event_id>`, `slack:command:<trigger_id>`, `slack:interaction:<body hash>`,
   `discord:<interaction id>`. Slack's retries (`X-Slack-Retry-Num`) and any redelivery are the same
   key, recorded once.
3. **Acknowledge.** Slack within its three seconds; Discord with its interaction answer (a pong, an
   ephemeral line, a deferred update, or a modal).
4. **Handle.** A worker on each node claims due actions by compare-and-swap on `next_at` (as GitHub
   deliveries do). It retries with backoff (six attempts) and keeps handled ones three days. A node
   that dies holding one leaves it due when the claim lapses.

What an action becomes:

- **A trigger event** in the owner's automation inbox (`Code::automation_event`), keyed
  `slack:<team>:<id>`. This happens only if the user may drive roll and an enabled automation
  listens for that kind (`automation_listens`), so a busy channel never fills the inbox. Mentions
  and reactions get a 👀.
- **A steer.** A message in a chat thread bound to an agent thread (Slack: any reply in it;
  Discord: the status card's Reply button, which opens a modal) becomes `message.send` with
  `mode: steer`. Slack sends a mention in a thread twice (the mention and the message); both carry
  one command id, so the agent hears it once.
- **An answer.** A request card's button, select or modal becomes `request.respond` with the
  decision or answers.

Answers and steers act as the automation that made the binding, through roll's API (`Code::automation_api`: a
call token like steps' own, docs/code/automations.md "Steps act through the API"). So the agent
policy and cluster routing are the API's. An automation that runs as an agent can steer but can't
answer approvals, because `request.respond` is the owner's. Command ids come from the action's key,
so a retried action sends the same command once.

## Bindings and cards

- `chat_threads` binds a chat thread (Slack's `thread_ts`, or Discord's status message) to an
  agent thread (machine and thread) and to the automation (and `run_as`) it acts as.
- `chat_requests` ties each posted request card to the agent's request. A click is checked
  against its card, its card's binding, and the install it came from (a card is only answered
  from the workspace it was posted in). The click reads the request from the mirror: one that is
  already answered (anywhere) is shown as answered instead.
- The cards are platform-neutral (`code_chat::card`). `chatops/cards.rs` builds them from the
  mirror: the status shows the thread's title, its state (starting, running, waiting on you,
  done, failed, stopped), the newest run's steps (its tool calls), the activity line, links (the
  thread in the dashboard, the change it published), and why it failed. Approvals become buttons
  for exactly the request's decisions (Approve, Approve for session, Decline, Stop). Questions
  become selects (multi-selects when `multi`) or free-text inputs, plus Submit. Discord sends one
  component per interaction, so its picks are remembered until Submit.

## Triggers

| Source | Events | Data | Config |
| --- | --- | --- | --- |
| `slack` | `command` (`/roll …`), `mention`, `message` (watched channels), `reaction` | `team, channel, user, text, prompt, ts, thread_ts`; `reaction`, `author` | `team`, `channels` (needed for `message`), `reactions` (default `robot_face`) |
| `discord` | `command` (`/roll prompt:…`) | `guild, channel, user, user_name, prompt` | `guild`, `channels` |
| `github` | `issue.opened`, `issue.labeled`, `comment.mention`, `review.requested`, `check.failed` | `repo, number, title, body, labels, author, author_association, trusted_author, prompt, is_pr, change, reviewer, check, branch, on_default, sha, pr, url` | `repos`, `labels`, `branches`, `anyone` |

roll code's own events (changes, landings, checks, work items, broken main, which arrive as `work`
items) are the built-in `code` trigger's (docs/code/automations.md).

## Steps

| Step | What it does |
| --- | --- |
| `slack.post` / `slack.reply` / `slack.dm` | Post text or Block Kit (`blocks`, whose strings are templates) to a channel, the event's thread, or a person. Posted once per run and step (`chat_marks`). |
| `slack.update` | Edit a message an earlier step posted (`ts`). |
| `slack.status` | Follow an agent thread in Slack: the live status message, request cards, the binding. Waits (thread records wake the run) until the thread's work ends; 48 h timeout. Output `channel, ts, thread_ts, status, title, change, failed`. |
| `slack.notify` | A notice about an agent (by default the agent event's thread) to a channel or a DM: the status card, and cards with buttons for what it waits on. Bound, so a reply steers. |
| `discord.post`, `discord.status`, `discord.notify` | The same for Discord. Requests are replies to the status message, and the status has a Reply button. |
| `github.comment` | Comment on an issue or pull request (the event's by default). Made once per run and step. |
| `github.label` | Add or remove labels (one that isn't there is fine). |
| `github.status` | Set a commit status (`pending`, `success`, `failure`, `error`; context `roll/automation` by default). |
| `github.pr` | Open a pull request from a roll change, for example `steps.publish.change`. It pushes the change's head as its branch (or `roll/change-<n>`), opens the pull request into the default branch (or finds the open one), and links the change to it so its webhook finds the change. |

**Notifications** ("waiting on you", "failed", "done") are automations too: trigger
`agent {events: [thread.waiting, thread.failed, thread.done]}`, then `slack.notify {to: "U…"}`.
They're durable, visible in run history, and editable. The dashboard's templates make them in one
click (`apps/dashboard/src/lib/automation-templates.ts`).

## GitHub

- **Events.** Each part of a delivery that a connected repo handles (`github/inbox.rs`, durable
  until handled) is also recorded in the repo owner's automation inbox, keyed
  `github:<delivery id>:<repo id>`, before the part's own handling. GitHub's redeliveries and a
  part's retries record nothing twice.
- **Authority.** A comment speaks for roll only from someone GitHub says can push: `OWNER`,
  `MEMBER` or `COLLABORATOR` (`author_association`). Anyone else's `@roll` starts nothing. An
  issue opened by anyone else is recorded with `trusted_author: false`; the trigger takes it only
  with `anyone: true`. Labels need triage rights on GitHub, so a label event is the labeller's
  authority.
- **Code from outside the repo** never runs on its own. Comments and review requests on a fork's
  pull request start nothing until a writer approves its head (`approve-run`, as for checks and
  previews). A pull request roll doesn't know yet counts as a fork's. Failing checks start
  something only on the default branch or on the repo's own pull requests: GitHub lists none for a
  fork's.
- **Not answered twice.** On a pull request, a plain `@roll <question>` is the owner's
  automations' when one listens for `comment.mention`. The reviewer keeps its own commands
  (`review`, `fix`, `resolve`, …; docs/code/reviews.md).
- **Tenancy.** An installation spans tenants. Steps reach it only through one of the automation
  owner's own connected repos, and listing or connecting installations stays admin-only.
- The App needs the `check_run` and `status` events (besides the ones it had) for `check.failed`,
  and `Pull requests: write` and `Commit statuses: write` for the steps.

## Configuration

| Variable | |
| --- | --- |
| `ROLL_SLACK_CLIENT_ID`, `ROLL_SLACK_CLIENT_SECRET`, `ROLL_SLACK_SIGNING_SECRET` | The Slack app (all three, or none). `ROLL_SLACK_API_BASE` and `ROLL_SLACK_AUTHORIZE_URL` point at a test server. |
| `ROLL_DISCORD_APPLICATION_ID`, `ROLL_DISCORD_PUBLIC_KEY`, `ROLL_DISCORD_BOT_TOKEN`, `ROLL_DISCORD_CLIENT_SECRET` | The Discord application (all four, or none). `ROLL_DISCORD_API_BASE`, `ROLL_DISCORD_AUTHORIZE_URL`. |
| `ROLL_SECRET_KEY` | Seals Slack bot tokens. Slack installs need it. |
| `ROLL_CODE_PUBLIC_URL` | The API's public URL, from which the OAuth redirects are made (`…/v1/code/chat/{slack,discord}/oauth`). |
| `ROLL_GITHUB_*` | The GitHub App (docs/code/github-app.md). |

The Slack app's settings use these URLs: Event Subscriptions `…/v1/code/chat/slack/events` (bot
events `app_mention`, `message.channels`, `message.groups`, `message.im`, `reaction_added`,
`app_uninstalled`, `tokens_revoked`), the slash command `/roll` → `…/v1/code/chat/slack/commands`,
and Interactivity → `…/v1/code/chat/slack/interactions`. Discord's Interactions Endpoint URL is
`…/v1/code/chat/discord/interactions`.
