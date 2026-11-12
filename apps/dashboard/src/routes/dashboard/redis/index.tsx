import { createFileRoute } from "@tanstack/react-router";
import { RedisDatabases } from "@/components/redis/databases";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/redis/")({
  head: () => pageHead({ title: "Redis" }),
  component: RedisDatabases,
});
