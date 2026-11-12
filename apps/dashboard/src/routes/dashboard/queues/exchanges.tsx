import { createFileRoute } from "@tanstack/react-router";
import { Exchanges } from "@/components/queues/exchanges";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/exchanges")({
  head: () => pageHead({ title: "Exchanges · Queues" }),
  component: Exchanges,
});
