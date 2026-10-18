import type { Metadata } from "next";
import { RedisDatabases } from "@/components/redis/databases";

export const metadata: Metadata = { title: "Redis" };

export default function RedisPage() {
  return <RedisDatabases />;
}
