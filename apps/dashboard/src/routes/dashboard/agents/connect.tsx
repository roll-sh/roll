import { createFileRoute } from "@tanstack/react-router";
import { ConnectAgents } from "@/components/agents/connect";
import { CodeShell } from "@/components/code/shell";
import { pageHead } from "@/lib/head";

/** Approve a `roll agent login` (`?code=`), or add a machine. */
export const Route = createFileRoute("/dashboard/agents/connect")({
  head: () => pageHead({ title: "Connect a machine · Agents" }),
  component: function ConnectRoute() {
    const { code } = Route.useSearch();
    return (
      <CodeShell crumb="Agents · connect">
        <ConnectAgents code={code?.toUpperCase() || undefined} />
      </CodeShell>
    );
  },
});
