import { createFileRoute } from "@tanstack/react-router";
import { Vhosts } from "@/components/queues/vhosts";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/vhosts")({
  head: () => pageHead({ title: "Vhosts · Queues" }),
  component: Vhosts,
});
