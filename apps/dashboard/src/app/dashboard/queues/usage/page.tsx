import type { Metadata } from "next";
import { QueuesUsage } from "@/components/queues/usage";

export const metadata: Metadata = { title: "Usage · Queues" };

export default function QueuesUsagePage() {
  return <QueuesUsage />;
}
