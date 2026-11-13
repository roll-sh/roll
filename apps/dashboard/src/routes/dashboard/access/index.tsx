import { createFileRoute } from "@tanstack/react-router";
import { AccessOverview } from "@/components/access/overview";
import { pageHead } from "@/lib/head";

/** MCP & access: connect a client, and every token and connection (docs/permissions.md). */
export const Route = createFileRoute("/dashboard/access/")({
  head: () => pageHead({ title: "MCP & access" }),
  component: AccessOverview,
});
