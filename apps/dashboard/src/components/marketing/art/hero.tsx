import type { ReactNode } from "react";
import { AgentsHero, AppsHero, CodeHero, PreviewsHero } from "./hero-build";
import { NosqlHero, PostgresHero, RedisHero, SearchHero, StorageHero } from "./hero-data";
import { QueuesHero, StreamsHero, TasksHero } from "./hero-messaging";
import { AccessHero, GatewayHero, SecretsHero } from "./hero-platform";

/** Each product's picture on its own page. A product without one shows its card's drawing instead. */
export const HERO: Partial<Record<string, () => ReactNode>> = {
  postgres: PostgresHero,
  redis: RedisHero,
  nosql: NosqlHero,
  search: SearchHero,
  storage: StorageHero,
  streams: StreamsHero,
  queues: QueuesHero,
  tasks: TasksHero,
  apps: AppsHero,
  previews: PreviewsHero,
  code: CodeHero,
  agents: AgentsHero,
  secrets: SecretsHero,
  access: AccessHero,
  gateway: GatewayHero,
};
