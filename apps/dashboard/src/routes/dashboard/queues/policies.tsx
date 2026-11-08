import { createFileRoute } from "@tanstack/react-router";
import { Policies } from "@/components/queues/policies";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/policies")({
  head: () => pageHead({ title: "Policies · Queues" }),
  component: Policies,
});
