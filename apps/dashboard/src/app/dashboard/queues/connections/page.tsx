import type { Metadata } from "next";
import { Connections } from "@/components/queues/connections";

export const metadata: Metadata = { title: "Connections · Queues" };

export default function QueuesConnectionsPage() {
  return <Connections />;
}
