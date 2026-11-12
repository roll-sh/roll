import { createFileRoute } from "@tanstack/react-router";
import { Users } from "@/components/queues/users";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/users")({
  head: () => pageHead({ title: "Users · Queues" }),
  component: Users,
});
