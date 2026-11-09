import { createFileRoute } from "@tanstack/react-router";
import { QueuesUsage } from "@/components/queues/usage";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/usage")({
  head: () => pageHead({ title: "Usage · Queues" }),
  component: QueuesUsage,
});
