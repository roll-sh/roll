import { createFileRoute } from "@tanstack/react-router";
import { NosqlDatabases } from "@/components/nosql/databases";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/nosql/")({
  head: () => pageHead({ title: "NoSQL" }),
  component: NosqlDatabases,
});
