# roll gateway

roll's model gateway runs on one of the owner's computers. The owner's other machines and sandboxes
send their coding agents' model requests to it, and it sends them on to the owner's own upstreams.
Code: `crates/model-gateway` (the gateway), `agent-host/src/gateway.rs` (the daemon that runs it),
`code-control/src/agents/gateway.rs` (the owner's plan on the relay), and
`apps/dashboard/src/components/agents/gateway/` (the UI). Wire: `agent-proto/src/gateway.rs`.

It does what CLIProxyAPI does for API keys and compatible upstreams, in Rust and inside roll:

- **Inbound APIs.** It speaks the three APIs coding agents use: Anthropic Messages (Claude Code),
  OpenAI Responses (Codex) and OpenAI Chat Completions (most ACP agents).
- **Upstreams.** It sends requests to Anthropic, OpenAI, Azure OpenAI, Amazon Bedrock, Google Vertex
  AI, OpenRouter, or any Anthropic- or OpenAI-compatible base URL.
- **Translation.** Where an upstream speaks another API, it translates the request, the response and
  the stream.
- **Routing.**
  - Aliases and per-model routes.
  - Priority order, and failover between upstreams on errors and outages.
  - Circuit breaking and session affinity.
- **Client keys.** Each machine gets its own key, which can be revoked.
- **Records.**
  - Usage and cost history by machine, project, thread, model and upstream.
  - A request log.

## Limits

These are the user's and roll's rules, not settings:

- **No consumer subscriptions.** The gateway never stores, proxies or rotates consumer-subscription
  logins (Claude.ai or ChatGPT sign-ins). Upstreams take API keys and cloud credentials only.
  - Anthropic's Usage Policy prohibits services that "route requests through consumer
    subscriptions".
  - Claude Code's legal page says developers "may not collect, store, or intermediate Claude.ai
    credentials".
  - A machine whose agents should use a subscription logs that agent in on that machine itself
    ("Logging providers in", agents.md).
- **No disguised clients.** Requests go out as `roll-gateway/<version>`, followed by the client's
  own user agent. The gateway never presents a request as Claude Code or Codex, the "cloaking"
  CLIProxyAPI does by default.
- **No working around rate limits.** A 429 cools that upstream down for its `retry-after`, and only
  an upstream of another provider may take over. The gateway never tries another key of the same
  provider to get around that provider's limits (OpenAI's Terms: no circumventing rate limits).
  Failover between providers the owner pays for, on errors and outages, is what LiteLLM and
  OpenRouter do.
- **No management API.** The gateway has no management HTTP API. It is managed through daemon
  commands over the relay, and none of them returns a key, so no route exists to download one.

## Where it runs

The owner picks a computer, under "Set up the gateway on this computer". That computer's
`roll agent up` runs the gateway in-process as a supervised task, so there is nothing else to
install. A machine without the daemon can run it alone with `roll gateway serve --config
gateway.toml` ("Standalone").

- **Listening.**
  - It always listens on the loopback addresses.
  - It also listens on the addresses the owner names (`GatewaySettings.listen`, IP literals, never
    an unspecified address), all on one port (`port`, 8790).
  - When a bind fails, the daemon reports `GatewayRun::Failed` with the reason, keeps serving the
    addresses it did bind, and retries with backoff.
- **Live changes.** Changing upstreams, routes or keys applies from the next request. Listeners are
  rebound only when the addresses or the port change, so a change never drops a request in flight.
- **Storage.** `gateway.db`, mode 0600, in the daemon's data directory, holds:
  - client keys, as SHA-256 hashes;
  - hourly usage roll-ups, kept `retention_days` (400);
  - the request log, kept 7 days and at most 100,000 rows;
  - bodies, kept a day and only while `log_bodies` is on.

  Settings and upstream keys are in `agent.db`, next to the endpoints' keys.

## THE NOTICE

When a machine uses the gateway, every AI request from it goes out from the gateway's computer:
from its network and its IP address. The providers see that computer, not the machine the agent
runs on.

roll says so plainly, with the gateway computer's name, everywhere the gateway is chosen or used:

- in setup, which the owner acknowledges (`GatewayPlan.acknowledged_at`);
- in the Gateway section's list of machines;
- in a member's machine settings (its endpoint and its providers' rows);
- in the new-sandbox dialog when new sandboxes join;
- in a provider's status ("via roll gateway on <computer>").

The notice also says what happens when that computer is offline:

- Requests from the members fail.
- If the owner turned on fallback (`GatewayPlan.fallback`), a member's providers start on their own
  logins or keys instead.

The dashboard's `GatewayNotice` is the one component that says it.

## Machines that use it

A member machine has an ordinary endpoint (agents.md, "Endpoints"):

| Field | Value |
|---|---|
| `id` | `roll-gateway` |
| `kind` | `gateway` |
| `name` | "roll gateway on <computer>" |
| URL | the URL it reaches the gateway by |
| key | a client key of its own |

- **Variables.**
  - Claude Code gets `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`.
  - Codex gets a roll model provider at `<base>/v1` (Responses).
  - Other agents get `OPENAI_BASE_URL` (`<base>/v1`) and `OPENAI_API_KEY`, plus the Anthropic
    variables.
- **Joining** (`POST /v1/code/agents/{owner}/gateway/members`):
  1. The relay asks the gateway's daemon for a client key, with `gateway.client.issue` and the
     member's `seal_key`.
  2. The gateway mints the key (`rgk_…`), keeps only its hash, and answers the key sealed to the
     member with the endpoint's associated data (`roll-endpoint:roll-gateway`).
  3. The relay passes that box on unchanged in `endpoint.set`.

  The relay and the browser never see a client key. Revoking it (leaving, or "Revoke" in the
  gateway's clients) refuses the member's next request.
- **Attribution.** For usage by thread and project, the member's daemon adds `x-roll-thread` and
  `x-roll-project` headers (ids only):
  - Claude Code through `ANTHROPIC_CUSTOM_HEADERS`;
  - Codex through its model provider's `env_http_headers` (a fixed `-c` override naming
    `ROLL_GATEWAY_THREAD` and `ROLL_GATEWAY_PROJECT`, which each session's process gets).

  Only where the process is the thread's own. The gateway reads them and never forwards them.
- **Fallback.** With `fallback` on, the member's daemon checks `<base>/health` (2 s, cached 30 s)
  before starting a provider's process. If the gateway doesn't answer, the provider starts without
  the endpoint, on its own login or keys, and its status says so. A running session keeps what it
  started with.
- **New sandboxes.** With `new_sandboxes` on, a sandbox joins when its daemon first says hello. The
  relay spawns the join on the node holding the owner's agents lease. A sandbox roll hosts elsewhere
  (a backend other than `docker`) has no route to the gateway ("Reachability"): the relay never
  joins it, and a join asked for by hand is refused (409) with why.

## Reachability

Machines connect out to roll, and roll never opens a port or tunnel on them (agents.md). Members
reach the gateway over a network the owner already has:

1. **The same computer**: a loopback URL (`http://` + loopback + `:8790`).
2. **The owner's network**: a LAN or tailnet address of the gateway's computer. The owner adds it
   as a listen address and as a labelled URL in the plan ("Tailscale", "Office LAN").
3. **Local Docker sandboxes** (the `docker` backend on the gateway's computer): the
   `roll-sandboxes` bridge's gateway address, as a listen address and a URL ("Docker sandboxes").
   Sandboxes can't reach each other (`enable_icc=false`), but they reach the host's bridge address
   unless the operator's firewall drops it (agents.md, "Sandboxes"). In that case, allow the
   gateway's port first.

**Hosted sandboxes** (agent37) have no route to an owner's computer, and roll doesn't open one. This
is a known gap. Its design, should it be built, is a **relay tunnel**:

- The member's daemon would listen on loopback.
- Each request would travel over the member's relay link, through the relay, and over the gateway
  daemon's link to the gateway in-process, with the stream coming back the same way.
- Frames would be bounded and flow-controlled, a member could open only a few streams at once, and
  the relay would hold no body.

The gap is deliberate for now:

- The tunnel puts every token of every request through roll's relay.
- It adds a data plane that agents.md avoids.
- agent37's own networking is unknown.

Until then, a hosted sandbox uses an endpoint whose gateway is reachable from the internet (one
the owner runs behind their own TLS and authentication), or its own keys or login.

## Routing

For a request asking for model `m`:

1. **Candidates.**
   - **Route**: the most specific route for `m` (exact, then the longest prefix ending in `*`) gives
     its targets in order, each an upstream and the model name to ask it for. An alias is a route
     that renames the model.
   - **No route**: every enabled upstream that serves `m` (its `models` patterns, or what its
     `/models` listed), in the settings' order (priority).
2. **Health.**
   - An open circuit skips the upstream until it half-opens: 5 failures in a row open it for 30 s,
     doubling up to 5 minutes; one request then tries it, and a success closes it.
   - A cooling upstream is skipped until its 429's `retry-after` (at most 10 minutes).
3. **Affinity** (`affinity`, on by default). A session goes back to the upstream it started on
   while that one is healthy, so prompt caches keep hitting.
   - Session ids come from Claude Code's and Codex's session headers, then `prompt_cache_key` or
     `metadata.user_id`.
   - At most 10,000 sessions are remembered, each for an hour.
4. **Failover.**
   - A connection error, a timeout, a 5xx, Anthropic's 529, a 401 or 403 (the owner's key is
     wrong) or an error event before a stream's first event moves to the next candidate, but only
     before the first byte reaches the client. A 404 moves on too, without counting against the
     circuit.
   - A 429 moves only to an upstream of another provider, and while it cools down every upstream
     of its provider is skipped. The named kinds are one provider each, whatever their host or
     key (two Anthropic keys, two Azure resources, two Bedrock regions); a compatible upstream is
     the provider at its host; two upstreams at one host are one provider.
   - Every upstream tried is in the log entry's `attempts`.

**Which API goes upstream:**

- the client's own API when the upstream speaks it (passed through: only the model name changes,
  and streams are byte-exact);
- else Messages for the Anthropic family;
- else Responses for an OpenAI-family upstream when the client sent Responses;
- else Chat.

## Upstreams

| Kind | URL | Auth | Speaks |
|---|---|---|---|
| `anthropic` | `https://api.anthropic.com` | `x-api-key` | Messages |
| `openai` | `https://api.openai.com/v1` | bearer | Chat, Responses |
| `azure_openai` | `<base>/openai/v1` | `api-key` | Chat, Responses |
| `bedrock` | `bedrock-runtime.<region>.amazonaws.com` | a Bedrock API key, or SigV4 from `ACCESS_KEY_ID:SECRET_ACCESS_KEY` | Messages (InvokeModel; its event stream decoded) |
| `vertex` | `<region>-aiplatform.googleapis.com` in `project` | a service account's JSON (a token from its JWT, cached) | Messages for Claude (`rawPredict`), Chat for other models |
| `openrouter` | `https://openrouter.ai/api/v1` | bearer | Chat |
| `anthropic_compatible` | `<base>/v1/messages` | `x-api-key` and bearer | Messages |
| `openai_compatible` | `<base>/chat/completions` (and `/responses` with `responses`) | bearer | Chat (Responses) |

- **Keys.** A key is sealed to the gateway's daemon in the browser (`roll-seal-v1`, associated data
  `roll-gateway-upstream:<id>`), or read from 1Password on that computer (`key_ref`).
- **Private addresses.** Private, loopback and tailnet addresses are refused, whether written out or
  resolved, unless the upstream allows them (`allow_private`, for a model server on the owner's
  network). The client follows no redirects.
- **Testing** (`gateway.test`). Asks each upstream for its model list (Bedrock: its foundation
  models; Vertex: a token). It never generates.

## Standalone

A computer without `roll agent up` runs the same gateway from a TOML file:

```toml
# gateway.toml
port = 8790
listen = ["100.101.102.103"]   # besides loopback: this computer's tailnet or LAN address
# data_dir = "/var/lib/roll-gateway"   # where gateway.db lives (default: this file's directory)

[[upstreams]]
id = "anthropic"
name = "Anthropic"
kind = "anthropic"

[[upstreams]]
id = "openai"
name = "OpenAI"
kind = "openai"

[[routes]]
model = "fast"
targets = [{ upstream = "anthropic", model = "claude-haiku-4-5" }]

[keys]                          # never a key itself: an environment variable or a file
anthropic = "env:ANTHROPIC_API_KEY"
openai = "file:/etc/roll/openai.key"
```

The file is `GatewaySettings` (above) in TOML, plus `[keys]` and `data_dir`; relative paths are
from the file's directory. Unlike the daemon's settings, `enabled` defaults to on.

1. `roll gateway serve --config gateway.toml` serves it until Ctrl-C, and applies the file again
   when it's saved (a file that doesn't parse or check is reported, and the settings in use stay).
   A key file is read when the config is: after rotating one, touch the config.
2. `roll gateway client add <name> --config gateway.toml` makes a client key and prints it once,
   with the variables a client uses. `client list` and `client revoke <id>` manage them, with or
   without a running `serve`; a revoked key is refused on its next request.
3. A client points at the gateway:
   - Claude Code and Anthropic SDKs: `ANTHROPIC_BASE_URL=http://<address>:8790` and
     `ANTHROPIC_AUTH_TOKEN=<key>`;
   - Codex and OpenAI SDKs: `OPENAI_BASE_URL=http://<address>:8790/v1` and `OPENAI_API_KEY=<key>`.

## Inbound

| Route | API |
|---|---|
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1/messages/count_tokens` | Anthropic token counting (passed to an Anthropic-family upstream, else estimated) |
| `POST /v1/chat/completions` | OpenAI Chat |
| `POST /v1/responses` | OpenAI Responses |
| `GET /v1/models` | every model the gateway offers, in OpenAI's shape with Anthropic's fields |
| `GET /health` | `{"ok": true}`, with no key |

- **Client keys.** A client key comes as `Authorization: Bearer`, `x-api-key` or `api-key`, and is
  looked up on every request.
- **Errors.** Errors come back in the client's API's shape.
- **Limits.**

  | What | Limit |
  |---|---|
  | Request body | 32 MiB |
  | Non-streamed upstream body | 32 MiB |
  | Streamed event | 8 MiB |
  | Gap between upstream chunks | 5 minutes |
  | Requests in flight | 256 |

## Usage and cost

Every finished request adds to an hourly roll-up for each combination of:

- client (machine), project and thread;
- the model asked for;
- the upstream and its model.

It also adds a log row. `gateway.usage` sums the roll-ups by model, machine, project, thread or
upstream, in hours or days.

Cost is an estimate from list prices (`model-gateway/src/price.rs`, dated) or the owner's
`prices`. It is in micro-dollars, and it is not a bill: the bill is each provider's own.

## Management

These are daemon commands on the gateway's machine, all owner-only. Agent tokens are refused
(`agents/policy.rs`).

| Command | Effect |
|---|---|
| `gateway.configure {settings}` | Replace the settings, apply them live, and send `hello` with `MachineConfig.gateway` |
| `gateway.key {upstream, key}` | Set or clear an upstream's sealed key |
| `gateway.client.issue {name, machine, seal_key}` | Mint a client key and answer it sealed to `seal_key` (`IssuedClient`) |
| `gateway.client.revoke {client}` | Revoke a client key |
| `gateway.test {upstream?}` | Check one upstream, or all of them |
| `gateway.remove {}` | Stop, and forget settings, keys, clients and history |
| `gateway.status {}` | Health of the gateway and its upstreams (query) |
| `gateway.usage {from, to, by, bucket}` | Usage (query) |
| `gateway.log {after?, before?, limit, errors}` | The request log (query) |
| `gateway.log.body {seq}` | A logged request's bodies (query) |

The owner's plan is on the relay (`GatewayPlan`, docs/code/http-api.md, "Gateway"):

| Route | Effect |
|---|---|
| `GET/PUT/DELETE /v1/code/agents/{owner}/gateway` | Read, set or remove the plan |
| `POST …/gateway/members` | Join a machine |
| `DELETE …/gateway/members/{machine}` | Leave |

Changing the host while machines use it is refused: remove them first.
