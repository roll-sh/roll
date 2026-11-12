import { createFileRoute } from "@tanstack/react-router";
import { Consent } from "@/components/access/consent";
import { pageHead } from "@/lib/head";

/** roll's OAuth consent screen for MCP clients (docs/mcp.md, "Authorization"). */
export const Route = createFileRoute("/oauth/authorize")({
  head: () => pageHead({ title: "Connect a client" }),
  component: function AuthorizeRoute() {
    const search = Route.useSearch() as Record<string, unknown>;
    return <Consent search={search} />;
  },
});
