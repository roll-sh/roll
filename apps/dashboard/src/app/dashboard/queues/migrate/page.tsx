import type { Metadata } from "next";
import { Migrate } from "@/components/queues/migrate";

export const metadata: Metadata = { title: "Migrate · Queues" };

export default function QueuesMigratePage() {
  return <Migrate />;
}
