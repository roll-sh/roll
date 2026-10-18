import type { Metadata } from "next";
import { QueueList } from "@/components/queues/queue-list";

export const metadata: Metadata = { title: "Queue list · Queues" };

export default function QueuesListPage() {
  return <QueueList />;
}
