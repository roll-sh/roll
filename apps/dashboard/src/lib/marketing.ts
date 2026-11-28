/**
 * The marketing site's catalogue: every product, what it's compatible with, what it does and the
 * terminal scenes that show it. Every claim and number here is from the product's own docs (the
 * file is named next to each measured number); measured numbers are local runs and say so. Keep it
 * that way: nothing goes here that the repo doesn't back.
 */
import {
  ArrowRightLeft,
  BadgeCheck,
  Blocks,
  Bot,
  Box,
  CalendarClock,
  CheckCheck,
  Cog,
  Combine,
  Copy,
  Database,
  Eye,
  EyeOff,
  Feather,
  Fingerprint,
  Flag,
  Gauge,
  GitBranch,
  GitCommitHorizontal,
  GitMerge,
  Globe,
  HardDrive,
  History,
  Import,
  Inbox,
  Infinity,
  KeyRound,
  Laptop,
  Layers,
  Link2,
  ListOrdered,
  MessageSquare,
  Moon,
  Network,
  Plug,
  Receipt,
  Rocket,
  Route,
  Rows3,
  Scale,
  Search,
  Send,
  ShieldAlert,
  ShieldCheck,
  Shuffle,
  Split,
  SquareTerminal,
  Undo2,
  UserCheck,
  Users,
  Waves,
  Waypoints,
  Workflow,
  Zap,
  type LucideIcon,
} from "lucide-react";

export const INSTALL = "curl -fsSL https://roll.sh/install.sh | sh";

export type GroupId = "data" | "messaging" | "build" | "platform";

export const GROUPS: { id: GroupId; name: string; lede: string }[] = [
  { id: "data", name: "Data", lede: "Databases and storage that branch and sleep." },
  { id: "messaging", name: "Messaging", lede: "Logs, queues and jobs, durable on object storage." },
  { id: "build", name: "Build & run", lede: "Source control, previews, hosting and agents." },
  { id: "platform", name: "Platform", lede: "Secrets, scoped access and models." },
];

export type Line = { cmd: string; out?: undefined } | { cmd?: undefined; out: string; tone?: "ok" | "accent" };

export interface Scene {
  label: string;
  lines: Line[];
  /** Played on the shared mark when the scene finishes. */
  mark?: "deploying" | "success";
}

/** A measured number, and what it was measured against where the same run measured that too. */
export interface Fact {
  value: string;
  label: string;
  /** The other side of the comparison, as its doc writes it, with both in one unit for the bars. */
  against?: { name: string; value: string; ours: number; theirs: number };
}

export interface Product {
  slug: string;
  name: string;
  group: GroupId;
  icon: LucideIcon;
  /** What it speaks, in a few words. */
  compat: string;
  /** What it is, for someone who's never heard of it. */
  what: string;
  /** What people use for this today. */
  insteadOf: string;
  /** How you reach it. */
  connect: string;
  /** The compatibility story in one line, over the code that connects. */
  headline: string;
  tagline: string;
  blurb: string;
  features: { icon: LucideIcon; title: string; body: string }[];
  scenes: Scene[];
  dashboard?: string;
  facts?: Fact[];
  /** Where the facts were measured, and how. */
  factsNote?: string;
  visual?: "branch" | "zero" | "queue" | "agents" | "stream";
  /** What isn't there yet, said plainly. */
  status?: string;
}

const cmd = (c: string): Line => ({ cmd: c });
const out = (o: string, tone?: "ok" | "accent"): Line => ({ out: o, tone });

export const PRODUCTS: Product[] = [
  {
    slug: "postgres",
    name: "Postgres",
    group: "data",
    icon: Database,
    compat: "Postgres 16, 17 and 18, unmodified",
    what: "A Postgres database for your app.",
    insteadOf: "Amazon RDS, Neon, Supabase's database",
    connect: "any Postgres driver: psql, pgx, Prisma",
    headline: "Your driver, your ORM, your extensions.",
    tagline: "Real Postgres that sleeps when nobody's querying.",
    blurb: "Every query, extension, driver and ORM works, because the engine is Postgres itself. The serverless layer around it is Rust.",
    features: [
      { icon: Moon, title: "Scale to zero, wake on connect", body: "An idle database suspends; the first connection wakes it. Compute autoscales live from 0.25 to 2 CU without a restart." },
      { icon: GitBranch, title: "Branches", body: "Branch a database for a feature, a test run or an agent, from now or from any point in its history." },
      { icon: History, title: "Point-in-time restore", body: "Restore to any timestamp or LSN, into a new branch or in place." },
      { icon: Infinity, title: "Bottomless", body: "WAL and base images live on object storage. Read replicas come from the same history." },
    ],
    scenes: [
      {
        label: "roll db",
        lines: [
          cmd("roll db create shop"),
          out("shop  pg 18  ready"),
          out(""),
          out("postgres://roll@localhost:55432/main?options=endpoint%3Dep-shop", "accent"),
          cmd("roll db branch shop dev --at 2026-11-28T09:00:00Z"),
          out("branch dev in 205 ms"),
          cmd("psql $(roll db url shop dev) -c 'select count(*) from orders'"),
          out(" count", "ok"),
          out("  4821", "ok"),
        ],
      },
    ],
    dashboard: "/dashboard/databases",
    facts: [
      { value: "0.66 s", label: "cold start to first query" },
      { value: "205 ms", label: "branch from a suspended parent" },
      { value: "10.3k", label: "pgbench -S TPS, autoscaling 0.25 → 1 CU" },
    ],
    factsNote: "Measured on an M-series Mac with Docker (README.md, Postgres).",
    visual: "branch",
    status: "Next: TLS and SNI routing, connection pooling. Branches are full copies today, not shared pages.",
  },
  {
    slug: "redis",
    name: "Redis",
    group: "data",
    icon: Zap,
    compat: "Redis 8 clients, RESP2 and RESP3",
    what: "A Redis cache and key-value store.",
    insteadOf: "ElastiCache, Upstash, Redis Cloud",
    connect: "any Redis client, or its REST API",
    headline: "The Redis client you already have.",
    tagline: "Redis, durable on object storage, at zero when idle.",
    blurb: "Tested byte for byte against Redis 8. Many tenants share a node, and an idle database costs only its storage.",
    features: [
      { icon: Plug, title: "Any client", body: "RESP2 and RESP3 over TCP or TLS with SNI, and an Upstash-compatible REST API for the edge." },
      { icon: HardDrive, title: "Durable", body: "Writes are kept on object storage, with an optional sync mode that acknowledges only once they're there." },
      { icon: GitBranch, title: "Branches from any second", body: "Branch a database as it was at any moment in its retention window, without copying it." },
      { icon: Copy, title: "Read replicas", body: "Add read replicas for read-heavy traffic." },
    ],
    scenes: [
      {
        label: "roll redis",
        lines: [
          cmd("roll redis create cache --memory 256"),
          out("cache  256 MB  ready"),
          cmd("redis-cli -u $(roll redis url cache) SET greeting hello"),
          out("OK", "ok"),
          cmd("roll redis branch cache dev --at 15m"),
          out("branch dev from 15 minutes ago"),
        ],
      },
    ],
    dashboard: "/dashboard/redis",
    visual: "branch",
  },
  {
    slug: "nosql",
    name: "NoSQL",
    group: "data",
    icon: Rows3,
    compat: "ScyllaDB 2026.3, Cassandra 5.0, DynamoDB",
    what: "A wide-column database for huge, fast tables.",
    insteadOf: "Cassandra, ScyllaDB, DynamoDB",
    connect: "cqlsh, any CQL driver, the DynamoDB SDKs",
    headline: "Your CQL driver, or the DynamoDB SDK.",
    tagline: "Cassandra and ScyllaDB behaviour, with instant branches.",
    blurb: "Pick a dialect per database: ScyllaDB or Cassandra over CQL v3 to v5, and the DynamoDB API through Alternator.",
    features: [
      { icon: BadgeCheck, title: "Conformant", body: "All 2,009 of ScyllaDB's cqlpy tests that pass on ScyllaDB pass on roll, and all 1,253 Alternator tests." },
      { icon: GitBranch, title: "Instant branches", body: "A branch is one manifest write. Reset it, restore it in place, or mask its data for a preview." },
      { icon: Import, title: "Live import", body: "Import from a running Cassandra or ScyllaDB cluster, or from SSTable backups." },
      { icon: Moon, title: "Scale to zero", body: "A database loads on its first request: 20 to 44 ms to the first row, against ScyllaDB's seconds to start." },
    ],
    scenes: [
      {
        label: "roll nosql",
        lines: [
          cmd("roll nosql create app --dialect scylla"),
          out("app  scylla  ready"),
          cmd("roll nosql branch app --name pr-482 --expires 72h"),
          out("branch pr-482, expires in 72h"),
          cmd("roll nosql cqlsh app"),
          out("Connected to app (scylla dialect)", "ok"),
        ],
      },
    ],
    dashboard: "/dashboard/nosql",
    facts: [
      { value: "84k", label: "lightweight transactions a second", against: { name: "ScyllaDB", value: "10k", ours: 84.5, theirs: 10.1 } },
      { value: "119k", label: "point reads a second", against: { name: "ScyllaDB", value: "122k", ours: 119.7, theirs: 121.9 } },
      { value: "20 ms", label: "cold start to the first row" },
    ],
    factsNote: "One machine, two CPUs for each, local storage (docs/benchmarks/nosql.md). Not a cluster claim.",
    visual: "branch",
  },
  {
    slug: "search",
    name: "Search",
    group: "data",
    icon: Search,
    compat: "Elasticsearch 9, OpenSearch 3.9",
    what: "Full-text, vector and log search.",
    insteadOf: "Elasticsearch, OpenSearch, Elastic Cloud",
    connect: "the official Elasticsearch clients",
    headline: "Change the URL. Keep the client.",
    tagline: "Elasticsearch's API without the always-on cluster.",
    blurb: "The Elasticsearch REST API, checked against 9.5.5, or an OpenSearch 3.9 dialect. Data lives on object storage; idle clusters sleep.",
    features: [
      { icon: Plug, title: "Your clients", body: "11 of 11 official Elasticsearch clients work. 3,810 of the 3,905 YAML REST tests pass." },
      { icon: Combine, title: "Semantic and hybrid", body: "semantic_text with a built-in E5 model, and rrf retrievers that mix it with lexical search." },
      { icon: GitBranch, title: "Branches", body: "Branch a cluster now or as it was, to try a mapping or a reindex without touching production." },
      { icon: Feather, title: "Tiny at rest", body: "23 MiB idle, where Elasticsearch takes 2.4 GiB. It starts in 20 ms." },
    ],
    scenes: [
      {
        label: "roll search",
        lines: [
          cmd("roll search clusters create app"),
          out("app  elasticsearch 9  ready"),
          cmd("roll search request app GET '/products/_search?q=title:shoes'"),
          out('{ "hits": { "total": { "value": 42 } } }', "ok"),
          cmd("roll search clusters branch app app-yesterday --at 1d"),
          out("branch app-yesterday from 1 day ago"),
        ],
      },
    ],
    dashboard: "/dashboard/search",
    facts: [
      { value: "19.8 ms", label: "cold start", against: { name: "Elasticsearch", value: "35.7 s", ours: 19.8, theirs: 35700 } },
      { value: "23 MiB", label: "memory when idle", against: { name: "Elasticsearch", value: "2,395 MiB", ours: 23, theirs: 2395 } },
      { value: "3,810", label: "of Elasticsearch's YAML REST tests passing", against: { name: "the whole suite", value: "3,905", ours: 3810, theirs: 3905 } },
    ],
    factsNote: "docs/benchmarks/search.md and docs/search/README.md.",
    status: "Semantic search on OpenSearch-dialect clusters is still in progress.",
    visual: "branch",
  },
  {
    slug: "storage",
    name: "Storage",
    group: "data",
    icon: HardDrive,
    compat: "S3 API: aws-cli, boto3, rclone, mc",
    what: "S3-compatible object storage for files.",
    insteadOf: "Amazon S3, Cloudflare R2, MinIO",
    connect: "aws s3, boto3, rclone, any S3 SDK",
    headline: "Every S3 tool, pointed at roll.",
    tagline: "S3 with branches and an undo button.",
    blurb: "The S3 API your tools already use, with copy-on-write bucket branches and a recovery window for every delete and overwrite.",
    features: [
      { icon: GitBranch, title: "Branching buckets", body: "Branch a bucket for a preview or a migration. Nothing is copied until it changes." },
      { icon: Undo2, title: "Seven days to undo", body: "Deleted and overwritten objects wait in the trash, recoverable, before anything is gone." },
      { icon: Fingerprint, title: "Checksummed end to end", body: "Every object is verified on the way in and out. CopyObject moves a reference, not the bytes." },
      { icon: Blocks, title: "The whole API", body: "Versioning, Object Lock, lifecycle, IAM and STS, notifications, replication, S3 Express directory buckets." },
    ],
    scenes: [
      {
        label: "roll storage",
        lines: [
          cmd("roll storage create assets"),
          out("assets  ready"),
          cmd("aws s3 cp ./logo.svg s3://assets/ --endpoint-url $ROLL_S3"),
          out("upload: ./logo.svg to s3://assets/logo.svg", "ok"),
          cmd("roll storage branch assets assets-pr-42"),
          out("branch assets-pr-42"),
        ],
      },
    ],
    dashboard: "/dashboard/storage",
    visual: "branch",
  },
  {
    slug: "streams",
    name: "Streams",
    group: "messaging",
    icon: Waves,
    compat: "Kafka protocol, Kafka 4.1 tools",
    what: "Event streams your services publish and read.",
    insteadOf: "Kafka, Confluent, Amazon MSK",
    connect: "any Kafka client, or plain HTTP",
    headline: "Kafka clients, or just curl.",
    tagline: "Kafka-compatible streams, bottomless on S3.",
    blurb: "Produce and fetch go to disk and back without decoding a record. Partitions tier to object storage, and an idle broker costs almost nothing.",
    features: [
      { icon: Plug, title: "Your Kafka clients", body: "Tested with Apache Kafka 4.1's tools, librdkafka and Kafka Streams." },
      { icon: CheckCheck, title: "Exactly once", body: "Idempotent producers, transactions and log compaction." },
      { icon: Globe, title: "HTTP too", body: "Produce and consume over HTTP, follow with Server-Sent Events, or use Confluent REST v3." },
      { icon: GitBranch, title: "Stream branches", body: "Branch a stream to replay production traffic into a test, sharing its data instead of copying it." },
    ],
    scenes: [
      {
        label: "roll stream",
        lines: [
          cmd("roll stream create shop"),
          out("shop  ready"),
          cmd("roll stream topic create shop orders -p 6"),
          out("orders  6 partitions"),
          cmd("roll stream consume shop orders --group billing --follow"),
          out('{"id":1042,"total":59.00}', "ok"),
          out('{"id":1043,"total":12.50}', "ok"),
        ],
      },
    ],
    dashboard: "/dashboard/streams",
    facts: [
      { value: "1.1M", label: "records/s produced at 200 B" },
      { value: "1–4 ms", label: "broker start" },
      { value: "30×", label: "less memory than Kafka, idle" },
    ],
    factsNote: "Local release build (README.md and crates/stream-broker/README.md).",
    status: "One broker serves every partition today; replication across brokers is next.",
    visual: "stream",
  },
  {
    slug: "queues",
    name: "Queues",
    group: "messaging",
    icon: Inbox,
    compat: "RabbitMQ 4: AMQP, MQTT, STOMP",
    what: "Message queues between your services.",
    insteadOf: "RabbitMQ, Amazon MQ, CloudAMQP, SQS",
    connect: "AMQP, MQTT and STOMP clients",
    headline: "Clients change only the URL.",
    tagline: "RabbitMQ's protocols, a vhost per app that sleeps.",
    blurb: "AMQP 0-9-1 and 1.0, MQTT 3.1.1 and 5, STOMP, the RabbitMQ Stream protocol and the management API, checked against RabbitMQ 4.3.",
    features: [
      { icon: Plug, title: "Every protocol", body: "Your AMQP, MQTT and STOMP clients connect unchanged, and the management API answers your tools." },
      { icon: Moon, title: "A vhost per app", body: "Each one sleeps when idle and wakes on connect. An idle queue costs 7 KiB, against RabbitMQ's 51 KiB." },
      { icon: Gauge, title: "Fast where it counts", body: "Quorum queues at 180k msg/s against RabbitMQ's 71k; fan-out at 510k against 91k." },
      { icon: ArrowRightLeft, title: "Move over", body: "Guides from RabbitMQ, Amazon MQ, CloudAMQP and SQS." },
    ],
    scenes: [
      {
        label: "roll queues",
        lines: [
          cmd("roll queues vhosts create shop --credentials"),
          out("shop  amqp://shop:•••@localhost:5672/shop", "accent"),
          cmd("roll queues declare orders -t quorum --durable"),
          out("orders  quorum  durable", "ok"),
        ],
      },
    ],
    dashboard: "/dashboard/queues",
    facts: [
      { value: "180k", label: "messages a second on a quorum queue", against: { name: "RabbitMQ 4", value: "71k", ours: 180.2, theirs: 71.3 } },
      { value: "510k", label: "messages a second, fanned out to ten queues", against: { name: "RabbitMQ 4", value: "91k", ours: 509.8, theirs: 90.9 } },
      { value: "7 KiB", label: "of memory for an idle queue", against: { name: "RabbitMQ 4", value: "51 KiB", ours: 7, theirs: 51 } },
    ],
    factsNote: "Against RabbitMQ 4 on one machine (docs/benchmarks/queues.md). roll's peak memory was higher in that run.",
  },
  {
    slug: "tasks",
    name: "Tasks",
    group: "messaging",
    icon: CalendarClock,
    compat: "HTTP messages, schedules, workflows",
    what: "Background jobs, cron and durable workflows.",
    insteadOf: "a job queue, cron boxes, a workflow engine",
    connect: "HTTP: roll calls your endpoint",
    headline: "Publish with one HTTP call.",
    tagline: "Messages, schedules and durable workflows over HTTP.",
    blurb: "Publish to a URL and roll delivers it, with retries, delays and a dead-letter queue. An alternative to Cloud Tasks, QStash and Inngest.",
    features: [
      { icon: Send, title: "Reliable delivery", body: "Retries, delays and a dead-letter queue, signed with Standard Webhooks." },
      { icon: ListOrdered, title: "Queues with rules", body: "FIFO queues and rate limits for the APIs on the other end." },
      { icon: CalendarClock, title: "Schedules", body: "Cron in any time zone." },
      { icon: Workflow, title: "Workflows", body: "Inngest-style steps that survive restarts, and point-in-time restore of a cell." },
    ],
    scenes: [
      {
        label: "roll tasks",
        lines: [
          cmd(`roll tasks publish https://example.com/hook -b '{"hi":1}' --delay 30s --retries 5`),
          out("msg_8f2c  delivers in 30s", "ok"),
          cmd("roll tasks schedules create '0 9 * * MON-FRI' --url https://example.com/report --tz Europe/Budapest"),
          out("sched_41a0  next: Mon 09:00 Europe/Budapest"),
        ],
      },
    ],
    dashboard: "/dashboard/tasks",
    status: "Delivery is at least once.",
  },
  {
    slug: "apps",
    name: "Apps",
    group: "build",
    icon: Rocket,
    compat: "A repo, a Dockerfile or an image",
    what: "Hosting for your own code: web services, workers, cron.",
    insteadOf: "Heroku, Railway, Fly.io",
    connect: "git push, a Dockerfile, or a folder",
    headline: "From a folder to a URL.",
    tagline: "Your code, hosted, at zero until someone visits.",
    blurb: "Deploy from a repo, a Dockerfile, an image or a detected build. Services sleep when idle and wake on the next request.",
    features: [
      { icon: Moon, title: "Wake on request", body: "A suspended service resumes in 13 ms; a cold Node service answers in 360 ms." },
      { icon: Layers, title: "Immutable deploys", body: "Every deploy is kept. A rollback runs an old image again, instantly, without a rebuild." },
      { icon: Split, title: "Gradual rollouts", body: "Move traffic to a new deployment a step at a time." },
      { icon: Cog, title: "Workers, cron and domains", body: "Background workers, scheduled jobs and your own domains, beside your web service." },
    ],
    scenes: [
      {
        label: "roll up",
        mark: "deploying",
        lines: [
          cmd("roll up"),
          out("detected: node (package.json)"),
          out("building… done in 11.8 s"),
          out("deployed dep_7c1e  https://web-shop.up.roll.site", "accent"),
          cmd("curl -s https://web-shop.up.roll.site/health"),
          out("ok", "ok"),
        ],
      },
    ],
    dashboard: "/dashboard",
    facts: [
      { value: "13 ms", label: "resume from suspended" },
      { value: "360 ms", label: "cold start, Node" },
      { value: "~108 µs", label: "added by the waking proxy" },
    ],
    factsNote: "Measured on a shared host (docs/apps/README.md, Measured).",
    visual: "zero",
    status: "The AWS runtime and builder are tested against fakes so far.",
  },
  {
    slug: "previews",
    name: "Previews",
    group: "build",
    icon: Eye,
    compat: "Every change, the whole project",
    what: "A full copy of the project for every pull request.",
    insteadOf: "a shared staging environment",
    connect: "a pull request; roll does the rest",
    headline: "One command, or every pull request.",
    tagline: "A copy of the whole project for every change.",
    blurb: "Services plus branches of Postgres, Redis, buckets, NoSQL, streams, queues, tasks and search, for each change, sleeping when nobody looks.",
    features: [
      { icon: Layers, title: "The whole stack", body: "Not just the app: every database and bucket the project uses is branched for the preview." },
      { icon: EyeOff, title: "Safe data", body: "Branch schema only, or mask the data on the way, so a preview never shows a real customer." },
      { icon: Moon, title: "It sleeps", body: "The whole preview sleeps, databases included, and wakes on the first request." },
      { icon: MessageSquare, title: "Where you review", body: "Statuses and comments on GitHub pull requests, and notes that turn into work items." },
    ],
    scenes: [
      {
        label: "roll preview",
        mark: "success",
        lines: [
          cmd("roll preview up"),
          out("branching postgres, redis, assets…"),
          out("preview pv_3b9d ready in 4.6 s", "ok"),
          out("https://web-pr1240-shop.preview.roll.site", "accent"),
          cmd("roll preview connect pv_3b9d --kind postgres"),
          out("psql (18.0)  main=>"),
        ],
      },
    ],
    dashboard: "/dashboard",
    facts: [
      { value: "4.6 s", label: "change opened to preview ready, with Postgres" },
      { value: "1.2 s", label: "without a database" },
      { value: "13 ms", label: "a warm request" },
    ],
    factsNote: "docs/previews.md, Measured.",
    visual: "branch",
  },
  {
    slug: "code",
    name: "Code",
    group: "build",
    icon: GitMerge,
    compat: "git smart HTTP v2, GitHub mirror",
    what: "Git hosting with a merge queue, built for agents.",
    insteadOf: "a git host plus a separate merge queue",
    connect: "plain git, or roll's MCP tools",
    headline: "Plain git, or no checkout at all.",
    tagline: "Source control for a world of agents. Keep the code rolling.",
    blurb: "Merges happen on the server, changes land without a checkout, and a merge queue keeps main green while dozens of agents push at once.",
    features: [
      { icon: Zap, title: "Lands in O(change)", body: "Server-side three-way merges: about 1,300 to 1,700 merges a second, from 64 agents on a laptop." },
      { icon: GitMerge, title: "A merge queue", body: "Only tested trees reach main. A failing change drops out without blocking the next." },
      { icon: Users, title: "Reviews in one place", body: "People, review apps, GitHub reviewers and roll's own reviewer, on one record with one set of rules." },
      { icon: GitCommitHorizontal, title: "git, derived", body: "Clone and push with plain git. Mirror to GitHub, fast-forward, commit ids intact." },
    ],
    scenes: [
      {
        label: "roll code",
        mark: "success",
        lines: [
          cmd("roll code create acme/web"),
          out("https://roll.sh/acme/web"),
          cmd('roll code land acme/web --base main -m "Fix typo" README.md'),
          out("queued chg_9a41  position 1"),
          cmd("roll code queue acme/web"),
          out("chg_9a41  checks passed  landed on main", "ok"),
        ],
      },
    ],
    dashboard: "/dashboard/code",
    facts: [
      { value: "1,700", label: "merges per second, at best" },
      { value: "40,000", label: "merges in one run" },
      { value: "64", label: "agents pushing at once" },
    ],
    factsNote: "On a laptop (README.md, roll code).",
    visual: "queue",
  },
  {
    slug: "agents",
    name: "Agents",
    group: "build",
    icon: Bot,
    compat: "Claude Code, Codex and ACP agents",
    what: "Your coding agents, run and watched from one place.",
    insteadOf: "agents in a dozen terminal tabs",
    connect: "roll agent up on your machine",
    headline: "The agents you already run.",
    tagline: "Every agent you run, on one canvas.",
    blurb: "Run Claude Code, Codex and ACP agents on your own machines or hosted sandboxes, with your own logins, and watch and steer them all from roll.",
    features: [
      { icon: Laptop, title: "Your machines", body: "roll agent up turns a laptop or a build box into a machine roll can run agents on. Provider logins never leave it." },
      { icon: Waypoints, title: "Threads on a canvas", body: "Delegations, handoffs and forks as cards and edges; approvals wait on one click." },
      { icon: Flag, title: "Checkpoints", body: "Work is checkpointed in a private git index, so a rollback never touches your own index or working files unasked." },
      { icon: Box, title: "Sandboxes", body: "Hosted sandboxes from 3¢ per running hour, for 2 vCPU and 4 GB." },
    ],
    scenes: [
      {
        label: "roll agent",
        lines: [
          cmd("roll agent login"),
          out("signed in; this machine is laptop"),
          cmd("roll agent up"),
          out("providers: claude, codex", "ok"),
          out("connected to roll.sh: 2 machines online", "accent"),
        ],
      },
    ],
    dashboard: "/dashboard/agents",
    visual: "agents",
  },
  {
    slug: "secrets",
    name: "Secrets",
    group: "platform",
    icon: KeyRound,
    compat: "roll run, 6 SDKs, Terraform, Kubernetes",
    what: "Secrets and environment variables, per environment.",
    insteadOf: "Doppler, Infisical, Vault, .env files",
    connect: "roll run, SDKs, CI and Kubernetes",
    headline: "In the process, never in a file.",
    tagline: "Secrets that stay right when everything rotates.",
    blurb: "Versioned secrets and environment variables per project, linked to the roll resources they belong to, injected without touching a file.",
    features: [
      { icon: Link2, title: "Linked to resources", body: "A database's password is linked to the database, so a rotation updates every place that uses it." },
      { icon: History, title: "History", body: "Every value is versioned, with point-in-time rollback and an audit log in a hash chain." },
      { icon: SquareTerminal, title: "roll run", body: "Inject values into a process at start, and restart it when they change. Nothing is written to disk." },
      { icon: Blocks, title: "Everywhere", body: "Clients for TypeScript, Go, Python, Java, .NET and Ruby; a GitHub Action, Terraform, Kubernetes and Lambda." },
    ],
    scenes: [
      {
        label: "roll secrets",
        lines: [
          cmd("roll secrets set STRIPE_KEY=sk_live_••• LOG_LEVEL=debug"),
          out("set 2 secrets in development (v14)", "ok"),
          cmd("roll run -e production -- npm start"),
          out("injected 23 values from production"),
          out("> shop@1.0.0 start", "accent"),
        ],
      },
    ],
    dashboard: "/dashboard",
  },
  {
    slug: "access",
    name: "Access & MCP",
    group: "platform",
    icon: ShieldCheck,
    compat: "Scoped tokens, OAuth, one MCP server",
    what: "Scoped tokens and one MCP server for everything.",
    insteadOf: "a pile of all-powerful API keys",
    connect: "a token, or MCP at /v1/mcp",
    headline: "One token, exactly what it needs.",
    tagline: "One set of rules for people, CI and agents.",
    blurb: "Scoped tokens are checked in front of every route, so the API, the CLI and the MCP server allow exactly the same things.",
    features: [
      { icon: KeyRound, title: "Scoped tokens", body: "Rules of product, resource and action. A rule can allow, or ask a person first." },
      { icon: Network, title: "One MCP server", body: "About 500 tools across every product at /v1/mcp, each one a route, so they never drift from the API." },
      { icon: UserCheck, title: "Approvals and audit", body: "An agent asks, a person decides, and both land in the audit log." },
      { icon: ShieldAlert, title: "Leaks revoked", body: "A token that turns up in a commit or GitHub's secret scanning is revoked, not just reported." },
    ],
    scenes: [
      {
        label: "roll token",
        lines: [
          cmd("roll token create ci --preset read-only --expires 90d"),
          out("rlt_•••••••••••• (shown once)", "accent"),
          cmd("roll mcp config vscode"),
          out("wrote .vscode/mcp.json: roll at /v1/mcp", "ok"),
        ],
      },
    ],
    dashboard: "/dashboard/access",
    facts: [
      { value: "~500", label: "MCP tools, one per route" },
      { value: "6.5 ms", label: "to build the tool list, p50" },
      { value: "1", label: "evaluator for every request" },
    ],
    factsNote: "docs/mcp.md.",
  },
  {
    slug: "gateway",
    name: "Gateway",
    group: "platform",
    icon: Route,
    compat: "Messages, Responses, Chat Completions",
    what: "One endpoint in front of your model API keys.",
    insteadOf: "LiteLLM-style proxies, keys on every laptop",
    connect: "the Messages, Chat or Responses API",
    headline: "Point every agent at one URL.",
    tagline: "Your model keys, behind one gateway you run.",
    blurb: "Anthropic Messages, OpenAI Responses and Chat Completions in; Anthropic, OpenAI, Azure, Bedrock, Vertex, OpenRouter or any compatible URL out.",
    features: [
      { icon: Shuffle, title: "Failover", body: "Move to the next upstream when one fails, translating between APIs on the way." },
      { icon: KeyRound, title: "Keys per machine", body: "Each member's machine gets its own client key, revocable on its own." },
      { icon: Receipt, title: "Usage and cost", body: "Every request is recorded with its tokens and its cost." },
      { icon: Scale, title: "Honest by design", body: "It takes API keys only, never a consumer-subscription login, and never presents itself as another client." },
    ],
    scenes: [
      {
        label: "roll gateway",
        lines: [
          cmd("roll gateway serve --config gateway.toml"),
          out("serving  upstreams: anthropic, bedrock", "ok"),
          cmd("roll gateway client add laptop --config gateway.toml"),
          out("client laptop  key sealed to the machine", "accent"),
        ],
      },
    ],
    dashboard: "/dashboard/agents",
  },
];

export const productHref = (slug: string) => `/products/${slug}`;

export const productBySlug = (slug: string) => PRODUCTS.find((p) => p.slug === slug);

/** The home page's terminal: one scene from each product people try first. */
export const HOME_SCENES: Scene[] = ["postgres", "redis", "streams", "storage", "apps", "code"].map((slug) => {
  const p = productBySlug(slug)!;
  return { ...p.scenes[0], label: p.name };
});
