import { Bot, Database, GitMerge, KeyRound, LayoutGrid, Rocket, Search, Waves, type LucideIcon } from "lucide-react";
import agents from "@/assets/marketing/dash-agents.webp";
import canvas from "@/assets/marketing/dash-canvas.webp";
import code from "@/assets/marketing/dash-code.webp";
import postgres from "@/assets/marketing/dash-postgres.webp";
import search from "@/assets/marketing/dash-search.webp";
import secrets from "@/assets/marketing/dash-secrets.webp";
import service from "@/assets/marketing/dash-service.webp";
import streams from "@/assets/marketing/dash-streams.webp";

/** A rectangle of a screenshot, in the page's own pixels (of 1360×850): x, y, width, height. */
export type Focus = [number, number, number, number];

/** One stop of the tour on a page: what to look at, and what it shows. No `focus` is the whole page. */
export type Beat = { title: string; body: string; focus?: Focus };

export type ShotInfo = {
  src: string;
  width: number;
  height: number;
  alt: string;
  /** The tour's tab. */
  label: string;
  icon: LucideIcon;
  /** The tour's stops on this page: the whole page first, then the parts worth a closer look. */
  beats: Beat[];
};

export const SHOT_SIZE = { width: 1360, height: 850 };
const size = SHOT_SIZE;

/**
 * The real dashboard, rendered by scripts/marketing-shots at 1360×850 (3×, so the tour can
 * zoom in), in the tour's order.
 * A beat's `focus` is measured on that render: check it again after rendering a page anew.
 */
export const SHOTS = {
  canvas: {
    src: canvas,
    ...size,
    label: "Projects",
    icon: LayoutGrid,
    beats: [
      { title: "A project on one canvas", body: "Its services, Postgres, cache, bucket, search and streams, wired the way they talk to each other." },
      { title: "Every card is a real resource", body: "This one is Postgres 18 with three branches, its compute moving between 0.25 and 1 CU.", focus: [628, 170, 242, 134] },
      { title: "Idle means zero", body: "The search cluster has scaled to zero. It wakes on the next query.", focus: [978, 491, 242, 134] },
      { title: "What the month costs", body: "The bill so far and where it's heading, for each project.", focus: [6, 686, 214, 116] },
    ],
    alt: "A project on roll's dashboard: two services, a Postgres database, a Redis cache, a bucket, a search cluster and a stream on one canvas",
  },
  postgres: {
    src: postgres,
    ...size,
    label: "Postgres",
    icon: Database,
    beats: [
      { title: "Postgres, serverless", body: "Branches of one database, compute that goes to zero, and a SQL editor next to the connection string." },
      { title: "Branches with their own compute", body: "main, a dev branch and one for a pull request. The idle one sits at 0 CU until something connects.", focus: [250, 296, 639, 296] },
      { title: "Query it right here", body: "A SQL editor on the page, beside the connection string every Postgres driver takes.", focus: [250, 610, 639, 240] },
    ],
    alt: "roll's Postgres page: three branches of a database, compute usage, a SQL editor and a connection string",
  },
  streams: {
    src: streams,
    ...size,
    label: "Streams",
    icon: Waves,
    beats: [
      { title: "Kafka topics, live", body: "Topics with live records and partitions, reachable from any Kafka client or plain HTTP." },
      { title: "Two ways in", body: "A Kafka bootstrap address for the clients you have, and an HTTP endpoint for everything else.", focus: [906, 296, 420, 333] },
      { title: "Bottomless partitions", body: "Each partition keeps a little on local disk and the rest in object storage.", focus: [906, 646, 420, 204] },
    ],
    alt: "roll's Streams page: topics, live records, partitions and the Kafka bootstrap address",
  },
  service: {
    src: service,
    ...size,
    label: "Deploys",
    icon: Rocket,
    beats: [
      { title: "Deploys you can read", body: "Every deploy kept as an image, each step timed, and a service that sleeps when nobody's visiting." },
      { title: "One live, the rest kept", body: "Every deploy is kept. A rollback runs an old image again, without a rebuild.", focus: [250, 314, 442, 102] },
      { title: "Every build step, timed", body: "Queue, build and rollout with what each took, and the steps that came from cache.", focus: [707, 235, 619, 252] },
    ],
    alt: "A web service on roll: its deployments, a deploy's build steps and its build log",
  },
  code: {
    src: code,
    ...size,
    label: "Merge queue",
    icon: GitMerge,
    beats: [
      { title: "The merge queue", body: "Changes tested together on main's tip. Only trees that passed land, and nobody rebases by hand." },
      { title: "A batch in flight", body: "Two changes rebased on the default branch and tested once, at its tip.", focus: [250, 424, 560, 272] },
      { title: "The queue at a glance", body: "How many are waiting, how many landed in the last hour, and how long they waited.", focus: [250, 290, 810, 116] },
    ],
    alt: "roll code's merge queue for a repo: a batch of two changes being tested, one waiting, and recent landings",
  },
  agents: {
    src: agents,
    ...size,
    label: "Agents",
    icon: Bot,
    beats: [
      { title: "Every agent, one canvas", body: "Claude Code, Codex and other agents on your own machines, with the one waiting for an answer." },
      { title: "Machines, projects, threads", body: "A laptop, its checkouts and the threads running in them, joined by the edges between them.", focus: [121, 133, 437, 210] },
      { title: "Answer without leaving", body: "A thread that's waiting on you shows its question on the canvas, ready for one click.", focus: [366, 405, 192, 276] },
    ],
    alt: "roll's agents canvas: machines, projects and agent threads in every state, with a question waiting for an answer",
  },
  secrets: {
    src: secrets,
    ...size,
    label: "Secrets",
    icon: KeyRound,
    beats: [
      { title: "One table, every environment", body: "A project's keys across environments, linked to the resources they come from, with drift caught." },
      { title: "Linked, not pasted", body: "These values come from the project's own database, cache, stream, bucket and search, so a rotation reaches every place that uses them.", focus: [237, 153, 661, 190] },
      { title: "Drift, caught", body: "A key that's missing from an environment, or differs between them, says so in its row.", focus: [237, 575, 1103, 132] },
    ],
    alt: "roll secrets for a project: eighteen keys across development, staging and production, linked to the project's resources",
  },
  search: {
    src: search,
    ...size,
    label: "Search",
    icon: Search,
    beats: [
      { title: "Search that sleeps", body: "An Elasticsearch-compatible cluster with Discover, KQL and ES|QL, asleep until the next query." },
      { title: "KQL or ES|QL", body: "Query the way you already do, against an index or a data view.", focus: [505, 206, 575, 58] },
      { title: "A day of documents", body: "Discover's histogram over the last 24 hours, with the documents under it.", focus: [521, 269, 806, 153] },
    ],
    alt: "A search cluster on roll: Discover with a 24-hour histogram and the search log's documents",
  },
} satisfies Record<string, ShotInfo>;

export type ShotName = keyof typeof SHOTS;

/** The dashboard page that shows a product, where there is one of its own. */
const FOR: Partial<Record<string, ShotName>> = {
  postgres: "postgres",
  search: "search",
  streams: "streams",
  apps: "service",
  code: "code",
  agents: "agents",
  secrets: "secrets",
};

export const shotFor = (slug: string): ShotName | undefined => FOR[slug];
