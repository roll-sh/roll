import type { Metadata } from "next";
import { Vhosts } from "@/components/queues/vhosts";

export const metadata: Metadata = { title: "Vhosts · Queues" };

export default function QueuesVhostsPage() {
  return <Vhosts />;
}
