import { createFileRoute } from "@tanstack/react-router";
import { QueueList } from "@/components/queues/queue-list";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/queues/")({
  head: () => pageHead({ title: "Queue list · Queues" }),
  component: QueueList,
});
