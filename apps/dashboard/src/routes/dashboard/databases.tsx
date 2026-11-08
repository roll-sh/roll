import { createFileRoute } from "@tanstack/react-router";
import { Databases } from "@/components/databases";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/databases")({
  head: () => pageHead({ title: "Databases" }),
  component: Databases,
});
