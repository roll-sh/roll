import { createFileRoute } from "@tanstack/react-router";
import { Approvals } from "@/components/access/approvals";
import { pageHead } from "@/lib/head";

/** Requests waiting for your approval; `?id=` (the link a pending answer carries) comes first. */
export const Route = createFileRoute("/dashboard/access/approvals")({
  head: () => pageHead({ title: "Approvals · MCP & access" }),
  component: function ApprovalsRoute() {
    const { id } = Route.useSearch() as { id?: string };
    return <Approvals focus={typeof id === "string" ? id : undefined} />;
  },
});
