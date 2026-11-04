import { createFileRoute } from "@tanstack/react-router";
import { Migrate } from "@/components/queues/migrate";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/migrate")({
  head: () => pageHead({ title: "Migrate · Queues" }),
  component: Migrate,
});
