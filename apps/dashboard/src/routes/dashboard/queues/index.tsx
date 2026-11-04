import { createFileRoute } from "@tanstack/react-router";
import { QueuesOverview } from "@/components/queues/overview";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/")({
  head: () => pageHead({ title: "Queues" }),
  component: QueuesOverview,
});
