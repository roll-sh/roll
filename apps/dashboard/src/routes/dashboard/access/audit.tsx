import { createFileRoute } from "@tanstack/react-router";
import { Audit } from "@/components/access/audit";
import { pageHead } from "@/lib/head";

/** Every request a token made and every MCP tool call; `?grant=` narrows it to one token, `?product=` to one product (`accounts`: the sign-in report), `?resource=` to what it was about (a person, from People). */
export const Route = createFileRoute("/dashboard/access/audit")({
  head: () => pageHead({ title: "Audit log · MCP & access" }),
  component: function AuditRoute() {
    const { grant, product, resource } = Route.useSearch() as { grant?: string; product?: string; resource?: string };
    const s = (v: unknown) => (typeof v === "string" ? v : undefined);
    return <Audit grant={s(grant)} product={s(product)} resource={s(resource)} />;
  },
});
