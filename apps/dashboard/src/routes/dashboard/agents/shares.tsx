import { createFileRoute } from "@tanstack/react-router";
import { SharesList } from "@/components/agents/share";
import { CodeShell } from "@/components/code/shell";
import { pageHead } from "@/lib/head";

/** An owner's public share links, to copy or revoke. */
export const Route = createFileRoute("/dashboard/agents/shares")({
  head: () => pageHead({ title: "Shared threads · Agents" }),
  component: function SharesRoute() {
    const { owner } = Route.useSearch();
    return (
      <CodeShell crumb="Agents · shares">
        <SharesList owner={owner || undefined} />
      </CodeShell>
    );
  },
});
