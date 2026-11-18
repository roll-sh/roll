import { createFileRoute } from "@tanstack/react-router";
import { Audit } from "@/components/access/audit";
import { pageHead } from "@/lib/head";

/** Every request a token made and every MCP tool call; `?grant=` narrows it to one token, `?product=` to one product (`accounts`: the sign-in report). */
export const Route = createFileRoute("/dashboard/access/audit")({
  head: () => pageHead({ title: "Audit log · MCP & access" }),
  component: function AuditRoute() {
    const { grant, product } = Route.useSearch() as { grant?: string; product?: string };
    return <Audit grant={typeof grant === "string" ? grant : undefined} product={typeof product === "string" ? product : undefined} />;
  },
});
