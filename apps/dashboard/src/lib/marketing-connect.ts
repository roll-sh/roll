/**
 * The code on each product's page: how you reach it with what you already run. Every command is
 * `roll-cli`'s own and every address, variable and file is the one the product's doc names (the
 * doc is beside each product). Client code is the client's own public API, and reads its address
 * from the environment, so nothing here invents a hostname. Keep it that way.
 */

export type Lang = "sh" | "ts" | "py" | "toml" | "yaml" | "json";

export interface Snippet {
  /** The tab. */
  label: string;
  lang: Lang;
  code: string;
}

export const SNIPPETS: Record<string, Snippet[]> = {
  // README.md, Postgres. `roll db url <project> [branch]`; `--at` is RFC 3339 or unix seconds.
  postgres: [
    {
      label: "psql",
      lang: "sh",
      code: `roll db create shop
psql "$(roll db url shop)"

# a branch of it, as it was this morning
roll db branch shop dev --at 2026-11-28T09:00:00Z
psql "$(roll db url shop dev)"`,
    },
    {
      label: "Node",
      lang: "ts",
      code: `import pg from "pg";

// DATABASE_URL="$(roll db url shop)"
const db = new pg.Client({ connectionString: process.env.DATABASE_URL });
await db.connect();

const { rows } = await db.query("select count(*) from orders");`,
    },
    {
      label: "Python",
      lang: "py",
      code: `import os, psycopg

# DATABASE_URL="$(roll db url shop)"
with psycopg.connect(os.environ["DATABASE_URL"]) as db:
    print(db.execute("select count(*) from orders").fetchone())`,
    },
  ],
  // README.md, Redis. `roll redis rest` prints UPSTASH_REDIS_REST_URL and _TOKEN.
  redis: [
    {
      label: "redis-cli",
      lang: "sh",
      code: `roll redis create cache --memory 256
redis-cli -u "$(roll redis url cache)" SET greeting hello

# as it was a quarter of an hour ago
roll redis branch cache dev --at 15m`,
    },
    {
      label: "Node",
      lang: "ts",
      code: `import { createClient } from "redis";

// REDIS_URL="$(roll redis url cache)"
const redis = createClient({ url: process.env.REDIS_URL });
await redis.connect();

await redis.set("session:9f2c", "…", { EX: 60 });`,
    },
    {
      label: "REST",
      lang: "ts",
      code: `// roll redis rest cache
//   UPSTASH_REDIS_REST_URL, UPSTASH_REDIS_REST_TOKEN
import { Redis } from "@upstash/redis";

const redis = Redis.fromEnv();
await redis.incr("visits");`,
    },
  ],
  // README.md, NoSQL. `roll nosql connect` prints the driver snippet with the host and credentials.
  nosql: [
    {
      label: "cqlsh",
      lang: "sh",
      code: `roll nosql create app --dialect scylla
roll nosql cqlsh app

# a copy for a pull request, gone in three days
roll nosql branch app --name pr-482 --expires 72h`,
    },
    {
      label: "Python",
      lang: "py",
      code: `# roll nosql connect app   prints this with your host and credentials
from cassandra.cluster import Cluster
from cassandra.auth import PlainTextAuthProvider

cluster = Cluster([HOST], port=PORT,
    auth_provider=PlainTextAuthProvider(USER, PASSWORD))
session = cluster.connect()`,
    },
    {
      label: "Import",
      lang: "sh",
      code: `# from a running Cassandra or ScyllaDB cluster, over CQL
roll nosql import app --from cql://reader:pw@prod:9042 --keyspaces shop

# what a branch changed in the schema, as CQL
roll nosql diff pr-482`,
    },
  ],
  // docs/search/guides/README.md.
  search: [
    {
      label: "curl",
      lang: "sh",
      code: `curl "$ELASTICSEARCH_URL/books/_search" \\
  -H "Authorization: ApiKey $ELASTICSEARCH_API_KEY" \\
  -H 'Content-Type: application/json' \\
  -d '{"query": {"match": {"title": "dune"}}}'`,
    },
    {
      label: "JavaScript",
      lang: "ts",
      code: `import { Client } from "@elastic/elasticsearch";

const client = new Client({
  node: process.env.ELASTICSEARCH_URL,
  auth: { apiKey: process.env.ELASTICSEARCH_API_KEY },
});

const result = await client.search({ index: "books", query: { match: { title: "dune" } } });`,
    },
    {
      label: "Python",
      lang: "py",
      code: `import os
from elasticsearch import Elasticsearch

es = Elasticsearch(os.environ["ELASTICSEARCH_URL"], api_key=os.environ["ELASTICSEARCH_API_KEY"])
result = es.search(index="books", query={"match": {"title": "dune"}})`,
    },
  ],
  // README.md, Storage. `roll storage keys create` prints AWS_ACCESS_KEY_ID, _SECRET and AWS_ENDPOINT_URL.
  storage: [
    {
      label: "aws",
      lang: "sh",
      code: `roll storage create assets
roll storage keys create laptop --permission write

aws s3 cp ./logo.png s3://assets/img/logo.png`,
    },
    {
      label: "boto3",
      lang: "py",
      code: `import boto3

# the key and AWS_ENDPOINT_URL come from the environment
s3 = boto3.client("s3")
s3.upload_file("logo.png", "assets", "img/logo.png")`,
    },
    {
      label: "Branch and undo",
      lang: "sh",
      code: `roll storage branch assets assets-pr-42   # instant, no data copied
roll storage trash assets                 # recently deleted, restorable`,
    },
  ],
  // README.md, Streams. `roll stream create` prints the bootstrap address, a key and client properties.
  streams: [
    {
      label: "roll",
      lang: "sh",
      code: `roll stream create shop
roll stream topic create shop orders -p 6

echo '{"id":1}' | roll stream produce shop orders -k user-1
roll stream consume shop orders --group billing --follow`,
    },
    {
      label: "Kafka tools",
      lang: "sh",
      code: `# client.properties is what \`roll stream create\` printed
kafka-console-consumer.sh --bootstrap-server "$BOOTSTRAP" \\
  --consumer.config client.properties \\
  --topic orders --group billing`,
    },
    {
      label: "HTTP",
      lang: "sh",
      code: `curl localhost:7070/v1/streams/shop/topics/orders/records \\
  -d '{"key":"user-1","value":{"id":2}}'

# follow the topic as Server-Sent Events
curl -N localhost:7070/v1/streams/shop/topics/orders/tail`,
    },
  ],
  // docs/queues/README.md. `roll queues url --vhost` prints the vhost's AMQP URL.
  queues: [
    {
      label: "roll",
      lang: "sh",
      code: `roll queues vhosts create shop --credentials
# amqp      amqp://shop:3f9c…@127.0.0.1:55672/shop
# mqtt      mqtt://shop%3Ashop:3f9c…@127.0.0.1:51883

roll queues declare orders -t quorum --durable`,
    },
    {
      label: "Node",
      lang: "ts",
      code: `import amqp from "amqplib";

// AMQP_URL="$(roll queues url --vhost shop)"
const conn = await amqp.connect(process.env.AMQP_URL);
const ch = await conn.createChannel();

ch.sendToQueue("orders", Buffer.from(JSON.stringify({ id: 1042 })));`,
    },
    {
      label: "Python",
      lang: "py",
      code: `import os, pika

conn = pika.BlockingConnection(pika.URLParameters(os.environ["AMQP_URL"]))
ch = conn.channel()

ch.basic_publish(exchange="", routing_key="orders", body=b'{"id":1042}')`,
    },
  ],
  // README.md, Tasks, and packages/tasks/README.md.
  tasks: [
    {
      label: "roll",
      lang: "sh",
      code: `roll tasks publish https://example.com/hook -b '{"hi":1}' --delay 30s --retries 5

roll tasks schedules create '0 9 * * MON-FRI' \\
  --url https://example.com/report --tz Europe/Budapest`,
    },
    {
      label: "TypeScript",
      lang: "ts",
      code: `import { Tasks } from "@roll/tasks";

const tasks = new Tasks(); // ROLL_TASKS_URL, ROLL_TASKS_TOKEN

await tasks.publish({
  url: "https://example.com/hooks/welcome",
  body: { userId: 42 },
  delay: "10m",
  retries: 5,
});`,
    },
    {
      label: "Workflow",
      lang: "ts",
      code: `import { createFunction, serve } from "@roll/tasks";

export const onboarding = createFunction(
  { id: "onboarding", trigger: { event: "user.created" }, retries: 3 },
  async ({ event, step }) => {
    const user = await step.run("load-user", () => db.users.find(event.data.id));
    await step.run("send-welcome", () => email.send(user.email, "Welcome!"));
    await step.sleep("wait-a-day", "1d");
  },
);

const handler = serve({ appId: "my-app", functions: [onboarding] });`,
    },
  ],
  // docs/apps/README.md: `roll up`, `roll logs`, `roll apps rollback`, roll.toml's `[app]`.
  apps: [
    {
      label: "roll up",
      lang: "sh",
      code: `cd my-app
roll up          # detects the build, deploys it, prints the URL
roll logs

# put an earlier deployment live again, without a rebuild
roll apps rollback dep_7c1d`,
    },
    {
      label: "roll.toml",
      lang: "toml",
      code: `# optional: the build is detected without it
[app]
build = "pnpm build"
start = "node dist/server.js"
health_path = "/health"`,
    },
  ],
  // docs/previews.md, and docs/code/README.md for roll.toml's `[preview]`.
  previews: [
    {
      label: "roll preview",
      lang: "sh",
      code: `roll preview up                           # a preview of this working tree
roll preview connect pv_3b9d --kind postgres
roll preview env pv_3b9d`,
    },
    {
      label: "roll.toml",
      lang: "toml",
      code: `[preview]
image = "node:22"
run = "pnpm i && pnpm dev --port $PORT"
port = 3000
postgres = "prj_…"     # the Postgres project to branch
redis = "rdb_…"        # the Redis database to branch`,
    },
  ],
  // README.md, Code.
  code: [
    {
      label: "git",
      lang: "sh",
      code: `git clone https://roll.sh/acme/web.git

git push origin main                    # pushing to main lands
git push origin HEAD:refs/land/main     # lands from a stale base
git push origin fix-typo                # any other branch opens a change`,
    },
    {
      label: "No checkout",
      lang: "sh",
      code: `roll code land acme/web --base main -m "Fix typo" README.md

roll code watch acme/web                # one line per landing`,
    },
    {
      label: "Agents",
      lang: "sh",
      code: `# claim the next work item; exits 3 when there's none
id=$(roll code work claim acme/web --agent a7)`,
    },
  ],
  // docs/code/agents.md.
  agents: [
    {
      label: "roll agent",
      lang: "sh",
      code: `roll agent login     # a device login; this machine gets a name
roll agent up        # offers the providers installed here to roll`,
    },
  ],
  // docs/secrets.md and docs/secrets/platforms.md.
  secrets: [
    {
      label: "roll run",
      lang: "sh",
      code: `roll secrets set STRIPE_KEY=sk_live_••• LOG_LEVEL=debug

roll run -- npm run dev
roll run -e production --watch -- ./server`,
    },
    {
      label: "TypeScript",
      lang: "ts",
      code: `import { createClient } from "@roll/secrets";

const secrets = createClient({ api: process.env.ROLL_API_URL!, token: process.env.ROLL_TOKEN! });

const { values } = await secrets.values("staging");
const stop = secrets.watch("staging", { onChange: (v) => reconfigure(v.values) });`,
    },
    {
      label: "GitHub Actions",
      lang: "yaml",
      code: `permissions:
  id-token: write
steps:
  - uses: roll-sh/roll/sdk/github-action@main
    with:
      api-url: https://roll.example.com
      identity: idn_…
      environment: staging
  - run: make test`,
    },
  ],
  // docs/permissions.md and docs/mcp.md.
  access: [
    {
      label: "Tokens",
      lang: "sh",
      code: `roll token create ci --preset read-only --expires 90d

roll token create migrate --product postgres --resource prj_ab… \\
  --allow read,write --ask delete`,
    },
    {
      label: "MCP",
      lang: "sh",
      code: `roll mcp config vscode

claude mcp add --transport http roll http://127.0.0.1:7070/v1/mcp`,
    },
    {
      label: "mcp.json",
      lang: "json",
      code: `{
  "mcpServers": {
    "roll": {
      "type": "http",
      "url": "http://127.0.0.1:7070/v1/mcp",
      "headers": { "Authorization": "Bearer rlt_…" }
    }
  }
}`,
    },
  ],
  // docs/code/gateway.md, "Standalone".
  gateway: [
    {
      label: "gateway.toml",
      lang: "toml",
      code: `port = 8790

[[upstreams]]
id = "anthropic"
name = "Anthropic"
kind = "anthropic"

[[upstreams]]
id = "openai"
name = "OpenAI"
kind = "openai"

[keys]              # never a key itself
anthropic = "env:ANTHROPIC_API_KEY"
openai = "file:/etc/roll/openai.key"`,
    },
    {
      label: "Serve",
      lang: "sh",
      code: `roll gateway serve --config gateway.toml
roll gateway client add laptop --config gateway.toml`,
    },
    {
      label: "Clients",
      lang: "sh",
      code: `# Claude Code and Anthropic SDKs
export ANTHROPIC_BASE_URL=http://<address>:8790
export ANTHROPIC_AUTH_TOKEN=<key>

# Codex and OpenAI SDKs
export OPENAI_BASE_URL=http://<address>:8790/v1
export OPENAI_API_KEY=<key>`,
    },
  ],
};

export const snippetsFor = (slug: string): Snippet[] => SNIPPETS[slug] ?? [];
