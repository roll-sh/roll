import { createFileRoute } from "@tanstack/react-router";
import { Connections } from "@/components/queues/connections";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/connections")({
  head: () => pageHead({ title: "Connections · Queues" }),
  component: Connections,
});
